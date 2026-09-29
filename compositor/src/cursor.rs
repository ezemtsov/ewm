use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, anyhow};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Physical, Point, Transform};
use smithay::wayland::compositor::{SurfaceAttributes, with_states};
use tracing::warn;
use xcursor::CursorTheme;
use xcursor::parser::{Image, parse_xcursor};

/// Some default looking `left_ptr` icon.
static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../resources/cursor.rgba");

type XCursorCache = HashMap<(CursorIcon, i32), Option<Rc<XCursor>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorConfig {
    pub theme: String,
    pub size: u8,
}

impl CursorConfig {
    pub fn new(theme: impl Into<String>, size: u8) -> Self {
        Self {
            theme: theme.into(),
            size,
        }
    }

    pub fn resolve(theme: Option<String>, size: u8) -> Self {
        let theme = theme
            .or_else(|| Self::loadable_env_theme("XCURSOR_THEME"))
            .or_else(|| Self::loadable_theme("default"))
            .or_else(Self::first_loadable_theme)
            .unwrap_or_else(|| "default".to_owned());

        Self { theme, size }
    }

    pub fn env_vars(&self) -> HashMap<String, String> {
        HashMap::from([
            ("XCURSOR_THEME".to_string(), self.theme.clone()),
            ("XCURSOR_SIZE".to_string(), self.size.to_string()),
        ])
    }

    pub fn apply_env(&self) {
        // SAFETY: only the compositor thread writes these variables, at startup and on
        // cursor config reloads. Other Emacs threads that read the environment
        // concurrently are a pre-existing hazard of running in-process.
        unsafe {
            env::set_var("XCURSOR_THEME", &self.theme);
            env::set_var("XCURSOR_SIZE", self.size.to_string());
        }
    }

    fn loadable_env_theme(var: &str) -> Option<String> {
        env::var(var)
            .ok()
            .filter(|theme| !theme.is_empty())
            .and_then(|theme| Self::loadable_theme(&theme))
    }

    fn loadable_theme(theme: &str) -> Option<String> {
        Self::theme_has_default_cursor(theme).then(|| theme.to_owned())
    }

    fn theme_has_default_cursor(theme: &str) -> bool {
        let theme = CursorTheme::load(theme);
        theme.load_icon(CursorIcon::Default.name()).is_some()
            || CursorIcon::Default
                .alt_names()
                .iter()
                .any(|icon| theme.load_icon(icon).is_some())
    }

    fn first_loadable_theme() -> Option<String> {
        for root in cursor_theme_search_paths() {
            let Ok(entries) = fs::read_dir(root) else {
                continue;
            };

            let mut themes = entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name != "hicolor")
                .collect::<Vec<_>>();
            themes.sort();

            if let Some(theme) = themes
                .into_iter()
                .find(|theme| Self::theme_has_default_cursor(theme))
            {
                return Some(theme);
            }
        }

        None
    }
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self::new("default", 24)
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn cursor_theme_search_paths() -> Vec<PathBuf> {
    let Some(path) = non_empty_env("XCURSOR_PATH") else {
        return Vec::new();
    };
    let home = non_empty_env("HOME");
    let home_dir = home.as_deref().map(Path::new);

    path.split(':')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| expand_home_dir(PathBuf::from(entry), home_dir))
        .collect()
}

fn expand_home_dir(path: PathBuf, home_dir: Option<&Path>) -> Option<PathBuf> {
    let mut components = path.iter();
    if components.next()? != "~" {
        return Some(path);
    }

    let mut expanded = home_dir?.to_path_buf();
    for component in components {
        expanded.push(component);
    }
    Some(expanded)
}

pub struct CursorManager {
    theme: CursorTheme,
    theme_name: String,
    size: u8,
    current_cursor: CursorImageStatus,
    named_cursor_cache: RefCell<XCursorCache>,
}

impl CursorManager {
    pub fn new(theme: &str, size: u8) -> Self {
        Self::ensure_env(theme, size);

        let theme_name = theme.to_owned();
        let theme = CursorTheme::load(theme);

        Self {
            theme,
            theme_name,
            size,
            current_cursor: CursorImageStatus::default_named(),
            named_cursor_cache: Default::default(),
        }
    }

    /// Reload the cursor theme.
    pub fn reload(&mut self, theme: &str, size: u8) {
        Self::ensure_env(theme, size);
        self.theme = CursorTheme::load(theme);
        self.theme_name = theme.to_owned();
        self.size = size;
        self.named_cursor_cache.get_mut().clear();
    }

    /// Checks if the cursor WlSurface is alive, and if not, cleans it up.
    pub fn check_cursor_image_surface_alive(&mut self) {
        if let CursorImageStatus::Surface(surface) = &self.current_cursor
            && !surface.alive()
        {
            self.current_cursor = CursorImageStatus::default_named();
        }
    }

    /// Get the current rendering cursor.
    pub fn get_render_cursor(&self, scale: i32) -> RenderCursor {
        match self.current_cursor.clone() {
            CursorImageStatus::Hidden => RenderCursor::Hidden,
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });

                RenderCursor::Surface { hotspot, surface }
            }
            CursorImageStatus::Named(icon) => self.get_render_cursor_named(icon, scale),
        }
    }

    fn get_render_cursor_named(&self, icon: CursorIcon, scale: i32) -> RenderCursor {
        self.get_cursor_with_name(icon, scale)
            .map(|cursor| RenderCursor::Named {
                icon,
                scale,
                cursor,
            })
            .unwrap_or_else(|| RenderCursor::Named {
                icon: Default::default(),
                scale,
                cursor: self.get_default_cursor(scale),
            })
    }

    pub fn is_current_cursor_animated(&self, scale: i32) -> bool {
        match &self.current_cursor {
            CursorImageStatus::Hidden => false,
            CursorImageStatus::Surface(_) => false,
            CursorImageStatus::Named(icon) => self
                .get_cursor_with_name(*icon, scale)
                .unwrap_or_else(|| self.get_default_cursor(scale))
                .is_animated_cursor(),
        }
    }

    /// Get named cursor for the given `icon` and `scale`.
    pub fn get_cursor_with_name(&self, icon: CursorIcon, scale: i32) -> Option<Rc<XCursor>> {
        self.named_cursor_cache
            .borrow_mut()
            .entry((icon, scale))
            .or_insert_with_key(|(icon, scale)| {
                let size = self.size as i32 * scale;
                let mut cursor = Self::load_xcursor(&self.theme, icon.name(), size);

                // Check alternative names to account for non-compliant themes.
                if cursor.is_err() {
                    for name in icon.alt_names() {
                        cursor = Self::load_xcursor(&self.theme, name, size);
                        if cursor.is_ok() {
                            break;
                        }
                    }
                }

                if let Err(err) = &cursor {
                    warn!("error loading xcursor {}@{size}: {err:?}", icon.name());
                }

                // The default cursor must always have a fallback.
                if *icon == CursorIcon::Default && cursor.is_err() {
                    cursor = Ok(Self::fallback_cursor());
                }

                cursor.ok().map(Rc::new)
            })
            .clone()
    }

    /// Get default cursor.
    pub fn get_default_cursor(&self, scale: i32) -> Rc<XCursor> {
        // The default cursor always has a fallback.
        self.get_cursor_with_name(CursorIcon::Default, scale)
            .unwrap()
    }

    /// Currently used cursor_image as a cursor provider.
    pub fn cursor_image(&self) -> &CursorImageStatus {
        &self.current_cursor
    }

    /// Set new cursor image provider.
    pub fn set_cursor_image(&mut self, cursor: CursorImageStatus) {
        self.current_cursor = cursor;
    }

    pub fn debug_state(&self, scale: i32) -> serde_json::Value {
        match self.current_cursor.clone() {
            CursorImageStatus::Hidden => {
                serde_json::json!({ "provider": "hidden" })
            }
            CursorImageStatus::Surface(surface) => with_states(&surface, |states| {
                let hotspot = states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .hotspot;
                let mut attrs = states.cached_state.get::<SurfaceAttributes>();
                let attrs = attrs.current();
                let renderer_state = states
                    .data_map
                    .get::<RendererSurfaceStateUserData>()
                    .map(|state| state.lock().unwrap());
                let buffer_scale = renderer_state
                    .as_ref()
                    .map(|state| state.buffer_scale())
                    .unwrap_or(attrs.buffer_scale);
                let buffer_transform = renderer_state
                    .as_ref()
                    .map(|state| state.buffer_transform())
                    .unwrap_or_else(|| attrs.buffer_transform.into());
                let logical_size = renderer_state
                    .as_ref()
                    .and_then(|state| state.buffer_size());
                let buffer_size =
                    logical_size.map(|size| size.to_buffer(buffer_scale, buffer_transform));
                let surface_size = renderer_state
                    .as_ref()
                    .and_then(|state| state.surface_size());

                serde_json::json!({
                    "provider": "surface",
                    "hotspot": [hotspot.x, hotspot.y],
                    "buffer_scale": buffer_scale,
                    "buffer_transform": format!("{:?}", buffer_transform),
                    "buffer_size": buffer_size.map(|size| [size.w, size.h]),
                    "logical_size": logical_size.map(|size| [size.w, size.h]),
                    "surface_size": surface_size.map(|size| [size.w, size.h]),
                })
            }),
            CursorImageStatus::Named(requested_icon) => {
                let rendered = self.get_render_cursor_named(requested_icon, scale);
                let RenderCursor::Named { icon, cursor, .. } = rendered else {
                    unreachable!("named cursor renders as a named cursor");
                };
                let (idx, frame) = cursor.frame(0);
                let path = self.resolve_cursor_path(icon);

                serde_json::json!({
                    "provider": "named",
                    "theme": self.theme_name,
                    "configured_size": self.size,
                    "scale": scale,
                    "requested_size": self.size as i32 * scale,
                    "requested_icon": requested_icon.name(),
                    "rendered_icon": icon.name(),
                    "resolved_name": path.as_ref().map(|(name, _)| name.as_str()),
                    "resolved_path": path.as_ref().map(|(_, path)| path.display().to_string()),
                    "embedded_fallback": path.is_none(),
                    "frame_index": idx,
                    "frame_nominal_size": frame.size,
                    "frame_size": [frame.width, frame.height],
                    "hotspot": [frame.xhot, frame.yhot],
                })
            }
        }
    }

    /// Load the cursor with the given `name` from the file system picking the closest
    /// one to the given `size`.
    fn load_xcursor(theme: &CursorTheme, name: &str, size: i32) -> anyhow::Result<XCursor> {
        crate::tracy_span!("load_xcursor");

        let path = theme
            .load_icon(name)
            .ok_or_else(|| anyhow!("no default icon"))?;

        let mut file = File::open(path).context("error opening cursor icon file")?;
        let mut buf = vec![];
        file.read_to_end(&mut buf)
            .context("error reading cursor icon file")?;

        let mut images = parse_xcursor(&buf).context("error parsing cursor icon file")?;

        let (width, height) = images
            .iter()
            .min_by_key(|image| (size - image.size as i32).abs())
            .map(|image| (image.width, image.height))
            .unwrap();

        images.retain(move |image| image.width == width && image.height == height);

        let animation_duration = images.iter().fold(0, |acc, image| acc + image.delay);

        Ok(XCursor {
            images,
            animation_duration,
        })
    }

    fn resolve_cursor_path(&self, icon: CursorIcon) -> Option<(String, PathBuf)> {
        self.theme
            .load_icon(icon.name())
            .map(|path| (icon.name().to_owned(), path))
            .or_else(|| {
                icon.alt_names().iter().find_map(|name| {
                    self.theme
                        .load_icon(name)
                        .map(|path| ((*name).to_owned(), path))
                })
            })
    }

    /// Set the common XCURSOR env variables.
    fn ensure_env(theme: &str, size: u8) {
        // SAFETY: see `CursorConfig::apply_env`.
        unsafe {
            env::set_var("XCURSOR_THEME", theme);
            env::set_var("XCURSOR_SIZE", size.to_string());
        }
    }

    fn fallback_cursor() -> XCursor {
        let images = vec![Image {
            size: 32,
            width: 64,
            height: 64,
            xhot: 1,
            yhot: 1,
            delay: 0,
            pixels_rgba: Vec::from(FALLBACK_CURSOR_DATA),
            pixels_argb: vec![],
        }];

        XCursor {
            images,
            animation_duration: 0,
        }
    }
}

/// The cursor prepared for renderer.
pub enum RenderCursor {
    Hidden,
    Surface {
        hotspot: Point<i32, Logical>,
        surface: WlSurface,
    },
    Named {
        icon: CursorIcon,
        scale: i32,
        cursor: Rc<XCursor>,
    },
}

type TextureCache = HashMap<(CursorIcon, i32), Vec<MemoryRenderBuffer>>;

#[derive(Default)]
pub struct CursorTextureCache {
    cache: RefCell<TextureCache>,
}

impl CursorTextureCache {
    pub fn clear(&mut self) {
        self.cache.get_mut().clear();
    }

    pub fn get(
        &self,
        icon: CursorIcon,
        scale: i32,
        cursor: &XCursor,
        idx: usize,
    ) -> MemoryRenderBuffer {
        self.cache
            .borrow_mut()
            .entry((icon, scale))
            .or_insert_with(|| {
                cursor
                    .frames()
                    .iter()
                    .map(|frame| {
                        MemoryRenderBuffer::from_slice(
                            &frame.pixels_rgba,
                            Fourcc::Argb8888,
                            (frame.width as i32, frame.height as i32),
                            scale,
                            Transform::Normal,
                            None,
                        )
                    })
                    .collect()
            })[idx]
            .clone()
    }
}

// The XCursorBuffer implementation is inspired by `wayland-rs`, thus provided under MIT license.

/// The state of the `NamedCursor`.
pub struct XCursor {
    /// The image for the underlying named cursor.
    images: Vec<Image>,
    /// The total duration of the animation.
    animation_duration: u32,
}

impl XCursor {
    /// Given a time, calculate which frame to show, and how much time remains until the next frame.
    ///
    /// Time will wrap, so if for instance the cursor has an animation lasting 100ms,
    /// then calling this function with 5ms and 105ms as input gives the same output.
    pub fn frame(&self, mut millis: u32) -> (usize, &Image) {
        if self.animation_duration == 0 {
            return (0, &self.images[0]);
        }

        millis %= self.animation_duration;

        let mut res = 0;
        for (i, img) in self.images.iter().enumerate() {
            if millis < img.delay {
                res = i;
                break;
            }
            millis -= img.delay;
        }

        (res, &self.images[res])
    }

    /// Get the frames for the given `XCursor`.
    pub fn frames(&self) -> &[Image] {
        &self.images
    }

    /// Check whether the cursor is animated.
    pub fn is_animated_cursor(&self) -> bool {
        self.images.len() > 1
    }

    /// Get hotspot for the given `image`.
    pub fn hotspot(image: &Image) -> Point<i32, Physical> {
        (image.xhot as i32, image.yhot as i32).into()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// Serializes tests that mutate the process environment; the `unsafe` env calls in
    /// this module are only sound while this lock is held.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

    impl EnvRestore {
        fn capture(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| (*name, env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                if let Some(value) = value {
                    unsafe { env::set_var(name, value) };
                } else {
                    unsafe { env::remove_var(name) };
                }
            }
        }
    }

    struct TempThemeRoot(PathBuf);

    impl TempThemeRoot {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root =
                env::temp_dir().join(format!("ewm-cursor-theme-{}-{nanos}", std::process::id()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempThemeRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_cursor_theme(root: &Path, theme: &str, cursor_name: &str) {
        let cursor_dir = root.join(theme).join("cursors");
        fs::create_dir_all(&cursor_dir).unwrap();
        fs::write(cursor_dir.join(cursor_name), []).unwrap();
    }

    #[test]
    fn missing_default_cursor_uses_fallback() {
        let _guard = env_lock().lock().unwrap();
        let manager = CursorManager::new("ewm-missing-test-theme", 24);
        let RenderCursor::Named { cursor, .. } = manager.get_render_cursor(1) else {
            panic!("default cursor should render as a named cursor");
        };

        let frame = cursor.frames().first().unwrap();
        assert_eq!((frame.width, frame.height), (64, 64));
        assert_eq!(XCursor::hotspot(frame), Point::from((1, 1)));
    }

    #[test]
    fn auto_cursor_config_uses_loadable_env_theme() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvRestore::capture(&["XCURSOR_PATH", "XCURSOR_THEME"]);
        let root = TempThemeRoot::new();
        make_cursor_theme(root.path(), "EnvTheme", "default");
        make_cursor_theme(root.path(), "OtherTheme", "default");
        unsafe { env::set_var("XCURSOR_PATH", root.path()) };
        unsafe { env::set_var("XCURSOR_THEME", "EnvTheme") };

        let config = CursorConfig::resolve(None, 24);

        assert_eq!(config, CursorConfig::new("EnvTheme", 24));
    }

    #[test]
    fn auto_cursor_config_skips_unloadable_default() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvRestore::capture(&["XCURSOR_PATH", "XCURSOR_THEME"]);
        let root = TempThemeRoot::new();
        make_cursor_theme(root.path(), "LoadableTheme", "default");
        unsafe { env::set_var("XCURSOR_PATH", root.path()) };
        unsafe { env::set_var("XCURSOR_THEME", "default") };

        let config = CursorConfig::resolve(None, 24);

        assert_eq!(config, CursorConfig::new("LoadableTheme", 24));
    }

    #[test]
    fn explicit_cursor_config_does_not_probe_theme() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvRestore::capture(&["XCURSOR_PATH", "XCURSOR_THEME"]);
        let root = TempThemeRoot::new();
        make_cursor_theme(root.path(), "EnvTheme", "default");
        unsafe { env::set_var("XCURSOR_PATH", root.path()) };
        unsafe { env::set_var("XCURSOR_THEME", "EnvTheme") };

        let config = CursorConfig::resolve(Some("MissingTheme".to_owned()), 24);

        assert_eq!(config, CursorConfig::new("MissingTheme", 24));
    }

    #[test]
    fn cursor_config_exports_xcursor_environment() {
        let config = CursorConfig::new("Theme", 32);
        assert_eq!(
            config.env_vars(),
            HashMap::from([
                ("XCURSOR_THEME".to_string(), "Theme".to_string()),
                ("XCURSOR_SIZE".to_string(), "32".to_string()),
            ])
        );
    }

    #[test]
    fn explicit_cursor_config_overrides_ambient_environment() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvRestore::capture(&["XCURSOR_THEME", "XCURSOR_SIZE"]);
        unsafe { env::set_var("XCURSOR_THEME", "AmbientTheme") };
        unsafe { env::set_var("XCURSOR_SIZE", "12") };

        let manager = CursorManager::new("ConfiguredTheme", 37);

        assert_eq!(manager.theme_name, "ConfiguredTheme");
        assert_eq!(manager.size, 37);
        assert_eq!(env::var("XCURSOR_THEME").unwrap(), "ConfiguredTheme");
        assert_eq!(env::var("XCURSOR_SIZE").unwrap(), "37");
    }
}
