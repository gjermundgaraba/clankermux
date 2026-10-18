//! Ghostty binary snapshot encoding and incremental decoding.

use std::ffi::c_void;
use std::ptr;

use ghostty_vt_sys as sys;

use crate::error::{Result, check};
use crate::io;
use crate::terminal::{Term, Terminal};

/// Bytes before the first record: the 8-byte magic and the u16 version.
pub const ENVELOPE_LEN: usize = 10;
/// Bytes in a record header: tag u16, payload length u32, CRC32C u32.
pub const RECORD_HEADER_LEN: usize = 10;

pub const TAG_TERMINAL: u16 = 1;
pub const TAG_SCREEN: u16 = 2;
pub const TAG_PAGE: u16 = 3;
pub const TAG_HISTORY: u16 = 4;
pub const TAG_READY: u16 = 5;
pub const TAG_FINISH: u16 = 6;
pub const TAG_CONTINUATION: u16 = 7;

/// Encodes the whole terminal, history included, into one buffer.
pub fn encode(term: &Term) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    // SAFETY: the writer borrows `out` only for the duration of this call.
    check(unsafe { sys::ghostty_snapshot_encode(term.raw(), io::writer(&mut out)) })?;
    Ok(out)
}

/// The record starting at `pos`: its tag and the position just past it,
/// or `None` when the buffer ends before the record does.
pub fn record_at(buf: &[u8], pos: usize) -> Option<(u16, usize)> {
    let header = buf.get(pos..pos + RECORD_HEADER_LEN)?;
    let tag = u16::from_le_bytes([header[0], header[1]]);
    let len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
    let end = pos + RECORD_HEADER_LEN + len;
    (end <= buf.len()).then_some((tag, end))
}

/// Position just past the `READY` record, once every byte through it is
/// in `buf`.
pub fn ready_end(buf: &[u8]) -> Option<usize> {
    end_of_step(buf, ENVELOPE_LEN, |tag| tag == TAG_READY)
}

/// Position just past the next `PAGE` or `FINISH` record at or after
/// `pos`, which is what one decoder `next` call consumes.
pub fn step_end(buf: &[u8], pos: usize) -> Option<usize> {
    end_of_step(buf, pos, |tag| tag == TAG_PAGE || tag == TAG_FINISH)
}

fn end_of_step(buf: &[u8], mut pos: usize, last: impl Fn(u16) -> bool) -> Option<usize> {
    loop {
        let (tag, end) = record_at(buf, pos)?;
        if last(tag) {
            return Some(end);
        }
        pos = end;
    }
}

struct Source {
    data: Vec<u8>,
    pos: usize,
}

unsafe extern "C" fn read_source(
    userdata: *mut c_void,
    buffer: *mut u8,
    capacity: usize,
    out_read: *mut usize,
) -> bool {
    // SAFETY: userdata is the boxed Source owned by the Decoder, and
    // buffer has room for capacity bytes.
    unsafe {
        let source = &mut *(userdata as *mut Source);
        let n = capacity.min(source.data.len() - source.pos);
        ptr::copy_nonoverlapping(source.data.as_ptr().add(source.pos), buffer, n);
        source.pos += n;
        *out_read = n;
    }
    true
}

/// A pull decoder fed from a buffer the caller fills incrementally.
///
/// The decoder reads whole steps and is unusable after a short read, so
/// callers check [`Decoder::ready_available`] or [`Decoder::step_available`]
/// before [`Decoder::ready`] or [`Decoder::next_page`].
pub struct Decoder {
    raw: sys::GhosttySnapshotDecoder,
    source: Box<Source>,
    finished: bool,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        let mut source = Box::new(Source {
            data: Vec::new(),
            pos: 0,
        });
        let reader = sys::GhosttyReader {
            read: Some(read_source),
            userdata: &mut *source as *mut Source as *mut c_void,
        };
        let mut raw = ptr::null_mut();
        // SAFETY: the reader's userdata outlives the decoder because both
        // live in this struct and the decoder is freed first.
        check(unsafe { sys::ghostty_snapshot_decoder_new(ptr::null(), &mut raw, reader) })?;
        Ok(Decoder {
            raw,
            source,
            finished: false,
        })
    }

    pub fn set_max_continuation_bytes(&mut self, max: usize) -> Result<()> {
        // SAFETY: option value type is size_t*.
        check(unsafe {
            sys::ghostty_snapshot_decoder_set(
                self.raw,
                sys::GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
                &max as *const usize as *const c_void,
            )
        })
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.source.data.extend_from_slice(bytes);
    }

    /// Bytes pushed but not yet consumed.
    pub fn buffered(&self) -> usize {
        self.source.data.len() - self.source.pos
    }

    pub fn ready_available(&self) -> bool {
        self.source.pos == 0 && ready_end(&self.source.data).is_some()
    }

    pub fn step_available(&self) -> bool {
        step_end(&self.source.data, self.source.pos).is_some()
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Decodes through `READY` and returns the renderable terminal.
    pub fn ready(&mut self) -> Result<Terminal> {
        let mut raw = ptr::null_mut();
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_ready(self.raw, &mut raw) };
        self.compact();
        check(result)?;
        // SAFETY: on success raw is a caller-owned terminal.
        Ok(unsafe { Terminal::from_raw(raw) })
    }

    /// Decodes one history page. Returns `false` once `FINISH` is validated.
    pub fn next_page(&mut self) -> Result<bool> {
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_next(self.raw) };
        self.compact();
        if result == sys::GHOSTTY_NO_VALUE {
            self.finished = true;
            return Ok(false);
        }
        check(result)?;
        Ok(true)
    }

    /// Decodes a complete snapshot in one call.
    pub fn decode(&mut self) -> Result<Terminal> {
        let mut raw = ptr::null_mut();
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_decode(self.raw, &mut raw) };
        self.compact();
        check(result)?;
        self.finished = true;
        // SAFETY: on success raw is a caller-owned terminal.
        Ok(unsafe { Terminal::from_raw(raw) })
    }

    fn compact(&mut self) {
        self.source.data.drain(..self.source.pos);
        self.source.pos = 0;
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: raw was created by decoder_new and is freed once.
        unsafe { sys::ghostty_snapshot_decoder_free(self.raw) }
    }
}

/// Decodes a complete snapshot held in memory.
pub fn decode(bytes: &[u8]) -> Result<Terminal> {
    let mut decoder = Decoder::new()?;
    decoder.push(bytes);
    decoder.decode()
}
