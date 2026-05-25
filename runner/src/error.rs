use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Nix(nix::Error),
    Intercept(String),
    Pty(String),
    InvalidState(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "IO: {e}"),
            Error::Nix(e) => write!(f, "Nix: {e}"),
            Error::Intercept(s) => write!(f, "Intercept: {s}"),
            Error::Pty(s) => write!(f, "PTY: {s}"),
            Error::InvalidState(s) => write!(f, "Invalid state: {s}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<nix::Error> for Error {
    fn from(e: nix::Error) -> Self {
        Error::Nix(e)
    }
}
