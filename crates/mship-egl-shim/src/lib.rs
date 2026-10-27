//! libEGL.so for Cuttlefish's crosvm/gfxstream (the Android VM's gfxstream fallback, /opt/crosvm/bin).
//!
//! gfxstream registers an EGL_ANDROID_blob_cache callback (GetBlob/SetBlob) with the host EGL; with Mesa freedreno
//! its GetBlob lookup spins inside shader compiles (render thread stuck at 100% CPU, guest UI hangs). This hides
//! the extension; every other EGL entry point resolves to the real libEGL.so.1 (DT_NEEDED).

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Mutex;

const EGL_EXTENSIONS: i32 = 0x3055;
const HIDDEN: &str = "EGL_ANDROID_blob_cache";

type QueryString = unsafe extern "C" fn(*mut c_void, i32) -> *const c_char;
type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;

fn real(name: &CStr) -> *mut c_void {
    static HANDLE: Mutex<usize> = Mutex::new(0);
    let mut h = HANDLE.lock().unwrap();
    if *h == 0 {
        *h = unsafe { libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) } as usize;
    }
    if *h == 0 { std::ptr::null_mut() } else { unsafe { libc::dlsym(*h as *mut c_void, name.as_ptr()) } }
}

/// The extension list without the hidden one (strings live as long as the display: one per input)
fn strip(s: &str) -> String {
    s.split(' ').filter(|e| !e.is_empty() && *e != HIDDEN).collect::<Vec<_>>().join(" ")
}

/// # Safety
/// EGL's eglQueryString.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglQueryString(dpy: *mut c_void, name: i32) -> *const c_char {
    static LAST: Mutex<(usize, Option<CString>)> = Mutex::new((0, None));
    let f = real(c"eglQueryString");
    if f.is_null() {
        return std::ptr::null();
    }
    let s = unsafe { std::mem::transmute::<*mut c_void, QueryString>(f)(dpy, name) };
    if name != EGL_EXTENSIONS || s.is_null() {
        return s;
    }
    let text = unsafe { CStr::from_ptr(s) }.to_string_lossy();
    if !text.split(' ').any(|e| e == HIDDEN) {
        return s;
    }
    let mut last = LAST.lock().unwrap();
    if last.0 != s as usize || last.1.is_none() {
        *last = (s as usize, CString::new(strip(&text)).ok());
    }
    last.1.as_ref().map_or(s, |c| c.as_ptr())
}

/// # Safety
/// EGL's eglGetProcAddress.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglGetProcAddress(name: *const c_char) -> *mut c_void {
    let n = unsafe { CStr::from_ptr(name) };
    if n == c"eglSetBlobCacheFuncsANDROID" {
        return std::ptr::null_mut();
    }
    if n == c"eglQueryString" {
        return eglQueryString as *mut c_void;
    }
    let f = real(c"eglGetProcAddress");
    if f.is_null() { std::ptr::null_mut() } else { unsafe { std::mem::transmute::<*mut c_void, GetProcAddress>(f)(name) } }
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_only_the_blob_cache() {
        assert_eq!(super::strip("EGL_KHR_a EGL_ANDROID_blob_cache  EGL_KHR_b"), "EGL_KHR_a EGL_KHR_b");
        assert_eq!(super::strip("EGL_ANDROID_blob_cache_x"), "EGL_ANDROID_blob_cache_x");
    }
}
