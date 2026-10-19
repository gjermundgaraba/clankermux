//! `wait`, and the wait a `send` carries: facts about a session checked
//! from a cursor on, each time its stream changes.

use std::time::Duration;

use proto::{CursorParams, ErrorCode, Match, StreamCursor, Until, WaitReason, Waited};
use regex::bytes::Regex;
use tokio::time::Instant;

use crate::registry::Fail;
use crate::session::{Body, Session, Shared};

/// The OSC 7501 states a status wait may name.
const STATES: [&str; 5] = ["idle", "working", "done", "blocked", "error"];

/// Plain text kept for an output match: older text is dropped half at a
/// time.
const MAX_PLAIN: usize = 256 * 1024;

/// How far before new text a regex is searched: a match is found when it
/// starts at most this far before the text that completes it.
const REGEX_REACH: usize = 64 * 1024;

/// What `until.output` looks for.
enum Needle {
    Text(Vec<u8>),
    Regex(Regex),
}

/// An `until`, checked: a regex compiled, the states known.
pub struct Facts {
    exited: bool,
    output: Option<Needle>,
    status: Option<(Vec<String>, Option<String>)>,
    prompt: bool,
    quiet: Option<Duration>,
}

impl Facts {
    pub fn new(until: Until) -> Result<Self, Fail> {
        let invalid = |m: String| (ErrorCode::InvalidRequest, m);
        let flag = |name: &str, value: Option<bool>| match value {
            None => Ok(false),
            Some(true) => Ok(true),
            Some(false) => Err(invalid(format!("until.{name} is true or absent"))),
        };
        let output = match until.output {
            None => None,
            Some(o) => Some(match (o.text, o.regex) {
                (Some(text), None) if !text.is_empty() => Needle::Text(text.into_bytes()),
                (None, Some(regex)) => Needle::Regex(
                    Regex::new(&regex).map_err(|e| invalid(format!("until.output.regex: {e}")))?,
                ),
                _ => {
                    return Err(invalid(
                        "until.output takes one of text (not empty) and regex".into(),
                    ));
                }
            }),
        };
        let status = match until.status {
            None => None,
            Some(s) => {
                if s.states.is_empty() {
                    return Err(invalid("until.status.states is empty".into()));
                }
                if let Some(bad) = s.states.iter().find(|st| !STATES.contains(&st.as_str())) {
                    return Err(invalid(format!("until.status: unknown state {bad:?}")));
                }
                Some((s.states, s.record))
            }
        };
        let facts = Facts {
            exited: flag("exited", until.exited)?,
            output,
            status,
            prompt: flag("prompt", until.prompt)?,
            quiet: until.quiet_ms.map(Duration::from_millis),
        };
        if !facts.exited
            && facts.output.is_none()
            && facts.status.is_none()
            && !facts.prompt
            && facts.quiet.is_none()
        {
            return Err(invalid("until names nothing to wait for".into()));
        }
        Ok(facts)
    }
}

/// The session's cursor now.
pub fn cursor_of(s: &Session) -> StreamCursor {
    match &s.body {
        Body::Live(live) => StreamCursor {
            offset: live.stream.offset(),
            marker: live.stream.latest_seq(),
        },
        Body::Parked(parked) => StreamCursor {
            offset: parked.offset,
            marker: parked.marker,
        },
    }
}

/// `after` checked against the session: not past its cursor.
pub fn anchor(s: &Session, after: Option<CursorParams>) -> Result<StreamCursor, Fail> {
    let now = cursor_of(s);
    let Some(after) = after else {
        return Ok(now);
    };
    if after.offset > now.offset || after.marker > now.marker {
        return Err((
            ErrorCode::InvalidRequest,
            format!(
                "after {}/{} is past the session's cursor {}/{}",
                after.offset, after.marker, now.offset, now.marker
            ),
        ));
    }
    Ok(StreamCursor {
        offset: after.offset,
        marker: after.marker,
    })
}

/// Escape sequences, stripped from the stream for an output match.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Vt {
    Ground,
    Escape,
    Intermediate,
    Csi,
    /// OSC, DCS, APC, PM or SOS, until BEL or ST.
    Text,
    TextEscape,
}

/// The stream as plain text from the wait's cursor on: printable
/// characters, newlines and tabs, without escape sequences, carriage
/// returns or other controls, and where each run of it came from.
struct Plain {
    vt: Vt,
    bytes: Vec<u8>,
    /// `(index in bytes, stream offset)` at the start of each run of
    /// bytes kept from consecutive stream bytes.
    runs: Vec<(usize, u64)>,
    /// The stream offset after the last byte kept.
    next_kept: Option<u64>,
    /// The stream offset after the last byte pushed.
    next_seen: Option<u64>,
    /// Where a text needle is searched for next.
    searched: usize,
}

impl Plain {
    fn new() -> Self {
        Plain {
            vt: Vt::Ground,
            bytes: Vec::new(),
            runs: Vec::new(),
            next_kept: None,
            next_seen: None,
            searched: 0,
        }
    }

    fn push(&mut self, offset: u64, chunk: &[u8]) {
        if self.next_seen.is_some_and(|next| next != offset) {
            // Bytes were lost: whatever sequence was open is gone too.
            self.vt = Vt::Ground;
        }
        self.next_seen = Some(offset + chunk.len() as u64);
        for (i, &b) in chunk.iter().enumerate() {
            let keep = self.step(b);
            if keep {
                let at = offset + i as u64;
                if self.next_kept != Some(at) {
                    self.runs.push((self.bytes.len(), at));
                }
                self.bytes.push(b);
                self.next_kept = Some(at + 1);
            }
        }
    }

    /// Drops the older half once over `MAX_PLAIN`.
    fn trim(&mut self) {
        if self.bytes.len() > MAX_PLAIN {
            self.drop_front(self.bytes.len() - MAX_PLAIN / 2);
        }
    }

    /// Advances the escape parser; whether `b` is plain text.
    fn step(&mut self, b: u8) -> bool {
        const ESC: u8 = 0x1b;
        let abort = b == 0x18 || b == 0x1a;
        self.vt = match self.vt {
            Vt::Ground => match b {
                ESC => Vt::Escape,
                b'\n' | b'\t' => return true,
                0x20..=0x7e | 0x80..=0xff => return true,
                _ => Vt::Ground,
            },
            Vt::Escape => match b {
                b'[' => Vt::Csi,
                b']' | b'P' | b'X' | b'^' | b'_' => Vt::Text,
                0x20..=0x2f => Vt::Intermediate,
                ESC => Vt::Escape,
                _ => Vt::Ground,
            },
            Vt::Intermediate => match b {
                0x20..=0x2f => Vt::Intermediate,
                _ => Vt::Ground,
            },
            Vt::Csi => match b {
                _ if abort => Vt::Ground,
                ESC => Vt::Escape,
                0x40..=0x7e => Vt::Ground,
                _ => Vt::Csi,
            },
            Vt::Text => match b {
                _ if abort => Vt::Ground,
                0x07 => Vt::Ground,
                ESC => Vt::TextEscape,
                _ => Vt::Text,
            },
            Vt::TextEscape => match b {
                b'\\' => Vt::Ground,
                _ => {
                    self.vt = Vt::Escape;
                    return self.step(b);
                }
            },
        };
        false
    }

    fn drop_front(&mut self, n: usize) {
        let first = self.offset_of(n);
        self.bytes.drain(..n);
        self.runs.retain(|&(i, _)| i > n);
        for run in &mut self.runs {
            run.0 -= n;
        }
        self.runs.insert(0, (0, first));
        self.searched = self.searched.saturating_sub(n);
    }

    /// The stream offset of the kept byte at `index`.
    fn offset_of(&self, index: usize) -> u64 {
        let run = self.runs.partition_point(|&(i, _)| i <= index) - 1;
        let (start, offset) = self.runs[run];
        offset + (index - start) as u64
    }

    /// The first match from where the last search left off; a regex from
    /// `REGEX_REACH` before `new`, where the text not yet searched starts.
    fn find(&mut self, needle: &Needle, new: usize) -> Option<(usize, usize)> {
        match needle {
            Needle::Text(text) => {
                let found = self.bytes[self.searched..]
                    .windows(text.len())
                    .position(|w| w == text.as_slice())
                    .map(|at| (self.searched + at, self.searched + at + text.len()));
                self.searched = (self.bytes.len() + 1).saturating_sub(text.len());
                found
            }
            Needle::Regex(regex) => regex
                .find_at(&self.bytes, new.saturating_sub(REGEX_REACH))
                .map(|m| (m.start(), m.end())),
        }
    }
}

/// One wait: its facts, from its cursor on.
pub struct Waiter {
    facts: Facts,
    after: StreamCursor,
    /// Program status counts the records set after this tick.
    status_since: u64,
    /// The next stream byte to scan for `output`.
    scanned: u64,
    plain: Plain,
    started: Instant,
}

/// What a check found.
pub enum Check {
    Fired(Box<Waited>),
    /// Nothing yet: check again on the next change, or at this instant.
    Pending(Option<Instant>),
}

impl Waiter {
    /// A `wait` from `after`: a fact that already holds there fires.
    pub fn new(facts: Facts, after: StreamCursor) -> Self {
        Waiter {
            facts,
            after,
            status_since: 0,
            scanned: after.offset,
            plain: Plain::new(),
            started: Instant::now(),
        }
    }

    /// A `send`'s wait, from the session's cursor now: program status
    /// counts only records set from now on, since one already in a state
    /// is the previous turn's.
    pub fn for_send(facts: Facts, s: &Session) -> Self {
        Waiter {
            status_since: s.records.tick(),
            ..Waiter::new(facts, cursor_of(s))
        }
    }

    /// Under the session lock: the stream bytes not yet scanned for
    /// `output`, from where they start. Bytes the ring no longer holds
    /// are skipped.
    fn unscanned(&mut self, s: &Session) -> Option<(u64, Vec<u8>)> {
        self.facts.output.as_ref()?;
        let Body::Live(live) = &s.body else {
            return None;
        };
        self.scanned = self.scanned.max(live.stream.retained_start());
        let chunk = live.stream.read(self.scanned, usize::MAX)?;
        let at = self.scanned;
        self.scanned += chunk.len() as u64;
        Some((at, chunk))
    }

    /// Outside the lock: strips and searches the new bytes.
    fn scan(&mut self, new: Option<(u64, Vec<u8>)>) -> Option<(usize, usize)> {
        let needle = self.facts.output.as_ref()?;
        let before = self.plain.bytes.len();
        if let Some((at, chunk)) = new {
            self.plain.push(at, &chunk);
        }
        let found = self.plain.find(needle, before);
        if found.is_none() {
            self.plain.trim();
        }
        found
    }

    /// Checks the facts against the session, in stream order: output
    /// (`found` by `scan`), prompt, program status, the end of the stream,
    /// quiet.
    fn check(&mut self, s: &Session, found: Option<(usize, usize)>) -> Result<Check, Fail> {
        if s.removed {
            return Err((ErrorCode::NoSuchSession, format!("no session {}", s.id)));
        }
        let fired = |reason, at| Waited {
            reason,
            at,
            exit: None,
            matched: None,
            record: None,
        };
        let live = match &s.body {
            Body::Live(live) => Some(live),
            Body::Parked(_) => None,
        };
        if let Some((start, end)) = found {
            let offset = self.plain.offset_of(start);
            let after = if end > start {
                self.plain.offset_of(end - 1) + 1
            } else {
                offset
            };
            return Ok(Check::Fired(Box::new(Waited {
                matched: Some(Match {
                    text: String::from_utf8_lossy(&self.plain.bytes[start..end]).into_owned(),
                    offset,
                }),
                ..fired(WaitReason::Matched, at(s, after))
            })));
        }
        if self.facts.prompt
            && let Some(live) = live
            && let Some(&offset) = live.prompts.iter().find(|&&o| o > self.after.offset)
        {
            return Ok(Check::Fired(Box::new(fired(
                WaitReason::Prompt,
                at(s, offset),
            ))));
        }
        if let Some((states, id)) = &self.facts.status
            && let Some(record) = s
                .records
                .resolved_since(self.status_since)
                .into_iter()
                .find(|r| states.contains(&r.state) && id.as_ref().is_none_or(|id| *id == r.id))
        {
            return Ok(Check::Fired(Box::new(Waited {
                record: Some(record),
                ..fired(WaitReason::Status, cursor_of(s))
            })));
        }
        // Output that came after the scan is scanned before anything
        // later in the stream counts.
        if self.facts.output.is_some()
            && let Some(live) = live
            && self.scanned < live.stream.offset()
        {
            return Ok(Check::Pending(Some(Instant::now())));
        }
        let ended = s.exited_at.is_some() || live.is_none();
        if self.facts.exited && ended {
            return Ok(Check::Fired(Box::new(Waited {
                exit: s.exit.clone(),
                ..fired(WaitReason::Exited, cursor_of(s))
            })));
        }
        if ended && (self.facts.output.is_some() || self.facts.prompt || self.facts.quiet.is_some())
        {
            return Err((
                ErrorCode::NotRunning,
                "the stream has ended and nothing matched".into(),
            ));
        }
        if let (Some(quiet), Some(live)) = (self.facts.quiet, live) {
            let due = self.started.max(live.last_output) + quiet;
            if Instant::now() >= due {
                return Ok(Check::Fired(Box::new(fired(
                    WaitReason::Quiet,
                    cursor_of(s),
                ))));
            }
            return Ok(Check::Pending(Some(due)));
        }
        Ok(Check::Pending(None))
    }
}

/// The cursor at `offset`: the latest marker at or before it.
fn at(s: &Session, offset: u64) -> StreamCursor {
    match &s.body {
        Body::Live(live) => StreamCursor {
            offset,
            marker: live.stream.seq_at(offset),
        },
        Body::Parked(_) => cursor_of(s),
    }
}

/// Runs `waiter` until a fact holds, `timeout` passes (a result too) or
/// a check fails; woken by every change of the session's stream.
pub async fn run(
    session: &Shared,
    mut waiter: Waiter,
    timeout: Option<Duration>,
) -> Result<Waited, Fail> {
    let deadline = timeout.map(|t| waiter.started + t);
    let wake = session.lock().unwrap().wake.clone();
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let new = waiter.unscanned(&session.lock().unwrap());
        let found = waiter.scan(new);
        let next = {
            let s = session.lock().unwrap();
            match waiter.check(&s, found)? {
                Check::Fired(waited) => return Ok(*waited),
                Check::Pending(_) if deadline.is_some_and(|d| Instant::now() >= d) => {
                    return Ok(Waited {
                        reason: WaitReason::Timeout,
                        at: cursor_of(&s),
                        exit: None,
                        matched: None,
                        record: None,
                    });
                }
                Check::Pending(next) => [next, deadline].into_iter().flatten().min(),
            }
        };
        match next {
            Some(next) => {
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep_until(next) => {}
                }
            }
            None => notified.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(chunks: &[&[u8]]) -> Plain {
        let mut p = Plain::new();
        let mut offset = 100;
        for chunk in chunks {
            p.push(offset, chunk);
            offset += chunk.len() as u64;
        }
        p
    }

    #[test]
    fn plain_text_drops_sequences_and_keeps_offsets() {
        let p = plain(&[
            b"a\x1b[31mb\x1b]0;title\x07c\r\n",
            b"\x1b[",
            b"0md\x1bP+q\x1b\\e",
        ]);
        assert_eq!(p.bytes, b"abc\nde");
        // a at 100, b after the 5-byte CSI, c after the 10-byte OSC.
        assert_eq!(p.offset_of(0), 100);
        assert_eq!(p.offset_of(1), 106);
        assert_eq!(p.offset_of(2), 117);
        assert_eq!(p.offset_of(3), 119);
        // d follows "\x1b[" + "0m" split across chunks.
        assert_eq!(p.offset_of(4), 124);
    }

    #[test]
    fn a_text_needle_is_found_across_chunks_once() {
        let mut p = plain(&[b"xxpi"]);
        let needle = Needle::Text(b"ping".to_vec());
        assert_eq!(p.find(&needle, 0), None);
        p.push(104, b"ng!");
        assert_eq!(p.find(&needle, 4), Some((2, 6)));
    }

    #[test]
    fn dropping_old_text_keeps_offsets() {
        let mut p = Plain::new();
        p.push(0, &vec![b'a'; MAX_PLAIN]);
        p.push(MAX_PLAIN as u64, b"\x1b[mzz");
        p.trim();
        assert!(p.bytes.len() <= MAX_PLAIN / 2 + 2);
        let z = p.bytes.len() - 2;
        assert_eq!(p.offset_of(z), MAX_PLAIN as u64 + 3);
        assert_eq!(p.offset_of(0), MAX_PLAIN as u64 - (z as u64));
    }
}
