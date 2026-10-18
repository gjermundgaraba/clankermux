//! The 10-byte frame header and the raw payload shapes.

pub const HEADER_LEN: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameType {
    Hello = 0x01,
    Request = 0x02,
    Open = 0x10,
    Input = 0x11,
    Viewport = 0x12,
    Close = 0x13,
    Welcome = 0x81,
    Reply = 0x82,
    Event = 0x83,
    Error = 0x8F,
    Attached = 0x90,
    Snapshot = 0x91,
    SnapshotReady = 0x92,
    SnapshotEnd = 0x93,
    Output = 0x94,
    Resized = 0x95,
    Exited = 0x96,
    Gap = 0x97,
    Closed = 0x99,
}

impl FrameType {
    pub fn from_u8(byte: u8) -> Option<Self> {
        use FrameType::*;
        Some(match byte {
            0x01 => Hello,
            0x02 => Request,
            0x10 => Open,
            0x11 => Input,
            0x12 => Viewport,
            0x13 => Close,
            0x81 => Welcome,
            0x82 => Reply,
            0x83 => Event,
            0x8F => Error,
            0x90 => Attached,
            0x91 => Snapshot,
            0x92 => SnapshotReady,
            0x93 => SnapshotEnd,
            0x94 => Output,
            0x95 => Resized,
            0x96 => Exited,
            0x97 => Gap,
            0x99 => Closed,
            _ => return None,
        })
    }

    /// Frames the server sends have the high bit set.
    pub fn is_server(self) -> bool {
        self as u8 & 0x80 != 0
    }

    /// Whether the type belongs on channel 0 (`Error` belongs on either).
    pub fn is_control(self) -> bool {
        use FrameType::*;
        matches!(self, Hello | Request | Welcome | Reply | Event | Error)
    }

    /// Whether the type belongs on an attachment channel.
    pub fn is_attachment(self) -> bool {
        !self.is_control() || self == FrameType::Error
    }

    pub fn is_json(self) -> bool {
        use FrameType::*;
        !matches!(
            self,
            Input | Close | Snapshot | SnapshotReady | SnapshotEnd | Output
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub frame_type: FrameType,
    pub channel: u32,
    pub length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    UnknownType(u8),
    TooLong { length: u32, max_frame: u32 },
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::UnknownType(t) => write!(f, "unknown frame type 0x{t:02x}"),
            HeaderError::TooLong { length, max_frame } => {
                write!(f, "frame length {length} exceeds max_frame {max_frame}")
            }
        }
    }
}

impl std::error::Error for HeaderError {}

pub fn encode_header(frame_type: FrameType, channel: u32, length: u32) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0] = frame_type as u8;
    out[2..6].copy_from_slice(&channel.to_le_bytes());
    out[6..10].copy_from_slice(&length.to_le_bytes());
    out
}

/// Parses a header, ignoring `flags`.
pub fn parse_header(bytes: &[u8; HEADER_LEN], max_frame: u32) -> Result<Header, HeaderError> {
    let frame_type = FrameType::from_u8(bytes[0]).ok_or(HeaderError::UnknownType(bytes[0]))?;
    let channel = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    let length = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    if length > max_frame {
        return Err(HeaderError::TooLong { length, max_frame });
    }
    Ok(Header {
        frame_type,
        channel,
        length,
    })
}

/// A complete frame: header followed by the payload.
pub fn encode_frame(frame_type: FrameType, channel: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&encode_header(frame_type, channel, payload.len() as u32));
    out.extend_from_slice(payload);
    out
}

/// Splits a byte stream into frames.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    pos: usize,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, if buffered. A header error poisons the
    /// stream: the caller closes the connection.
    pub fn next_frame(&mut self, max_frame: u32) -> Result<Option<(Header, Vec<u8>)>, HeaderError> {
        let Some(head) = self.buf.get(self.pos..self.pos + HEADER_LEN) else {
            self.compact();
            return Ok(None);
        };
        let header = parse_header(head.try_into().unwrap(), max_frame)?;
        let start = self.pos + HEADER_LEN;
        let end = start + header.length as usize;
        if end > self.buf.len() {
            self.compact();
            return Ok(None);
        }
        let payload = self.buf[start..end].to_vec();
        self.pos = end;
        Ok(Some((header, payload)))
    }

    fn compact(&mut self) {
        self.buf.drain(..self.pos);
        self.pos = 0;
    }
}

/// `OUTPUT` payload: the stream offset of the first byte, then the bytes.
pub fn encode_output(offset: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + bytes.len());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn parse_output(payload: &[u8]) -> Option<(u64, &[u8])> {
    let (head, bytes) = payload.split_at_checked(8)?;
    Some((u64::from_le_bytes(head.try_into().unwrap()), bytes))
}
