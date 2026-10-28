use std::ffi::c_void;
use std::ptr;

use ghostty_vt_sys as sys;

use crate::error::{Error, Result, check};
use crate::io;
use crate::terminal::{Handler, Term, Terminal};

/// Bytes before the first record: the 8-byte magic and the u16 version.
const ENVELOPE_LEN: usize = 10;
/// Bytes in a record header: tag u16, payload length u32, CRC32C u32.
const RECORD_HEADER_LEN: usize = 10;

const TAG_PAGE: u16 = 3;
const TAG_READY: u16 = 5;
const TAG_FINISH: u16 = 6;

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

fn record_at(buf: &[u8], pos: usize) -> Option<(u16, usize)> {
    let header = buf.get(pos..pos + RECORD_HEADER_LEN)?;
    let tag = u16::from_le_bytes([header[0], header[1]]);
    let len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
    let end = pos + RECORD_HEADER_LEN + len;
    (end <= buf.len()).then_some((tag, end))
}

pub fn ready_end(buf: &[u8]) -> Option<usize> {
    end_of_step(buf, ENVELOPE_LEN, |tag| tag == TAG_READY)
}

/// Position just past the next `PAGE` or `FINISH` record at or after
/// `pos`, which is what one decoder `next_page` call consumes.
fn step_end(buf: &[u8], pos: usize) -> Option<usize> {
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
        let source = &mut *userdata.cast::<Source>();
        let n = capacity.min(source.data.len() - source.pos);
        ptr::copy_nonoverlapping(source.data.as_ptr().add(source.pos), buffer, n);
        source.pos += n;
        *out_read = n;
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Page,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTerminal {
    BeforeReady,
    Replaced,
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
    /// The terminal the library retains between steps, kept alive here
    /// whatever is done through `terminal`.
    owner: Option<Terminal<H>>,
    terminal: Option<Terminal<H>>,
    handler: Option<H>,
    finished: bool,
}

impl Decoder<()> {
    pub fn new() -> Result<Self> {
        Self::with_handler(())
    }
}

impl<H: Handler> Decoder<H> {
    pub fn with_handler(handler: H) -> Result<Self> {
        let source = Box::into_raw(Box::new(Source {
            data: Vec::new(),
            pos: 0,
        }));
        let reader = sys::GhosttyReader {
            read: Some(read_source),
            userdata: source.cast::<c_void>(),
        };
        let mut raw = ptr::null_mut();
        // SAFETY: the source outlives the decoder; Drop frees the decoder
        // first.
        if let Err(err) =
            check(unsafe { sys::ghostty_snapshot_decoder_new(ptr::null(), &raw mut raw, reader) })
        {
            drop(unsafe { Box::from_raw(source) });
            return Err(err);
        }
        Ok(Decoder {
            raw,
            source,
            owner: None,
            terminal: None,
            handler: Some(handler),
            finished: false,
        })
    }

    unsafe fn adopt(&mut self, raw: sys::GhosttyTerminal) -> &mut Terminal<H> {
        let handler = self.handler.take().expect("one terminal per decoder");
        // SAFETY: raw is a caller-owned terminal from the decoder.
        let owner = unsafe { Terminal::install(raw, handler) };
        let shared = owner.share();
        self.owner = Some(owner);
        self.terminal.insert(shared)
    }

    /// Refuses to go on when the terminal handed out was replaced (safe
    /// code can `mem::replace` through the `&mut`), since the library
    /// still holds the one it returned. The owner keeps that one alive,
    /// so another terminal cannot take its address, and whatever was
    /// moved out stays a valid terminal of its own.
    fn intact(&self) -> Result<()> {
        match (&self.terminal, &self.owner) {
            (Some(shared), Some(owner)) if shared.raw() != owner.raw() => Err(Error::InvalidValue),
            _ => Ok(()),
        }
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    fn source(&self) -> &Source {
        // SAFETY: source is live until Drop and the library only touches it
        // during decoder calls, which take `&mut self`.
        unsafe { &*self.source }
    }

    fn source_mut(&mut self) -> &mut Source {
        unsafe { &mut *self.source }
    }

    pub fn set_max_continuation_bytes(&mut self, max: usize) -> Result<()> {
        // SAFETY: option value type is size_t*.
        check(unsafe {
            sys::ghostty_snapshot_decoder_set(
                self.raw,
                sys::GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
                (&raw const max).cast::<c_void>(),
            )
        })
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.compact();
        self.source_mut().data.extend_from_slice(bytes);
    }

    #[cfg(test)]
    pub(crate) fn buffered(&self) -> usize {
        let source = self.source();
        source.data.len() - source.pos
    }

    pub fn ready_available(&self) -> bool {
        ready_end(&self.source().data).is_some()
    }

    pub fn step_available(&self) -> bool {
        let source = self.source();
        !self.finished && step_end(&source.data, source.pos).is_some()
    }

    pub fn ready(&mut self) -> Result<&mut Terminal<H>> {
        let mut raw = ptr::null_mut();
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_ready(self.raw, &raw mut raw) };
        check(result)?;
        // SAFETY: on success raw is a caller-owned terminal.
        Ok(unsafe { self.adopt(raw) })
    }

    pub fn next_page(&mut self) -> Result<Step> {
        self.intact()?;
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_next(self.raw) };
        if result == sys::GHOSTTY_NO_VALUE {
            self.finished = true;
            return Ok(Step::Finished);
        }
        check(result)?;
        Ok(Step::Page)
    }

    /// Decodes a complete snapshot in one call; the engine refuses it
    /// after `ready`.
    fn decode(&mut self) -> Result<&mut Terminal<H>> {
        let mut raw = ptr::null_mut();
        // SAFETY: raw is a live decoder.
        let result = unsafe { sys::ghostty_snapshot_decoder_decode(self.raw, &raw mut raw) };
        self.compact();
        check(result)?;
        self.finished = true;
        // SAFETY: on success raw is a caller-owned terminal.
        Ok(unsafe { self.adopt(raw) })
    }

    pub fn terminal(&mut self) -> Option<&mut Terminal<H>> {
        self.terminal.as_mut()
    }

    pub fn into_terminal(mut self) -> std::result::Result<Terminal<H>, NoTerminal> {
        if self.intact().is_err() {
            return Err(NoTerminal::Replaced);
        }
        self.terminal.take().ok_or(NoTerminal::BeforeReady)
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

pub fn decode(bytes: &[u8]) -> Result<Terminal> {
    let mut decoder = Decoder::new()?;
    decoder.push(bytes);
    decoder.decode()?;
    Ok(decoder.into_terminal().expect("decoded"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_stay_in_place_until_the_next_received_chunk() {
        let mut term = Terminal::new(80, 24).unwrap();
        term.set_scrollback_max_bytes(10 * 1024 * 1024).unwrap();
        term.vt_write(&b"a line of history\r\n".repeat(5000));
        let bytes = encode(&term).unwrap();
        let split = bytes.len() - 5;
        let mut decoder = Decoder::new().unwrap();
        decoder.push(&bytes[..split]);
        assert!(decoder.ready_available());
        decoder.ready().unwrap();
        assert_eq!(decoder.source().data.len(), split);
        let mut pages = 0;
        while decoder.step_available() {
            let pos = decoder.source().pos;
            assert_eq!(decoder.next_page().unwrap(), Step::Page);
            assert!(decoder.source().pos > pos);
            assert_eq!(decoder.source().data.len(), split);
            pages += 1;
        }
        assert!(pages > 1);
        assert!(!decoder.finished());
        let tail = decoder.source().data[decoder.source().pos..].to_vec();
        decoder.push(&bytes[split..]);
        assert_eq!(decoder.source().pos, 0);
        assert_eq!(&decoder.source().data[..tail.len()], tail);
        assert!(decoder.step_available());
        assert_eq!(decoder.next_page().unwrap(), Step::Finished);
        assert!(decoder.finished());
        assert_eq!(decoder.buffered(), 0);
        let restored = decoder.into_terminal().unwrap();
        assert_eq!(
            restored.scrollback_rows().unwrap(),
            term.scrollback_rows().unwrap()
        );
        let options = crate::formatter::Options {
            unwrap: true,
            trim: true,
            ..Default::default()
        };
        assert_eq!(
            crate::formatter::format(&restored, options).unwrap(),
            crate::formatter::format(&term, options).unwrap()
        );
    }
}
