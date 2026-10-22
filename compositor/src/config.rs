//! The options Emacs sets, sent whole and applied where they changed.

use std::mem;
use std::time::Duration;

use elisp::Value;
use serde::Deserialize;

use crate::backend::drm::LidPolicyReason;
use crate::cursor::CursorConfig;
use crate::idle::IdleAction::{DeactivateMonitors, RunCommand};
use crate::input::InputConfigEntry;
use crate::layout::overview::{DEFAULT_BACKDROP_COLOR, DEFAULT_BACKGROUND_COLOR, DEFAULT_ZOOM};
use crate::render_helpers::shadow_style::Blur;
use crate::{Event, State};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    pub input: Vec<InputConfigEntry>,
    pub idle: Idle,
    pub cursor: Cursor,
    pub cursor_hide: CursorHide,
    pub focus_follows_mouse: bool,
    /// Alpha of unfocused toplevels, 1 leaves them opaque.
    pub unfocused_alpha: f32,
    pub blur: Blur,
    pub animations: bool,
    pub overview: Overview,
    /// Fill behind the frames, the default one when absent.
    pub background: Option<[f32; 3]>,
    pub power: Power,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            input: Vec::new(),
            idle: Idle::default(),
            cursor: Cursor::default(),
            cursor_hide: CursorHide::default(),
            focus_follows_mouse: false,
            unfocused_alpha: 1.,
            blur: Blur::default(),
            animations: true,
            overview: Overview::default(),
            background: None,
            power: Power::default(),
        }
    }
}

impl Config {
    /// The config a plist spells, a section that does not parse reported by key and defaulted.
    pub fn from_sections(plist: &Value) -> (Self, Vec<(String, String)>) {
        let items = match plist {
            Value::List(items) => items.as_slice(),
            _ => &[],
        };
        let (mut kept, mut rejected) = (Vec::new(), Vec::new());
        for pair in items.chunks(2) {
            match elisp::serde::from_value::<Self>(&Value::List(pair.to_vec())) {
                Ok(_) => kept.extend_from_slice(pair),
                Err(err) => rejected.push((pair[0].to_string(), err.to_string())),
            }
        }
        let config = elisp::serde::from_value(&Value::List(kept));
        (config.unwrap_or_default(), rejected)
    }

    /// Pull the renderer inputs into the ranges it accepts.
    fn clamp(&mut self) {
        self.unfocused_alpha = self.unfocused_alpha.clamp(0., 1.);
        let blur = &mut self.blur;
        blur.passes = blur.passes.clamp(1, 31);
        blur.offset = blur.offset.clamp(0., 100.);
        blur.noise = blur.noise.clamp(0., 1000.);
        blur.saturation = blur.saturation.clamp(0., 1000.);
    }
}

/// Seconds of inactivity before the command runs, or the monitors turn off without one.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Idle {
    pub timeout: Option<u64>,
    pub command: Option<String>,
}

/// Xcursor theme and size, a loadable theme from the environment and 24 when absent.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Cursor {
    pub theme: Option<String>,
    pub size: Option<u8>,
}

impl Cursor {
    pub fn resolve(&self) -> CursorConfig {
        CursorConfig::resolve(self.theme.clone(), self.size.unwrap_or(24))
    }
}

/// Seconds of pointer inactivity before the cursor hides, and whether typing hides it.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct CursorHide {
    pub timeout: Option<u64>,
    pub when_typing: bool,
}

/// Zoom of the strip and the colour behind it, the defaults when absent.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Overview {
    pub zoom: Option<f64>,
    pub backdrop_color: Option<[f32; 3]>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Power {
    pub closed_lid_suspends: bool,
    pub sepia: bool,
}

impl Default for Power {
    fn default() -> Self {
        Self {
            closed_lid_suspends: true,
            sepia: false,
        }
    }
}

/// Opaque colour from channels in 0 to 1.
fn opaque([r, g, b]: [f32; 3]) -> [f32; 4] {
    [r.clamp(0., 1.), g.clamp(0., 1.), b.clamp(0., 1.), 1.]
}

impl State {
    /// Store `config` and apply what differs from the one in effect.
    pub(crate) fn apply_config(&mut self, mut config: Config) {
        config.clamp();
        let old = mem::replace(&mut self.ewm.config, config.clone());
        if old.input != config.input {
            self.configure_input();
        }
        if old.idle != config.idle {
            let action = config.idle.command.map_or(DeactivateMonitors, RunCommand);
            self.ewm
                .configure_idle(config.idle.timeout.map(Duration::from_secs), action);
        }
        if old.cursor != config.cursor {
            let cursor = config.cursor.resolve();
            self.ewm.cursor_manager.reload(&cursor.theme, cursor.size);
            self.backend.clear_cursor_texture_cache();
            let vars = cursor.env_vars();
            self.backend.import_environment(&vars);
            self.ewm.queue_event(Event::Environment { vars });
            self.ewm.queue_redraw_for_pointer();
        }
        if old.cursor_hide != config.cursor_hide {
            let hide = config.cursor_hide;
            self.ewm
                .configure_cursor_hide(hide.timeout.map(Duration::from_secs), hide.when_typing);
        }
        if old.power != config.power {
            self.ewm.set_sepia(config.power.sepia);
            if old.power.closed_lid_suspends != config.power.closed_lid_suspends
                && let Some(drm) = self.backend.as_drm_mut()
            {
                drm.reapply_lid_policy(&mut self.ewm, LidPolicyReason::Config);
            }
        }
        if old.animations != config.animations {
            self.ewm
                .animations_clock
                .set_complete_instantly(!config.animations);
        }
        if old.unfocused_alpha != config.unfocused_alpha
            || old.blur != config.blur
            || old.overview != config.overview
            || old.background != config.background
        {
            let overview = &mut self.ewm.overview;
            overview.set_zoom(config.overview.zoom.unwrap_or(DEFAULT_ZOOM));
            overview.backdrop_color = config
                .overview
                .backdrop_color
                .map_or(DEFAULT_BACKDROP_COLOR, opaque);
            overview.background_color = config.background.map_or(DEFAULT_BACKGROUND_COLOR, opaque);
            self.ewm.queue_redraw_all();
        }
    }
}
