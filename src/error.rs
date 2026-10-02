use std::fmt;
use std::io;

/// The one error type: a message that is fit to show to a user.
#[derive(Debug)]
pub struct Error {
    pub msg: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Error {
        Error { msg: msg.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::new(e.to_string())
    }
}

/// `err!("bad {}", x)` builds an [`Error`].
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::error::Error::new(format!($($arg)*)) };
}
