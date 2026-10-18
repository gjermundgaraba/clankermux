//! Writer and reader callbacks over Rust buffers.

use std::ffi::c_void;

use ghostty_vt_sys as sys;

pub(crate) unsafe extern "C" fn write_to_vec(
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) -> bool {
    // SAFETY: userdata is the `*mut Vec<u8>` passed by the caller that owns
    // the writer, and data points at len readable bytes for this call.
    unsafe {
        let out = &mut *(userdata as *mut Vec<u8>);
        out.extend_from_slice(std::slice::from_raw_parts(data, len));
    }
    true
}

pub(crate) fn writer(out: &mut Vec<u8>) -> sys::GhosttyWriter {
    sys::GhosttyWriter {
        write: Some(write_to_vec),
        userdata: out as *mut Vec<u8> as *mut c_void,
    }
}

/// Copies a borrowed `GhosttyString` into an owned string, lossily.
pub(crate) unsafe fn string(s: &sys::GhosttyString) -> String {
    if s.ptr.is_null() || s.len == 0 {
        return String::new();
    }
    // SAFETY: the library guarantees ptr points at len bytes for the
    // duration of the call this string was received in.
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
    String::from_utf8_lossy(bytes).into_owned()
}
