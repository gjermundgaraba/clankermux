//! The 10-byte frame header and the raw payload shapes.

pub const HEADER_LEN: usize = 10;

/// Defines the enum and the table `from_u8` searches from one list.
macro_rules! frame_types {
    ($($name:ident = $value:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u8)]
        pub enum FrameType {
            $($name = $value,)*
        }

        impl FrameType {
            pub const ALL: &[FrameType] = &[$(FrameType::$name,)*];
        }
    };
}

frame_types! {
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
        Self::ALL.iter().copied().find(|t| *t as u8 == byte)
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
pub enum FrameError {
    UnknownType(u8),
    TooLong {
        length: u32,
        max_frame: u64,
    },
    /// A message shorter than a header.
    Short,
    /// A message whose header length is not its payload length.
    LengthMismatch {
        length: u32,
        payload: usize,
    },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::UnknownType(t) => write!(f, "unknown frame type 0x{t:02x}"),
            FrameError::TooLong { length, max_frame } => {
                write!(f, "frame length {length} exceeds max_frame {max_frame}")
            }
            FrameError::Short => write!(f, "message shorter than a frame header"),
            FrameError::LengthMismatch { length, payload } => {
                write!(f, "frame length {length} with {payload} payload bytes")
            }
        }
    }
}

impl std::error::Error for FrameError {}

pub(crate) fn encode_header(frame_type: FrameType, channel: u32, length: u32) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0] = frame_type as u8;
    out[2..6].copy_from_slice(&channel.to_le_bytes());
    out[6..10].copy_from_slice(&length.to_le_bytes());
    out
}

/// Parses a header, ignoring `flags`.
pub fn parse_header(bytes: &[u8; HEADER_LEN], max_frame: u64) -> Result<Header, FrameError> {
    let frame_type = FrameType::from_u8(bytes[0]).ok_or(FrameError::UnknownType(bytes[0]))?;
    let channel = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    let length = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    if u64::from(length) > max_frame {
        return Err(FrameError::TooLong { length, max_frame });
    }
    Ok(Header {
        frame_type,
        channel,
        length,
    })
}

/// A complete frame: header followed by the payload.
pub fn encode_frame(frame_type: FrameType, channel: u32, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).expect("frame payload longer than u32");
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&encode_header(frame_type, channel, length));
    out.extend_from_slice(payload);
    out
}

/// A complete frame with a JSON payload.
pub fn encode_json_frame<T: serde::Serialize>(
    frame_type: FrameType,
    channel: u32,
    body: &T,
) -> Vec<u8> {
    encode_frame(
        frame_type,
        channel,
        &serde_json::to_vec(body).expect("serializable"),
    )
}

/// Parses one message that carries exactly one frame.
pub fn parse_frame(message: &[u8], max_frame: u64) -> Result<(Header, &[u8]), FrameError> {
    let (head, payload) = message
        .split_at_checked(HEADER_LEN)
        .ok_or(FrameError::Short)?;
    let header = parse_header(head.try_into().unwrap(), max_frame)?;
    if header.length as usize != payload.len() {
        return Err(FrameError::LengthMismatch {
            length: header.length,
            payload: payload.len(),
        });
    }
    Ok((header, payload))
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
    pub fn next_frame(&mut self, max_frame: u64) -> Result<Option<(Header, Vec<u8>)>, FrameError> {
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

/// The bytes of the stream offset that starts an `OUTPUT` payload.
pub const OUTPUT_OFFSET_LEN: usize = 8;

/// `OUTPUT` payload: the stream offset of the first byte, then the bytes.
pub fn encode_output(offset: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(OUTPUT_OFFSET_LEN + bytes.len());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn parse_output(payload: &[u8]) -> Option<(u64, &[u8])> {
    let (head, bytes) = payload.split_at_checked(OUTPUT_OFFSET_LEN)?;
    Some((u64::from_le_bytes(head.try_into().unwrap()), bytes))
}
