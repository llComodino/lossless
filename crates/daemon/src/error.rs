use lossless_proto::{ErrorCode, WireError};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RequestError {
    #[error(transparent)]
    Engine(#[from] lossless_engine::Error),
    #[error(transparent)]
    Library(#[from] lossless_library::Error),
    #[error("{0}")]
    BadRequest(String),
    #[error("no music_dir configured")]
    NoMusicDir,
    #[error("a scan is already running")]
    ScanRunning,
    #[error("no playable files")]
    NoPlayableFiles,
}

impl RequestError {
    pub(crate) fn to_wire(&self) -> WireError {
        match self {
            Self::Engine(e) => e.to_wire(),
            Self::Library(e) => e.to_wire(),
            Self::BadRequest(_) => self.wire(ErrorCode::BadRequest),
            Self::NoMusicDir => self.wire(ErrorCode::NoMusicDir),
            Self::ScanRunning => self.wire(ErrorCode::ScanRunning),
            Self::NoPlayableFiles => self.wire(ErrorCode::NotFound),
        }
    }

    fn wire(&self, code: ErrorCode) -> WireError {
        WireError {
            code,
            message: self.to_string(),
        }
    }
}
