//! Writer and reader callbacks over Rust buffers.

use std::ffi::c_void;

use ghostty_vt_sys as sys;

unsafe extern "C" fn write_to_vec(userdata: *mut c_void, data: *const u8, len: usize) -> bool {
    // SAFETY: userdata is the `*mut Vec<u8>` passed by the caller that owns
    // the writer, and data points at len readable bytes for this call.
    unsafe {
        let out = &mut *userdata.cast::<Vec<u8>>();
        out.extend_from_slice(bytes(data, len));
    }
    true
}

/// `len` bytes at `data`; empty for a null pointer, which the library may
/// pass with a zero length.
///
/// # Safety
/// A non-null `data` points at `len` readable bytes for the lifetime.
pub(crate) unsafe fn bytes<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if data.is_null() || len == 0 {
        return &[];
    }
    // SAFETY: the caller's.
    unsafe { std::slice::from_raw_parts(data, len) }
}

pub(crate) fn writer(out: &mut Vec<u8>) -> sys::GhosttyWriter {
    sys::GhosttyWriter {
        write: Some(write_to_vec),
        userdata: std::ptr::from_mut(out).cast(),
    }
}

/// Copies a borrowed `GhosttyString` into an owned string, lossily.
pub(crate) unsafe fn string(s: &sys::GhosttyString) -> String {
    // SAFETY: the library guarantees ptr points at len bytes for the
    // duration of the call this string was received in.
    String::from_utf8_lossy(unsafe { bytes(s.ptr, s.len) }).into_owned()
}
