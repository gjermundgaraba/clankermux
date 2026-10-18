//! The handler of a client's local engine: it records the answers the
//! server's terminal gives to the program's queries, so that the answers
//! an outer terminal produces to the same queries (passed through) can
//! be dropped instead of forwarded as input.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ghostty_vt::{ColorScheme, Handler, SizeReport};

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
        let mut pos = 0;
        while pos < data.len() {
            match vt::scan(&data[pos..], true) {
                Scan::Sequence(n) => {
                    if let Some(shape) = vt::shape(&data[pos..pos + n]) {
                        self.arm(shape);
                    }
                    pos += n;
                }
                Scan::Plain(n) => pos += n,
                Scan::Incomplete => break,
            }
        }
    }

    /// The server answers size queries; the local engine stays silent
    /// but expects the outer terminal's `CSI … t`.
    fn size(&mut self) -> Option<SizeReport> {
        self.arm(Shape::Csi {
            private: None,
            intermediates: Vec::new(),
            final_byte: b't',
        });
        None
    }

    /// The server answers `CSI ? 996 n` from the profile; the outer
    /// terminal's `CSI ? 997 ; … n` is expected either way.
    fn color_scheme(&mut self) -> Option<ColorScheme> {
        self.arm(Shape::Csi {
            private: Some(b'?'),
            intermediates: Vec::new(),
            final_byte: b'n',
        });
        None
    }
}

impl Expect {
    fn arm(&mut self, shape: Shape) {
        if self.passthrough {
            self.expected.push_back((shape, Instant::now()));
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.expected.retain(|(_, at)| now - *at < EXPIRY);
    }

    /// Whether a string answer (OSC, DCS or APC) is expected, so that a
    /// string introducer on stdin starts one rather than being an alt-key.
    pub fn expects_string(&mut self) -> bool {
        self.expire();
        self.expected.iter().any(|(shape, _)| shape.is_string())
    }

    /// Removes from `input` (whole sequences only) the responses that an
    /// outer terminal gave to queries answered by the server.
    pub fn filter(&mut self, input: &[u8]) -> Vec<u8> {
        self.expire();
        if self.expected.is_empty() {
            return input.to_vec();
        }
        let strings = self.expected.iter().any(|(shape, _)| shape.is_string());
        let mut out = Vec::with_capacity(input.len());
        let mut pos = 0;
        while pos < input.len() {
            match vt::scan(&input[pos..], strings) {
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
}

#[cfg(test)]
mod tests {
    use ghostty_vt::{ColorKind, Rgb, Terminal};

    use super::*;

    fn passthrough() -> Terminal<Expect> {
        let mut term = Terminal::new(80, 24)
            .unwrap()
            .with_handler(Expect::default());
        // The engine answers OSC 11 only with a background set; a
        // session's snapshot carries one.
        term.set_color(ColorKind::Background, Some(Rgb { r: 0, g: 0, b: 0 }))
            .unwrap();
        term.handler_mut().passthrough = true;
        term
    }

    #[test]
    fn drops_the_outer_terminal_answers_to_what_the_engine_queried() {
        let mut term = passthrough();
        // DA1, CPR, two size queries, the colour scheme, a kitty graphics
        // query and OSC 11: the outer terminal answers each.
        term.vt_write(b"\x1b[c\x1b[6n\x1b[18t\x1b[14t\x1b[?996n\x1b_Gi=31,s=1,v=1,a=q;AAAA\x1b\\\x1b]11;?\x1b\\");
        let answers: &[&[u8]] = &[
            b"\x1b[?1;2c",
            b"\x1b[10;5R",
            b"\x1b[8;24;80t",
            b"\x1b[4;480;720t",
            b"\x1b[?997;1n",
            b"\x1b_Gi=31;OK\x1b\\",
            b"\x1b]11;rgb:1111/1111/1111\x1b\\",
        ];
        let e = term.handler_mut();
        for answer in answers {
            assert_eq!(
                e.filter(answer),
                b"",
                "{:?}",
                String::from_utf8_lossy(answer)
            );
        }
        // Each is dropped once; a second CPR is a key.
        assert_eq!(e.filter(b"x\x1b[10;5Ry"), b"x\x1b[10;5Ry");
    }

    #[test]
    fn string_answers_are_expected_only_while_armed() {
        let mut term = passthrough();
        assert!(!term.handler_mut().expects_string());
        term.vt_write(b"\x1b[>0q");
        assert!(term.handler_mut().expects_string());
        assert_eq!(term.handler_mut().filter(b"\x1bP>|outer 1\x1b\\"), b"");
        assert!(!term.handler_mut().expects_string());
    }

    #[test]
    fn nothing_expected_without_passthrough() {
        let mut term = Terminal::new(80, 24)
            .unwrap()
            .with_handler(Expect::default());
        term.vt_write(b"\x1b[c\x1b[18t");
        assert_eq!(
            term.handler_mut().filter(b"\x1b[?1;2c\x1b[8;24;80t"),
            b"\x1b[?1;2c\x1b[8;24;80t"
        );
    }
}
