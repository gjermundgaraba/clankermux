use ghostty_vt::ScrollbackRow;
use ghostty_vt::formatter::{self, Format, Options, Region, VtExtras};
use ghostty_vt::{Term, Terminal};
use proto::{ErrorCode, ReadFormat, ReadParams, ReadScope, TailParams};

use crate::registry::{Fail, Registry, internal};
use crate::session::{Body, Session, blocking};

pub const REPLY_MARGIN: usize = 4096;

const TAIL_MAX_BYTES: u64 = 64 * 1024;

fn json_len(text: &str) -> usize {
    serde_json::to_string(text).map_or(usize::MAX, |s| s.len() - 2)
}

fn fit_json(text: &mut String, budget: usize) -> bool {
    if json_len(text) <= budget {
        return false;
    }
    let mut kept = 0;
    let mut start = text.len();
    for line in text.split_inclusive('\n').rev() {
        let len = json_len(line);
        if kept + len > budget {
            if kept == 0 {
                for (at, ch) in line.char_indices().rev() {
                    let len = json_len(ch.encode_utf8(&mut [0; 4]));
                    if kept + len > budget {
                        break;
                    }
                    kept += len;
                    start = text.len() - line.len() + at;
                }
            }
            break;
        }
        kept += len;
        start -= line.len();
    }
    text.drain(..start);
    true
}

fn unavailable(message: &str) -> Fail {
    (ErrorCode::ScopeUnavailable, message.into())
}

fn text_lines(text: &str) -> String {
    let mut out = text.lines().collect::<Vec<_>>().join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[derive(Default)]
struct Text {
    text: String,
    truncated: bool,
}

fn format_rows(
    term: &Term,
    format: ReadFormat,
    from: ScrollbackRow,
    budget: usize,
) -> Result<Text, Fail> {
    let run = |row: ScrollbackRow| {
        formatter::format(
            term,
            Options {
                format: match format {
                    ReadFormat::Text => Format::Plain,
                    ReadFormat::Vt => Format::Vt(VtExtras {
                        hyperlink: true,
                        ..VtExtras::default()
                    }),
                    ReadFormat::Html => Format::Html,
                },
                unwrap: true,
                trim: true,
                region: Region::FromRow(row),
            },
        )
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(internal)
    };
    if format == ReadFormat::Text {
        let mut text = text_lines(&run(from)?);
        let truncated = fit_json(&mut text, budget);
        return Ok(Text { text, truncated });
    }
    let text = run(from)?;
    if json_len(&text) <= budget {
        return Ok(Text {
            text,
            truncated: false,
        });
    }
    let ScrollbackRow(screen_top) = term.screen_top().map_err(internal)?;
    let end = screen_top
        .checked_add(term.rows().into())
        .ok_or_else(|| internal(ghostty_vt::Error::LimitExceeded))?;
    let (mut low, mut high) = (from.0 + 1, end);
    while low < high {
        let mid = low + (high - low) / 2;
        if json_len(&run(ScrollbackRow(mid))?) <= budget {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    let text = if low >= end {
        String::new()
    } else {
        run(ScrollbackRow(low))?
    };
    Ok(Text {
        text,
        truncated: true,
    })
}

fn from_terminal(
    term: &Term,
    scope: ReadScope,
    format: ReadFormat,
    scrollback: usize,
    budget: usize,
) -> Result<Text, Fail> {
    let from = match scope {
        ReadScope::Screen => term.history_start(scrollback).map_err(internal)?,
        ReadScope::History => ScrollbackRow(0),
        ReadScope::Alternate if term.alternate_screen() => term.screen_top().map_err(internal)?,
        ReadScope::Alternate => {
            return Err(unavailable("the session is not on the alternate screen"));
        }
        ReadScope::LastCommand => unreachable!("read from the stream"),
    };
    format_rows(term, format, from, budget)
}

fn last_command_bytes(s: &Session) -> Result<(Vec<u8>, bool), Fail> {
    let command = s
        .commands
        .current()
        .ok_or_else(|| unavailable("no command output is marked (OSC 133 C)"))?;
    let output = command.output.unwrap_or(command.start);
    let Body::Live(live) = &s.body else {
        return Ok((Vec::new(), true));
    };
    let end = command.end.unwrap_or_else(|| live.stream.offset());
    let start = output.max(live.stream.retained_start());
    let bytes = live.stream.read(start, end).unwrap_or_default();
    Ok((bytes, start > output))
}

fn replay(
    bytes: &[u8],
    size: proto::Size,
    format: ReadFormat,
    scrollback_limit: usize,
    budget: usize,
) -> Result<Text, Fail> {
    let mut term = Terminal::new(size.cols, size.rows).map_err(internal)?;
    term.set_scrollback_max_bytes(scrollback_limit)
        .map_err(internal)?;
    term.vt_write(bytes);
    format_rows(&term, format, ScrollbackRow(0), budget)
}

pub fn read(registry: &Registry, p: &ReadParams) -> Result<proto::Read, Fail> {
    let format = p.format.unwrap_or_default();
    let scope = p.scope.unwrap_or_default();
    if p.scrollback.is_some() && scope != ReadScope::Screen {
        return Err((
            ErrorCode::InvalidRequest,
            "scrollback is for scope screen only".into(),
        ));
    }
    let scrollback = match scope {
        ReadScope::History => usize::MAX,
        _ => p.scrollback.unwrap_or(0) as usize,
    };
    let limits = &registry.config.limits;
    let budget = proto::cap(limits.max_frame).saturating_sub(REPLY_MARGIN);
    let session = registry.get(&p.session)?;
    let s = session.lock().unwrap();
    let info = s.to_proto(false);
    let reply = |Text { text, truncated }: Text| proto::Read {
        text,
        cols: info.cols,
        rows: info.rows,
        cursor: s.live().map(|live| {
            let at = live.term.cursor();
            proto::Cursor {
                x: at.col,
                y: at.row,
            }
        }),
        offset: info.offset,
        status: info.status,
        truncated,
    };
    if scope == ReadScope::LastCommand {
        let (bytes, gone) = last_command_bytes(&s)?;
        let size = s.size();
        let reply = reply(Text::default());
        drop(s);
        let Text { text, truncated } =
            blocking(|| replay(&bytes, size, format, proto::cap(limits.scrollback), budget))?;
        return Ok(proto::Read {
            text,
            truncated: truncated || gone,
            ..reply
        });
    }
    match &s.body {
        _ if format == ReadFormat::Text && scope != ReadScope::Alternate => {
            let mut text = s.read_text(scrollback).map_err(internal)?;
            if !text.is_empty() {
                text.push('\n');
            }
            let truncated = fit_json(&mut text, budget);
            Ok(reply(Text { text, truncated }))
        }
        Body::Live(live) => {
            blocking(|| from_terminal(&live.term, scope, format, scrollback, budget)).map(reply)
        }
        Body::Parked(parked) => {
            if parked.engine != ghostty_vt::ENGINE {
                return Err((
                    ErrorCode::EngineMismatch,
                    format!(
                        "parked by {}; only text from the screen and history is served",
                        parked.engine
                    ),
                ));
            }
            let path = parked.snapshot_path.clone();
            let reply = reply(Text::default());
            drop(s);
            let read = blocking(|| {
                let bytes = std::fs::read(&path).map_err(internal)?;
                let term = ghostty_vt::snapshot::decode(&bytes).map_err(internal)?;
                from_terminal(&term, scope, format, scrollback, budget)
            })?;
            Ok(proto::Read {
                text: read.text,
                truncated: read.truncated,
                ..reply
            })
        }
    }
}

pub fn tail(registry: &Registry, p: &TailParams) -> Result<proto::Tail, Fail> {
    let max = p.max_bytes.unwrap_or(TAIL_MAX_BYTES);
    if max == 0 {
        return Err((
            ErrorCode::InvalidRequest,
            "max_bytes must be at least 1".into(),
        ));
    }
    let budget = proto::cap(registry.config.limits.max_frame).saturating_sub(REPLY_MARGIN);
    let session = registry.get(&p.session)?;
    let s = session.lock().unwrap();
    let past = || {
        (
            ErrorCode::InvalidRequest,
            "after is past the end of the stream".to_string(),
        )
    };
    let Body::Live(live) = &s.body else {
        let end = s.cursor().offset;
        if p.after > end {
            return Err(past());
        }
        return Ok(proto::Tail {
            text: String::new(),
            start_offset: end,
            next_offset: end,
            complete: true,
            truncated: p.after < end,
        });
    };
    let stream = &live.stream;
    let end = stream.offset();
    if p.after > end {
        return Err(past());
    }
    let start = p.after.max(stream.retained_start());
    let previous = start
        .checked_sub(1)
        .and_then(|at| stream.read(at, at + 1))
        .and_then(|b| b.first().copied());
    let bytes = stream.bytes_from(start);
    let mut filter = Filter::new(previous, start > p.after);
    let Ran {
        consumed,
        unfinished,
    } = blocking(|| filter.run(bytes, proto::cap(max), budget, s.exited_at.is_some()));
    let next = start + consumed as u64;
    Ok(proto::Tail {
        text: filter.out,
        start_offset: start,
        next_offset: next,
        complete: next == end || unfinished,
        truncated: start > p.after,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Ran {
    consumed: usize,
    unfinished: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    EscIntermediate,
    Csi,
    String,
    StringEsc,
}

struct Filter {
    out: String,
    state: State,
    utf8: Vec<u8>,
    previous: Option<u8>,
    skip_cut: bool,
    json: usize,
}

impl Filter {
    fn new(previous: Option<u8>, cut: bool) -> Self {
        Filter {
            out: String::new(),
            state: State::Ground,
            utf8: Vec::new(),
            previous,
            skip_cut: cut,
            json: 0,
        }
    }

    fn run(
        &mut self,
        bytes: impl IntoIterator<Item = u8>,
        max: usize,
        budget: usize,
        ended: bool,
    ) -> Ran {
        let mut boundary = 0;
        let mut len = 0;
        for (i, byte) in bytes.into_iter().enumerate() {
            len = i + 1;
            if self.state == State::Ground && self.utf8.is_empty() {
                boundary = i;
                if i >= max {
                    return Ran {
                        consumed: i,
                        unfinished: false,
                    };
                }
            }
            let mark = (self.out.len(), self.json);
            self.byte(byte);
            if self.json > budget {
                self.out.truncate(mark.0);
                self.json = mark.1;
                return Ran {
                    consumed: boundary,
                    unfinished: false,
                };
            }
        }
        if self.state == State::Ground && self.utf8.is_empty() {
            return Ran {
                consumed: len,
                unfinished: false,
            };
        }
        if ended {
            if !self.utf8.is_empty() {
                self.utf8.clear();
                if self.json + 3 > budget {
                    return Ran {
                        consumed: boundary,
                        unfinished: false,
                    };
                }
                self.emit('\u{fffd}');
            }
            return Ran {
                consumed: len,
                unfinished: false,
            };
        }
        Ran {
            consumed: boundary,
            unfinished: true,
        }
    }

    fn emit(&mut self, ch: char) {
        self.json += match ch {
            '"' | '\\' | '\n' | '\t' => 2,
            _ => ch.len_utf8(),
        };
        self.out.push(ch);
    }

    fn byte(&mut self, byte: u8) {
        match self.state {
            State::Ground => self.ground(byte),
            State::Esc => match byte {
                b'[' => self.state = State::Csi,
                b']' | b'P' | b'X' | b'^' | b'_' => self.state = State::String,
                0x20..=0x2f => self.state = State::EscIntermediate,
                0x1b => {}
                0x30..=0x7e | 0x18 | 0x1a => self.state = State::Ground,
                _ => {
                    self.state = State::Ground;
                    self.ground(byte);
                }
            },
            State::EscIntermediate => match byte {
                0x20..=0x2f => {}
                0x1b => self.state = State::Esc,
                0x30..=0x7e | 0x18 | 0x1a => self.state = State::Ground,
                _ => {
                    self.state = State::Ground;
                    self.ground(byte);
                }
            },
            State::Csi => match byte {
                0x1b => self.state = State::Esc,
                // The terminal carries these out inside a sequence too.
                b'\n' | b'\r' | b'\t' => self.ground(byte),
                0x20..=0x3f | 0x00..=0x17 | 0x19 | 0x1c..=0x1f => {}
                _ => self.state = State::Ground,
            },
            State::String => match byte {
                0x07 | 0x18 | 0x1a => self.state = State::Ground,
                0x1b => self.state = State::StringEsc,
                _ => {}
            },
            State::StringEsc => {
                if byte == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::Esc;
                    self.byte(byte);
                }
            }
        }
    }

    fn ground(&mut self, byte: u8) {
        if !self.utf8.is_empty() {
            if (0x80..=0xbf).contains(&byte) {
                self.utf8.push(byte);
                let need = match self.utf8[0] {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    _ => 4,
                };
                if self.utf8.len() == need {
                    let ch = std::str::from_utf8(&self.utf8)
                        .ok()
                        .and_then(|s| s.chars().next())
                        .unwrap_or('\u{fffd}');
                    self.utf8.clear();
                    if !('\u{80}'..='\u{9f}').contains(&ch) {
                        self.emit(ch);
                    }
                }
                return;
            }
            self.utf8.clear();
            self.emit('\u{fffd}');
        }
        if self.skip_cut {
            match byte {
                b'\n' => {
                    self.skip_cut = false;
                    self.previous = Some(byte);
                    return;
                }
                0x1b => self.skip_cut = false,
                _ => return,
            }
        }
        let previous = self.previous;
        if byte != 0x1b {
            self.previous = Some(byte);
        }
        match byte {
            0x1b => self.state = State::Esc,
            b'\n' if previous == Some(b'\r') => {}
            b'\n' | b'\r' => self.emit('\n'),
            b'\t' => self.emit('\t'),
            0x00..=0x1f | 0x7f => {}
            0x20..=0x7e => self.emit(byte as char),
            0xc2..=0xf4 => self.utf8.push(byte),
            _ => self.emit('\u{fffd}'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_keeps_the_newest_lines_or_the_tail_of_one() {
        let mut text = "aaaa\nbb\ncc\n".to_string();
        assert!(fit_json(&mut text, 8));
        assert_eq!(text, "bb\ncc\n");
        let mut text = "xxxxxxxxxx\n".to_string();
        fit_json(&mut text, 4);
        assert_eq!(text, "xx\n");
        let mut quoted = "a\"b\n".to_string();
        fit_json(&mut quoted, 3);
        assert_eq!(quoted, "b\n");
        let mut fits = "ok\n".to_string();
        assert!(!fit_json(&mut fits, 4));
    }

    fn filter(bytes: &[u8], max: usize, ended: bool) -> (String, usize, bool) {
        let mut f = Filter::new(None, false);
        let Ran {
            consumed,
            unfinished,
        } = f.run(bytes.iter().copied(), max, usize::MAX, ended);
        (f.out, consumed, unfinished)
    }

    #[test]
    fn the_filter_keeps_text_and_drops_sequences_and_controls() {
        let bytes = b"\x1b[1;31mred\x1b[0m\r\nnext\x1b]0;title\x07\x1b]8;;http://x\x1b\\link\x1b(B\x07\x08\ttab\rover\n";
        assert_eq!(
            filter(bytes, usize::MAX, false),
            ("red\nnextlink\ttab\nover\n".into(), bytes.len(), false)
        );
        assert_eq!(filter("é\u{9b}x\x7f".as_bytes(), 99, false).0, "éx");
        assert_eq!(filter(b"a\xffb", 99, false).0, "a\u{fffd}b");
        let mut f = Filter::new(Some(b'\r'), false);
        f.run(*b"\nx", 99, usize::MAX, false);
        assert_eq!(f.out, "x");
        assert_eq!(filter(b"50%\r\x1b[K\ndone", 99, false).0, "50%\ndone");
        assert_eq!(filter(b"a\x1b[\n1mb", 99, false).0, "a\nb");
    }

    #[test]
    fn the_filter_stops_at_boundaries() {
        assert_eq!(filter(b"ab\x1b]0;ti", 99, false), ("ab".into(), 2, true));
        assert_eq!(filter(b"ab\x1b]0;ti", 99, true), ("ab".into(), 8, false));
        assert_eq!(filter(b"ab\xc3", 99, false), ("ab".into(), 2, true));
        assert_eq!(filter(b"ab\xc3", 99, true), ("ab\u{fffd}".into(), 3, false));
        assert_eq!(filter(b"abcdef", 3, false), ("abc".into(), 3, false));
        assert_eq!(filter(b"ab\x1b[31mcd", 3, false), ("ab".into(), 7, false));
        assert_eq!(
            filter(b"\x1b]52;c;aGVsbG8=\x07x", 2, false),
            (String::new(), 16, false)
        );
        let mut f = Filter::new(None, false);
        assert_eq!(
            f.run(*b"ab\"c", 99, 3, false),
            Ran {
                consumed: 2,
                unfinished: false
            }
        );
        assert_eq!(f.out, "ab");
        for (cut, text) in [
            (&b"\xa9 rest\nnext\n"[..], "next\n"),
            (&b"aGVsbG8=\x07title\x1b[1mbold"[..], "bold"),
        ] {
            let mut f = Filter::new(None, true);
            f.run(cut.iter().copied(), 99, usize::MAX, false);
            assert_eq!(f.out, text);
        }
    }
}
