#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Vt {
    #[default]
    Ground,
    Escape,
    Intermediate,
    Csi,
    ControlString,
    ControlStringEscape,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Feed {
    Text,
    Escape,
    Sequence,
}

impl Vt {
    pub(crate) fn feed(&mut self, byte: u8) -> Feed {
        const BEL: u8 = 0x07;
        const CAN: u8 = 0x18;
        const SUB: u8 = 0x1a;
        const ESC: u8 = 0x1b;
        let (next, feed) = match *self {
            Vt::Ground if byte == ESC => (Vt::Escape, Feed::Escape),
            Vt::Ground => (Vt::Ground, Feed::Text),
            Vt::Escape => match byte {
                b'[' => (Vt::Csi, Feed::Sequence),
                b']' | b'P' | b'X' | b'^' | b'_' => (Vt::ControlString, Feed::Sequence),
                0x20..=0x2f => (Vt::Intermediate, Feed::Sequence),
                ESC => (Vt::Escape, Feed::Sequence),
                0x30..=0x7e | CAN | SUB => (Vt::Ground, Feed::Sequence),
                _ => (Vt::Ground, Feed::Text),
            },
            Vt::Intermediate => match byte {
                0x20..=0x2f => (Vt::Intermediate, Feed::Sequence),
                ESC => (Vt::Escape, Feed::Sequence),
                0x30..=0x7e | CAN | SUB => (Vt::Ground, Feed::Sequence),
                _ => (Vt::Ground, Feed::Text),
            },
            Vt::Csi => match byte {
                ESC => (Vt::Escape, Feed::Sequence),
                b'\n' | b'\r' | b'\t' => (Vt::Csi, Feed::Text),
                0x20..=0x3f | 0x00..=0x17 | 0x19 | 0x1c..=0x1f => (Vt::Csi, Feed::Sequence),
                _ => (Vt::Ground, Feed::Sequence),
            },
            Vt::ControlString => match byte {
                BEL | CAN | SUB => (Vt::Ground, Feed::Sequence),
                ESC => (Vt::ControlStringEscape, Feed::Sequence),
                _ => (Vt::ControlString, Feed::Sequence),
            },
            Vt::ControlStringEscape if byte == b'\\' => (Vt::Ground, Feed::Sequence),
            Vt::ControlStringEscape => {
                *self = Vt::Escape;
                return self.feed(byte);
            }
        };
        *self = next;
        feed
    }
}
