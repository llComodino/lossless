use std::path::PathBuf;

use lossless_proto::{ErrorCode, WireError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {reason}", path.display())]
    Open {
        path: PathBuf,
        reason: String,
        missing: bool,
    },
    #[error("{}: {reason}", path.display())]
    Decode { path: PathBuf, reason: String },
    #[error("{device}: {reason}")]
    Device { device: String, reason: String },
    #[error("index {index} out of range: queue has {len} entries")]
    OutOfRange { index: usize, len: usize },
    #[error("no hardware mixer")]
    NoMixer,
    #[error("{0}")]
    BadRequest(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn code(&self) -> ErrorCode {
        match self {
            Error::Open { missing: true, .. } => ErrorCode::NotFound,
            Error::Open { .. } => ErrorCode::Io,
            Error::Decode { .. } => ErrorCode::Decode,
            Error::Device { .. } => ErrorCode::Device,
            Error::OutOfRange { .. } => ErrorCode::OutOfRange,
            Error::NoMixer => ErrorCode::NoMixer,
            Error::BadRequest(_) => ErrorCode::BadRequest,
        }
    }

    pub fn to_wire(&self) -> WireError {
        WireError {
            code: self.code(),
            message: self.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use lossless_proto::ErrorCode;

    use super::Error;

    #[test]
    fn codes_and_messages() {
        let open = |missing| Error::Open {
            path: PathBuf::from("/m/a.flac"),
            reason: "gone".into(),
            missing,
        };
        assert_eq!(open(true).code(), ErrorCode::NotFound);
        assert_eq!(open(false).code(), ErrorCode::Io);
        let w = open(true).to_wire();
        assert_eq!(w.message, "/m/a.flac: gone");
        let dev = Error::Device {
            device: "hw:0".into(),
            reason: "busy".into(),
        };
        assert_eq!(
            (dev.code(), dev.to_string()),
            (ErrorCode::Device, "hw:0: busy".into())
        );
        let range = Error::OutOfRange { index: 5, len: 2 };
        assert_eq!(range.code(), ErrorCode::OutOfRange);
        assert_eq!(
            range.to_string(),
            "index 5 out of range: queue has 2 entries"
        );
        let dec = Error::Decode {
            path: "x.flac".into(),
            reason: "bad".into(),
        };
        assert_eq!(
            (dec.code(), dec.to_string()),
            (ErrorCode::Decode, "x.flac: bad".into())
        );
        assert_eq!(Error::NoMixer.code(), ErrorCode::NoMixer);
        let bad = Error::BadRequest("nope".into());
        assert_eq!(
            (bad.code(), bad.to_string()),
            (ErrorCode::BadRequest, "nope".into())
        );
    }
}
