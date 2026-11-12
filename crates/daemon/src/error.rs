use lossless_proto::{ErrorCode, WireError};

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
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
    #[error("no playable files{}", in_paths(.0))]
    NoPlayableFiles(Vec<String>),
}

fn in_paths(paths: &[String]) -> String {
    if paths.is_empty() {
        String::new()
    } else {
        format!(" in {}", paths.join(", "))
    }
}

impl RequestError {
    pub fn to_wire(&self) -> WireError {
        match self {
            Self::Engine(e) => e.to_wire(),
            Self::Library(e) => e.to_wire(),
            Self::BadRequest(_) => self.wire(ErrorCode::BadRequest),
            Self::NoMusicDir => self.wire(ErrorCode::NoMusicDir),
            Self::ScanRunning => self.wire(ErrorCode::ScanRunning),
            Self::NoPlayableFiles(_) => self.wire(ErrorCode::NotFound),
        }
    }

    fn wire(&self, code: ErrorCode) -> WireError {
        WireError {
            code,
            message: self.to_string(),
        }
    }
}
