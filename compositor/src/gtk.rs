//! GTK interop with the host (pgtk) Emacs process.
//!
//! The compositor runs as a dynamic module in the Emacs process, which is
//! linked against libgtk-3.  Symbols are resolved at use via `dlopen` +
//! `dlsym` so we don't have a link-time dependency on GTK.  Calls here
//! must run on the Emacs main thread.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use anyhow::{Context as _, Result, anyhow, ensure};

/// `GTK_STYLE_PROVIDER_PRIORITY_APPLICATION`.
const PRIORITY: u32 = 600;

unsafe fn sym(lib: *mut c_void, name: &CStr) -> Result<*mut c_void> {
    unsafe {
        let p = libc::dlsym(lib, name.as_ptr());
        ensure!(!p.is_null(), "missing GTK symbol: {name:?}");
        Ok(p)
    }
}

/// Install a CSS provider on the default screen at application priority.
/// Re-applying overrides the previous provider, so theme changes need no
/// handle bookkeeping. Must run on the Emacs main thread.
pub fn set_style(css: &str) -> Result<()> {
    let css = std::ffi::CString::new(css).context("CSS contains NUL byte")?;

    unsafe {
        let lib = libc::dlopen(
            c"libgtk-3.so.0".as_ptr(),
            libc::RTLD_LAZY | libc::RTLD_NOLOAD,
        );
        ensure!(!lib.is_null(), "libgtk-3.so.0 is not loaded");

        let init_check: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) -> c_int =
            std::mem::transmute(sym(lib, c"gtk_init_check")?);
        let provider_new: unsafe extern "C" fn() -> *mut c_void =
            std::mem::transmute(sym(lib, c"gtk_css_provider_new")?);
        let load_from_data: unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            isize,
            *mut *mut c_void,
        ) -> c_int = std::mem::transmute(sym(lib, c"gtk_css_provider_load_from_data")?);
        let screen_get_default: unsafe extern "C" fn() -> *mut c_void =
            std::mem::transmute(sym(lib, c"gdk_screen_get_default")?);
        let add_provider: unsafe extern "C" fn(*mut c_void, *mut c_void, u32) =
            std::mem::transmute(sym(lib, c"gtk_style_context_add_provider_for_screen")?);
        let g_object_unref: unsafe extern "C" fn(*mut c_void) =
            std::mem::transmute(sym(lib, c"g_object_unref")?);

        // Required before the first frame so `gdk_screen_get_default' is non-null.
        ensure!(
            init_check(std::ptr::null_mut(), std::ptr::null_mut()) != 0,
            "gtk_init_check failed"
        );

        let provider = provider_new();
        if load_from_data(provider, css.as_ptr(), -1, std::ptr::null_mut()) == 0 {
            g_object_unref(provider);
            return Err(anyhow!("gtk_css_provider_load_from_data failed"));
        }
        let screen = screen_get_default();
        if screen.is_null() {
            g_object_unref(provider);
            return Err(anyhow!("gdk_screen_get_default returned null"));
        }
        add_provider(screen, provider, PRIORITY);

        libc::dlclose(lib);
    }
    Ok(())
}

type GetWindowFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type WindowFn = unsafe extern "C" fn(*mut c_void);
type GetDataFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type SetDataFn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void);

/// Cached function pointers for the freeze/thaw path; resolved once on first use.
struct FreezeApi {
    get_window: GetWindowFn,
    freeze: WindowFn,
    thaw: WindowFn,
    get_data: GetDataFn,
    set_data: SetDataFn,
}

unsafe impl Sync for FreezeApi {}

fn freeze_api() -> Result<&'static FreezeApi> {
    static API: OnceLock<Option<FreezeApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let lib = libc::dlopen(
            c"libgtk-3.so.0".as_ptr(),
            libc::RTLD_LAZY | libc::RTLD_NOLOAD,
        );
        if lib.is_null() {
            return None;
        }
        // libc::dlopen with RTLD_NOLOAD takes a refcount on the host's handle;
        // we never close it so the resolved pointers stay valid for the process lifetime.
        (|| -> Result<FreezeApi> {
            Ok(FreezeApi {
                get_window: std::mem::transmute::<*mut c_void, GetWindowFn>(sym(
                    lib,
                    c"gtk_widget_get_window",
                )?),
                freeze: std::mem::transmute::<*mut c_void, WindowFn>(sym(
                    lib,
                    c"gdk_window_freeze_updates",
                )?),
                thaw: std::mem::transmute::<*mut c_void, WindowFn>(sym(
                    lib,
                    c"gdk_window_thaw_updates",
                )?),
                get_data: std::mem::transmute::<*mut c_void, GetDataFn>(sym(
                    lib,
                    c"g_object_get_data",
                )?),
                set_data: std::mem::transmute::<*mut c_void, SetDataFn>(sym(
                    lib,
                    c"g_object_set_data",
                )?),
            })
        })()
        .ok()
    })
    .as_ref()
    .ok_or_else(|| anyhow!("libgtk-3.so.0 not loaded or missing symbols"))
}

/// Set whether the GtkWindow at WIDGET_PTR should skip its paint pipeline.
/// Thaw without matching freeze aborts under fatal-warnings, so we track
/// state per-widget via `g_object_set_data` (auto-released on widget destroy).
pub fn set_updates_frozen(widget_ptr: usize, frozen: bool) -> Result<()> {
    ensure!(widget_ptr != 0, "null GtkWindow pointer");
    let api = freeze_api()?;
    unsafe {
        let widget = widget_ptr as *mut c_void;
        let key = c"ewm-frozen".as_ptr();
        let was_frozen = !(api.get_data)(widget, key).is_null();
        if frozen == was_frozen {
            return Ok(());
        }
        let gdk_window = (api.get_window)(widget);
        ensure!(
            !gdk_window.is_null(),
            "widget has no GdkWindow (not realized?)"
        );
        if frozen {
            (api.freeze)(gdk_window);
            (api.set_data)(widget, key, std::ptr::dangling_mut::<c_void>());
        } else {
            (api.thaw)(gdk_window);
            (api.set_data)(widget, key, std::ptr::null_mut());
        }
    }
    Ok(())
}
