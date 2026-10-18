use ghostty_vt_sys as sys;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    OutOfMemory,
    InvalidValue,
    OutOfSpace,
    NoValue,
    Io,
    LimitExceeded,
    Rejected,
    Unknown(i32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::OutOfMemory => f.write_str("out of memory"),
            Error::InvalidValue => f.write_str("invalid value"),
            Error::OutOfSpace => f.write_str("out of space"),
            Error::NoValue => f.write_str("no value"),
            Error::Io => f.write_str("i/o error"),
            Error::LimitExceeded => f.write_str("limit exceeded"),
            Error::Rejected => f.write_str("rejected"),
            Error::Unknown(code) => write!(f, "libghostty-vt result {code}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn check(result: sys::GhosttyResult) -> Result<()> {
    Err(match result {
        sys::GHOSTTY_SUCCESS => return Ok(()),
        sys::GHOSTTY_OUT_OF_MEMORY => Error::OutOfMemory,
        sys::GHOSTTY_INVALID_VALUE => Error::InvalidValue,
        sys::GHOSTTY_OUT_OF_SPACE => Error::OutOfSpace,
        sys::GHOSTTY_NO_VALUE => Error::NoValue,
        sys::GHOSTTY_IO_ERROR => Error::Io,
        sys::GHOSTTY_LIMIT_EXCEEDED => Error::LimitExceeded,
        sys::GHOSTTY_REJECTED => Error::Rejected,
        other => Error::Unknown(other),
    })
}
