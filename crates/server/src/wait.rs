use std::time::Duration;

use proto::{CursorParams, ErrorCode, Match, StreamCursor, Until, WaitReason, Waited};
use regex::bytes::Regex;
use tokio::time::Instant;

use crate::records::STATES;
use crate::registry::Fail;
use crate::session::{Body, Session, Shared};

const MAX_PLAIN: usize = 256 * 1024;

const REGEX_REACH: usize = 64 * 1024;

enum Needle {
    Text(Vec<u8>),
    Regex(Regex),
}

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

pub fn anchor(s: &Session, after: Option<CursorParams>) -> Result<StreamCursor, Fail> {
    let now = s.cursor();
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Vt {
    Ground,
    Escape,
    Intermediate,
    Csi,
    ControlString,
    ControlStringEscape,
}

struct Plain {
    vt: Vt,
    bytes: Vec<u8>,
    runs: Vec<(usize, u64)>,
    next_kept: Option<u64>,
    next_seen: Option<u64>,
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

    fn trim(&mut self) {
        if self.bytes.len() > MAX_PLAIN {
            self.drop_front(self.bytes.len() - MAX_PLAIN / 2);
        }
    }

    fn step(&mut self, b: u8) -> bool {
        const ESC: u8 = 0x1b;
        let abort = b == 0x18 || b == 0x1a;
        self.vt = match self.vt {
            Vt::Ground => match b {
                ESC => Vt::Escape,
                b'\n' | b'\t' | 0x20..=0x7e | 0x80..=0xff => return true,
                _ => Vt::Ground,
            },
            Vt::Escape => match b {
                b'[' => Vt::Csi,
                b']' | b'P' | b'X' | b'^' | b'_' => Vt::ControlString,
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
            Vt::ControlString => match b {
                _ if abort => Vt::Ground,
                0x07 => Vt::Ground,
                ESC => Vt::ControlStringEscape,
                _ => Vt::ControlString,
            },
            Vt::ControlStringEscape => {
                if b == b'\\' {
                    Vt::Ground
                } else {
                    self.vt = Vt::Escape;
                    return self.step(b);
                }
            }
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

    fn offset_of(&self, index: usize) -> u64 {
        let run = self.runs.partition_point(|&(i, _)| i <= index) - 1;
        let (start, offset) = self.runs[run];
        offset + (index - start) as u64
    }

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

pub struct Waiter {
    facts: Facts,
    after: StreamCursor,
    status_since: u64,
    scanned: u64,
    plain: Plain,
    started: Instant,
}

pub enum Check {
    Fired(Box<Waited>),
    Pending { recheck_at: Option<Instant> },
}

impl Waiter {
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

    pub fn for_send(facts: Facts, s: &Session) -> Self {
        Waiter {
            status_since: s.records.tick(),
            ..Waiter::new(facts, s.cursor())
        }
    }

    fn unscanned(&mut self, s: &Session) -> Option<(u64, Vec<u8>)> {
        self.facts.output.as_ref()?;
        let Body::Live(live) = &s.body else {
            return None;
        };
        self.scanned = self.scanned.max(live.stream.retained_start());
        let chunk = live.stream.read(self.scanned, u64::MAX)?;
        let at = self.scanned;
        self.scanned += chunk.len() as u64;
        Some((at, chunk))
    }

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
        let live = s.live();
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
                ..fired(WaitReason::Status, s.cursor())
            })));
        }
        if self.facts.output.is_some()
            && let Some(live) = live
            && self.scanned < live.stream.offset()
        {
            return Ok(Check::Pending {
                recheck_at: Some(Instant::now()),
            });
        }
        let ended = s.exited_at.is_some() || live.is_none();
        if self.facts.exited && ended {
            return Ok(Check::Fired(Box::new(Waited {
                exit: s.exit.clone(),
                ..fired(WaitReason::Exited, s.cursor())
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
                return Ok(Check::Fired(Box::new(fired(WaitReason::Quiet, s.cursor()))));
            }
            return Ok(Check::Pending {
                recheck_at: Some(due),
            });
        }
        Ok(Check::Pending { recheck_at: None })
    }
}

fn at(s: &Session, offset: u64) -> StreamCursor {
    match &s.body {
        Body::Live(live) => StreamCursor {
            offset,
            marker: live.stream.seq_at(offset),
        },
        Body::Parked(_) => s.cursor(),
    }
}

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
                Check::Fired(fired) => return Ok(*fired),
                Check::Pending { .. } if deadline.is_some_and(|d| Instant::now() >= d) => {
                    return Ok(Waited {
                        reason: WaitReason::Timeout,
                        at: s.cursor(),
                        exit: None,
                        matched: None,
                        record: None,
                    });
                }
                Check::Pending { recheck_at } => [recheck_at, deadline].into_iter().flatten().min(),
            }
        };
        match next {
            Some(next) => {
                tokio::select! {
                    () = notified => {}
                    () = tokio::time::sleep_until(next) => {}
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
        assert_eq!(p.offset_of(0), 100);
        assert_eq!(p.offset_of(1), 106);
        assert_eq!(p.offset_of(2), 117);
        assert_eq!(p.offset_of(3), 119);
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
