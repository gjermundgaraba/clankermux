//! Splitting terminal byte streams into whole escape sequences.

/// What the start of a buffer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scan {
    /// A complete escape sequence of this length.
    Sequence(usize),
    /// Plain bytes (no ESC) of this length.
    Plain(usize),
    /// An escape sequence that has not fully arrived.
    Incomplete,
}

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Classifies the start of `buf`.
pub fn scan(buf: &[u8]) -> Scan {
    let Some(&first) = buf.first() else {
        return Scan::Plain(0);
    };
    if first != ESC {
        let len = buf.iter().position(|&b| b == ESC).unwrap_or(buf.len());
        return Scan::Plain(len);
    }
    let Some(&kind) = buf.get(1) else {
        return Scan::Incomplete;
    };
    match kind {
        // CSI: parameters, intermediates, then a final byte.
        b'[' => {
            for (i, &b) in buf.iter().enumerate().skip(2) {
                match b {
                    0x20..=0x3f => {}
                    0x40..=0x7e => return Scan::Sequence(i + 1),
                    _ => return Scan::Sequence(i),
                }
            }
            Scan::Incomplete
        }
        // OSC ends with BEL or ST; DCS, SOS, PM and APC end with ST.
        b']' | b'P' | b'X' | b'^' | b'_' => {
            let mut i = 2;
            while i < buf.len() {
                match buf[i] {
                    BEL if kind == b']' => return Scan::Sequence(i + 1),
                    ESC => {
                        return match buf.get(i + 1) {
                            Some(b'\\') => Scan::Sequence(i + 2),
                            Some(_) => Scan::Sequence(i),
                            None => Scan::Incomplete,
                        };
                    }
                    _ => i += 1,
                }
            }
            Scan::Incomplete
        }
        // SS3.
        b'O' => {
            if buf.len() >= 3 {
                Scan::Sequence(3)
            } else {
                Scan::Incomplete
            }
        }
        _ => Scan::Sequence(2),
    }
}

/// The length of the longest prefix of `buf` made of whole sequences and
/// plain bytes; the rest is an incomplete sequence.
pub fn complete_prefix(buf: &[u8]) -> usize {
    let mut pos = 0;
    while pos < buf.len() {
        match scan(&buf[pos..]) {
            Scan::Sequence(n) | Scan::Plain(n) => pos += n,
            Scan::Incomplete => break,
        }
    }
    pos
}

/// The shape of a terminal response, enough to recognise another
/// terminal's answer to the same query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Csi {
        private: Option<u8>,
        intermediates: Vec<u8>,
        final_byte: u8,
    },
    Osc(Vec<u8>),
    Dcs(Vec<u8>),
}

/// The shape of one complete sequence, when it can be a response.
pub fn shape(seq: &[u8]) -> Option<Shape> {
    match seq.get(1)? {
        b'[' => {
            let body = &seq[2..];
            let (&final_byte, rest) = body.split_last()?;
            let private = rest.first().copied().filter(|b| (0x3c..=0x3f).contains(b));
            let intermediates = rest
                .iter()
                .copied()
                .filter(|b| (0x20..=0x2f).contains(b))
                .collect();
            Some(Shape::Csi {
                private,
                intermediates,
                final_byte,
            })
        }
        b']' => {
            let number = seq[2..]
                .iter()
                .copied()
                .take_while(u8::is_ascii_digit)
                .collect();
            Some(Shape::Osc(number))
        }
        b'P' => {
            // The leading digits distinguish valid from invalid replies
            // (XTGETTCAP), not the query; the bytes through the first
            // letter or symbol do.
            let mut prefix = Vec::new();
            for &b in seq[2..].iter().skip_while(|b| b.is_ascii_digit()) {
                prefix.push(b);
                if (0x40..=0x7e).contains(&b) {
                    break;
                }
            }
            Some(Shape::Dcs(prefix))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_sequences() {
        assert_eq!(scan(b"abc\x1b[A"), Scan::Plain(3));
        assert_eq!(scan(b"\x1b[1;2R"), Scan::Sequence(6));
        assert_eq!(scan(b"\x1b[1;2"), Scan::Incomplete);
        assert_eq!(scan(b"\x1b"), Scan::Incomplete);
        assert_eq!(scan(b"\x1bOP"), Scan::Sequence(3));
        assert_eq!(scan(b"\x1ba"), Scan::Sequence(2));
        assert_eq!(scan(b"\x1b]11;rgb:00/00/00\x1b\\x"), Scan::Sequence(19));
        assert_eq!(scan(b"\x1b]11;rgb:00/00/00\x07"), Scan::Sequence(18));
        assert_eq!(scan(b"\x1bP>|ghostty\x1b"), Scan::Incomplete);
        assert_eq!(complete_prefix(b"ab\x1b[A\x1b["), 5);
        assert_eq!(complete_prefix(b"\x1b"), 0);
    }

    #[test]
    fn shapes_ignore_parameters() {
        assert_eq!(shape(b"\x1b[?62;22c"), shape(b"\x1b[?1;2c"));
        assert_ne!(shape(b"\x1b[?62;22c"), shape(b"\x1b[>1;2c"));
        assert_eq!(shape(b"\x1b[12;40R"), shape(b"\x1b[1;1R"));
        assert_ne!(shape(b"\x1b[12;40R"), shape(b"\x1b[?12;40R"));
        assert_eq!(shape(b"\x1b[?2026;2$y"), shape(b"\x1b[?2026;0$y"));
        assert_eq!(
            shape(b"\x1b]11;rgb:1/2/3\x1b\\"),
            shape(b"\x1b]11;rgb:a/b/c\x07")
        );
        assert_ne!(shape(b"\x1b]11;x\x07"), shape(b"\x1b]10;x\x07"));
        assert_eq!(
            shape(b"\x1bP>|clankermux 1\x1b\\"),
            shape(b"\x1bP>|ghostty 1.3\x1b\\")
        );
        assert_eq!(shape(b"\x1bP1+r524742\x1b\\"), shape(b"\x1bP0+r\x1b\\"));
        assert_eq!(shape(b"\x1bOP"), None);
        assert_eq!(shape(b"plain"), None);
    }
}
