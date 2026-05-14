//! GTK CSS injection into the host (pgtk) Emacs process.
//!
//! pgtk commits its first wl_surface buffer with the GTK widget background
//! before redisplay paints (bug#53232). The compositor runs as a dynamic
//! module in the Emacs process, which is linked against libgtk-3, so the
//! CSS-provider symbols are already in the address space; resolve at use.

use std::ffi::{c_char, c_int, c_void, CStr};

use anyhow::{anyhow, ensure, Context as _, Result};

/// `GTK_STYLE_PROVIDER_PRIORITY_APPLICATION`.
const PRIORITY: u32 = 600;

unsafe fn sym(lib: *mut c_void, name: &CStr) -> Result<*mut c_void> {
    let p = libc::dlsym(lib, name.as_ptr());
    ensure!(!p.is_null(), "missing GTK symbol: {name:?}");
    Ok(p)
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
