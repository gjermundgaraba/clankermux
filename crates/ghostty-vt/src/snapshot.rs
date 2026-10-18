//! Ghostty binary snapshot encoding and incremental decoding.

use std::ffi::c_void;
use std::ptr;

use ghostty_vt_sys as sys;

use crate::error::{Error, Result, check};
use crate::io;
use crate::terminal::{Handler, Term, Terminal};

/// Bytes before the first record: the 8-byte magic and the u16 version.
pub const ENVELOPE_LEN: usize = 10;
/// Bytes in a record header: tag u16, payload length u32, CRC32C u32.
pub const RECORD_HEADER_LEN: usize = 10;

pub const TAG_PAGE: u16 = 3;
pub const TAG_READY: u16 = 5;
pub const TAG_FINISH: u16 = 6;

/// Encodes the whole terminal, history included, into one buffer.
pub fn encode(term: &Term) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    // SAFETY: the writer borrows `out` only for the duration of this call.
    match check(unsafe { sys::ghostty_snapshot_encode(term.raw(), io::writer(&mut out)) }) {
        Ok(()) => Ok(out),
        // The engine reports an unfinished sequence that outgrew the
        // continuation limit as an unavailable continuation, with the same
        // code as a bad argument; a valid terminal and writer leave only
        // that cause.
        Err(Error::InvalidValue) => Err(Error::LimitExceeded),
        Err(err) => Err(err),
    }
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
/// `pos`, which is what one decoder `next_page` call consumes.
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
    // SAFETY: userdata is the Source owned by the Decoder, and buffer has
    // room for capacity bytes.
    unsafe {
        let source = &mut *(userdata as *mut Source);
        let n = capacity.min(source.data.len() - source.pos);
        ptr::copy_nonoverlapping(source.data.as_ptr().add(source.pos), buffer, n);
        source.pos += n;
        *out_read = n;
    }
    true
}

/// A pull decoder fed from a buffer the caller fills incrementally. It
/// owns the terminal it restores until [`Decoder::into_terminal`], because
/// the library writes history into that terminal on every page.
///
/// The decoder reads whole steps and is unusable after a short read, so
/// callers check [`Decoder::ready_available`] or
/// [`Decoder::step_available`] before [`Decoder::ready`] or
/// [`Decoder::next_page`].
pub struct Decoder<H: Handler = ()> {
    raw: sys::GhosttySnapshotDecoder,
    source: *mut Source,
    terminal: Option<Terminal<H>>,
    /// Installed on the terminal once it exists.
    handler: Option<H>,
}

impl Decoder<()> {
    pub fn new() -> Result<Self> {
        Self::with_handler(())
    }
}

impl<H: Handler> Decoder<H> {
    /// A decoder whose restored terminal gets `handler` as soon as it
    /// exists, so effects raised while history is written reach it.
    pub fn with_handler(handler: H) -> Result<Self> {
        let source = Box::into_raw(Box::new(Source {
            data: Vec::new(),
            pos: 0,
        }));
        let reader = sys::GhosttyReader {
            read: Some(read_source),
            userdata: source as *mut c_void,
        };
        let mut raw = ptr::null_mut();
        // SAFETY: the source outlives the decoder; Drop frees the decoder
        // first.
        if let Err(err) =
            check(unsafe { sys::ghostty_snapshot_decoder_new(ptr::null(), &mut raw, reader) })
        {
            // SAFETY: source came from Box::into_raw above and was not
            // handed to a decoder.
            drop(unsafe { Box::from_raw(source) });
            return Err(err);
        }
        Ok(Decoder {
            raw,
            source,
            terminal: None,
            handler: Some(handler),
        })
    }

    /// Wraps the terminal the library just returned, with the handler.
    unsafe fn adopt(&mut self, raw: sys::GhosttyTerminal) -> &mut Terminal<H> {
        let handler = self.handler.take().expect("one terminal per decoder");
        // SAFETY: raw is a caller-owned terminal from the decoder.
        self.terminal
            .insert(unsafe { Terminal::install(raw, handler, Vec::new()) })
    }

    fn source(&self) -> &Source {
        // SAFETY: source is live until Drop and the library only touches it
        // during decoder calls, which take `&mut self`.
        unsafe { &*self.source }
    }

    fn source_mut(&mut self) -> &mut Source {
        // SAFETY: as in `source`.
        unsafe { &mut *self.source }
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
        self.source_mut().data.extend_from_slice(bytes);
    }

    /// Bytes pushed but not yet consumed.
    pub fn buffered(&self) -> usize {
        let source = self.source();
        source.data.len() - source.pos
    }

    /// Whether every byte through `READY` is buffered. Only meaningful
    /// before [`Decoder::ready`].
    pub fn ready_available(&self) -> bool {
        ready_end(&self.source().data).is_some()
    }

    pub fn step_available(&self) -> bool {
        let source = self.source();
        step_end(&source.data, source.pos).is_some()
    }

    /// Decodes through `READY`; the terminal is then renderable.
    pub fn ready(&mut self) -> Result<&mut Terminal<H>> {
        let mut raw = ptr::null_mut();
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_ready(self.raw, &mut raw) };
        self.compact();
        check(result)?;
        // SAFETY: on success raw is a caller-owned terminal.
        Ok(unsafe { self.adopt(raw) })
    }

    /// Decodes one history page into the terminal. Returns `false` once
    /// `FINISH` is validated.
    pub fn next_page(&mut self) -> Result<bool> {
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_next(self.raw) };
        self.compact();
        if result == sys::GHOSTTY_NO_VALUE {
            return Ok(false);
        }
        check(result)?;
        Ok(true)
    }

    /// Decodes a complete snapshot in one call.
    pub fn decode(&mut self) -> Result<&mut Terminal<H>> {
        let mut raw = ptr::null_mut();
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_decode(self.raw, &mut raw) };
        self.compact();
        check(result)?;
        // SAFETY: on success raw is a caller-owned terminal.
        Ok(unsafe { self.adopt(raw) })
    }

    /// The terminal restored so far, for rendering and live input between
    /// pages.
    pub fn terminal(&mut self) -> Option<&mut Terminal<H>> {
        self.terminal.as_mut()
    }

    /// Frees the decoder and releases the terminal, finished or not.
    pub fn into_terminal(mut self) -> Option<Terminal<H>> {
        self.terminal.take()
    }

    fn compact(&mut self) {
        let source = self.source_mut();
        source.data.drain(..source.pos);
        source.pos = 0;
    }
}

impl<H: Handler> Drop for Decoder<H> {
    fn drop(&mut self) {
        // SAFETY: raw was created by decoder_new and is freed once, before
        // the terminal it borrows (dropped after this) and the source.
        unsafe {
            sys::ghostty_snapshot_decoder_free(self.raw);
            drop(Box::from_raw(self.source));
        }
    }
}

/// Decodes a complete snapshot held in memory.
pub fn decode(bytes: &[u8]) -> Result<Terminal> {
    let mut decoder = Decoder::new()?;
    decoder.push(bytes);
    decoder.decode()?;
    Ok(decoder.into_terminal().expect("decoded"))
}
