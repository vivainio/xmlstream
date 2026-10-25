use std::{fmt, io};

#[derive(Debug)]
pub enum Error {
    /// Malformed input or a violated parser limit.
    Xml(String),
    /// A bad selector expression.
    Path(String),
    /// The writer was asked to emit something that would not be well-formed.
    Write(String),
    /// A record could not be filled in (see `Extract`).
    Extract(String),
    /// A memory budget was exceeded (see `Limits`).
    Limit(String),
    /// An error raised by a user callback.
    Other(Box<dyn std::error::Error + Send + Sync>),
    Io(io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Xml(m) => write!(f, "xml: {m}"),
            Error::Path(m) => write!(f, "path: {m}"),
            Error::Write(m) => write!(f, "write: {m}"),
            Error::Extract(m) => write!(f, "extract: {m}"),
            Error::Limit(m) => write!(f, "limit: {m}"),
            Error::Other(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Wrap any error from a callback.
    pub fn other(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Error::Other(e.into())
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub(crate) fn xml_err(e: impl fmt::Display) -> Error {
    Error::Xml(e.to_string())
}
