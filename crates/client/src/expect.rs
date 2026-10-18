//! The handler of a client's local engine: it records the answers the
//! server's terminal gives to the program's queries, so that the answers
//! an outer terminal produces to the same queries (passed through) can
//! be dropped instead of forwarded as input.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ghostty_vt::Handler;

use crate::vt::{self, Scan, Shape};

/// How long an expected answer stays armed.
const EXPIRY: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Expect {
    /// Whether output is passed to an outer terminal, which then answers
    /// the queries in it. Only then are answers expected.
    pub passthrough: bool,
    expected: VecDeque<(Shape, Instant)>,
}

impl Handler for Expect {
    fn write_pty(&mut self, data: &[u8]) {
        if !self.passthrough {
            return;
        }
        let now = Instant::now();
        let mut pos = 0;
        while pos < data.len() {
            match vt::scan(&data[pos..]) {
                Scan::Sequence(n) => {
                    if let Some(shape) = vt::shape(&data[pos..pos + n]) {
                        self.expected.push_back((shape, now));
                    }
                    pos += n;
                }
                Scan::Plain(n) => pos += n,
                Scan::Incomplete => break,
            }
        }
    }
}

impl Expect {
    pub fn new(passthrough: bool) -> Self {
        Expect {
            passthrough,
            expected: VecDeque::new(),
        }
    }

    /// Removes from `input` (whole sequences only) the responses that an
    /// outer terminal gave to queries answered by the server.
    pub fn filter(&mut self, input: &[u8]) -> Vec<u8> {
        let now = Instant::now();
        self.expected.retain(|(_, at)| now - *at < EXPIRY);
        if self.expected.is_empty() {
            return input.to_vec();
        }
        let mut out = Vec::with_capacity(input.len());
        let mut pos = 0;
        while pos < input.len() {
            match vt::scan(&input[pos..]) {
                Scan::Sequence(n) => {
                    let seq = &input[pos..pos + n];
                    let hit = vt::shape(seq)
                        .and_then(|shape| self.expected.iter().position(|(e, _)| *e == shape));
                    match hit {
                        Some(i) => {
                            self.expected.remove(i);
                        }
                        None => out.extend_from_slice(seq),
                    }
                    pos += n;
                }
                Scan::Plain(n) => {
                    out.extend_from_slice(&input[pos..pos + n]);
                    pos += n;
                }
                Scan::Incomplete => {
                    out.extend_from_slice(&input[pos..]);
                    break;
                }
            }
        }
        out
    }

    pub fn pending(&self) -> usize {
        self.expected.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_one_answer_of_each_expected_shape() {
        let mut e = Expect::new(true);
        e.write_pty(b"\x1b[?62;22c\x1b[24;80R");
        assert_eq!(e.pending(), 2);
        // The outer terminal answers differently, in order; a key press
        // between them is kept.
        let kept = e.filter(b"\x1b[?1;2c" as &[u8]);
        assert_eq!(kept, b"");
        let kept = e.filter(b"x\x1b[10;5Ry\x1b[10;5R");
        assert_eq!(kept, b"xy\x1b[10;5R", "the second CPR is a key");
        assert_eq!(e.pending(), 0);
    }

    #[test]
    fn nothing_expected_without_passthrough() {
        let mut e = Expect::default();
        e.write_pty(b"\x1b[?62c");
        assert_eq!(e.pending(), 0);
        assert_eq!(e.filter(b"\x1b[1;2R"), b"\x1b[1;2R");
    }
}
