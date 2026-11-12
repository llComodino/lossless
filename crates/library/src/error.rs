use std::path::PathBuf;

use lossless_proto::{ErrorCode, WireError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: no such file or folder", path.display())]
    NotFound { path: PathBuf },
    #[error("{}: not a flac or mp3 file", path.display())]
    NotPlayable { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn code(&self) -> ErrorCode {
        match self {
            Error::Io { .. } => ErrorCode::Io,
            Error::NotFound { .. } | Error::NotPlayable { .. } => ErrorCode::NotFound,
        }
    }

    pub fn to_wire(&self) -> WireError {
        WireError {
            code: self.code(),
            message: self.to_string(),
        }
    }
}
