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

/// Classifies the start of `buf`. With `strings`, `ESC ]`, `ESC P`,
/// `ESC X`, `ESC ^` and `ESC _` introduce OSC, DCS, SOS, PM and APC
/// strings that run to their terminator; without, they are two-byte
/// sequences, as the alt-keys a terminal sends them for are.
pub fn scan(buf: &[u8], strings: bool) -> Scan {
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
        b']' | b'P' | b'X' | b'^' | b'_' if strings => {
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

/// The length of the longest prefix of `buf` made of whole sequences,
/// whole UTF-8 characters and plain bytes; the rest has not fully arrived.
pub fn complete_prefix(buf: &[u8], strings: bool) -> usize {
    let mut pos = 0;
    while pos < buf.len() {
        match scan(&buf[pos..], strings) {
            Scan::Sequence(n) => pos += n,
            Scan::Plain(n) => {
                let tail = utf8_tail(&buf[pos..pos + n]);
                pos += n - tail;
                if tail > 0 {
                    break;
                }
            }
            Scan::Incomplete => break,
        }
    }
    pos
}

/// Bytes at the end of `bytes` that begin a UTF-8 character without
/// completing it.
fn utf8_tail(bytes: &[u8]) -> usize {
    let Some(start) = bytes
        .iter()
        .rposition(|&b| b & 0xc0 != 0x80)
        .filter(|&i| bytes.len() - i < 4)
    else {
        return 0;
    };
    let lead = bytes[start];
    let need = match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    };
    if bytes.len() - start < need {
        bytes.len() - start
    } else {
        0
    }
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
    /// By the first byte after the introducer: `G` for kitty graphics.
    Apc(Option<u8>),
}

impl Shape {
    /// Whether the response is a string (OSC, DCS or APC) rather than a
    /// CSI sequence.
    pub fn is_string(&self) -> bool {
        !matches!(self, Shape::Csi { .. })
    }
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
        b'_' => Some(Shape::Apc(seq.get(2).copied())),
        _ => None,
    }
}

/// A kitty keyboard protocol key press or repeat `CSI code ; mods [:
/// event] u`: the Unicode key code and the modifier bits (shift 1, alt
/// 2, ctrl 4, super 8), as the terminal sends them when a program
/// enabled the protocol. A release (event 3) is `None`.
pub fn kitty_key(seq: &[u8]) -> Option<(u32, u32)> {
    let body = seq.strip_prefix(b"\x1b[")?.strip_suffix(b"u")?;
    let body = std::str::from_utf8(body).ok()?;
    let mut fields = body.split(';');
    let code = fields.next()?.split(':').next()?.parse().ok()?;
    let mods = match fields.next() {
        Some(mods) => {
            let mut parts = mods.split(':');
            let mods = parts.next()?.parse::<u32>().ok()?.saturating_sub(1);
            if parts.next() == Some("3") {
                return None;
            }
            mods
        }
        None => 0,
    };
    Some((code, mods))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_sequences() {
        assert_eq!(scan(b"abc\x1b[A", true), Scan::Plain(3));
        assert_eq!(scan(b"\x1b[1;2R", true), Scan::Sequence(6));
        assert_eq!(scan(b"\x1b[1;2", true), Scan::Incomplete);
        assert_eq!(scan(b"\x1b", true), Scan::Incomplete);
        assert_eq!(scan(b"\x1bOP", true), Scan::Sequence(3));
        assert_eq!(scan(b"\x1ba", true), Scan::Sequence(2));
        assert_eq!(
            scan(b"\x1b]11;rgb:00/00/00\x1b\\x", true),
            Scan::Sequence(19)
        );
        assert_eq!(scan(b"\x1b]11;rgb:00/00/00\x07", true), Scan::Sequence(18));
        assert_eq!(scan(b"\x1bP>|ghostty\x1b", true), Scan::Incomplete);
        assert_eq!(scan(b"\x1b_Gi=1;OK\x1b\\", true), Scan::Sequence(11));
        assert_eq!(complete_prefix(b"ab\x1b[A\x1b[", true), 5);
        assert_eq!(complete_prefix(b"\x1b", true), 0);
    }

    #[test]
    fn alt_keys_are_not_string_introducers_unless_expected() {
        // Alt-] then "abc" and Enter: whole keys when no string answer is
        // expected, one unfinished OSC otherwise.
        let typed = b"\x1b]abc\r";
        assert_eq!(scan(typed, false), Scan::Sequence(2));
        assert_eq!(complete_prefix(typed, false), typed.len());
        assert_eq!(complete_prefix(typed, true), 0);
        assert_eq!(scan(b"\x1b_", false), Scan::Sequence(2));
    }

    #[test]
    fn unfinished_utf8_waits() {
        assert_eq!(complete_prefix("hé".as_bytes(), false), 3);
        assert_eq!(complete_prefix(&"hé".as_bytes()[..2], false), 1);
        assert_eq!(complete_prefix(&"漢".as_bytes()[..2], false), 0);
        assert_eq!(complete_prefix(b"a\x1b[A", false), 4);
        assert_eq!(complete_prefix(b"\xff\xfe", false), 2, "invalid bytes pass");
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
        assert_eq!(
            shape(b"\x1b_Gi=31;OK\x1b\\"),
            shape(b"\x1b_Gi=2;EINVAL\x1b\\")
        );
        assert!(shape(b"\x1b_Gi=31;OK\x1b\\").unwrap().is_string());
        assert_eq!(shape(b"\x1bOP"), None);
        assert_eq!(shape(b"plain"), None);
    }

    #[test]
    fn kitty_keys_decode() {
        assert_eq!(kitty_key(b"\x1b[92;5u"), Some((92, 4)));
        assert_eq!(kitty_key(b"\x1b[100u"), Some((100, 0)));
        assert_eq!(kitty_key(b"\x1b[100;1:1u"), Some((100, 0)));
        assert_eq!(kitty_key(b"\x1b[100;5:2u"), Some((100, 4)), "repeat");
        assert_eq!(kitty_key(b"\x1b[100;5:3u"), None, "release");
        assert_eq!(kitty_key(b"\x1b[97:65;2u"), Some((97, 1)));
        assert_eq!(kitty_key(b"\x1b[A"), None);
        assert_eq!(kitty_key(b"\x1b[1;2R"), None);
    }
}
