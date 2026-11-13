use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use lossless_proto::Repeat;
use serde::{Deserialize, Serialize};

/// What survives a restart: the queue, where in it, and the modes.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub queue: Vec<PathBuf>,
    pub index: Option<usize>,
    pub position_ms: u64,
    pub repeat: Repeat,
    pub random: bool,
}

impl Saved {
    /// Drop entries whose files are gone, keeping `index` on the same entry;
    /// if that entry is gone too, start from the top. Returns the count dropped.
    pub fn prune(self) -> (Self, usize) {
        let before = self.queue.len();
        let mut index = None;
        let mut queue = Vec::with_capacity(before);
        for (i, p) in self.queue.into_iter().enumerate() {
            if p.exists() {
                if self.index == Some(i) {
                    index = Some(queue.len());
                }
                queue.push(p);
            }
        }
        let dropped = before - queue.len();
        let position_ms = if index.is_some() { self.position_ms } else { 0 };
        (
            Self {
                queue,
                index,
                position_ms,
                ..self
            },
            dropped,
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("reading {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{} is not a regular file", .0.display())]
    NotFile(PathBuf),
    #[error("{} is larger than {MAX_STATE} bytes", .0.display())]
    TooLarge(PathBuf),
    #[error("parsing {}: {source}", path.display())]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// Largest accepted state file. A queue of 100k entries is a few MiB.
pub const MAX_STATE: u64 = 16 * 1024 * 1024;

/// Write `bytes` to `<path>.tmp`, created afresh (owner-only, never through
/// a symlink or with a stale file's permissions) and synced. A failed write
/// removes it.
fn write_tmp(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        })
        .inspect_err(|_| drop(fs::remove_file(&tmp)))?;
    Ok(tmp)
}

/// Write `<path>.tmp`, sync it, then rename it over `path`: a crash leaves
/// either the old file or the new one, never a partial one.
pub fn save(path: &Path, s: &Saved) -> io::Result<()> {
    let json = serde_json::to_vec(s)?;
    fs::rename(write_tmp(path, &json)?, path)
}

/// Create the directory for `path` and prove a state file can be written
/// there, through the same path `save` takes.
pub fn prepare(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
    fs::remove_file(write_tmp(path, b"")?)
}

/// Read the state at `path`; a missing file is `None`. Only a regular file
/// of at most `MAX_STATE` bytes is read, and reading stops past the cap.
pub fn load(path: &Path) -> Result<Option<Saved>, StateError> {
    let io = |source| StateError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(e)),
    };
    if !meta.is_file() {
        return Err(StateError::NotFile(path.to_path_buf()));
    }
    let mut bytes = Vec::new();
    if meta.len() <= MAX_STATE {
        File::open(path)
            .and_then(|f| f.take(MAX_STATE + 1).read_to_end(&mut bytes))
            .map_err(io)?;
    }
    if meta.len().max(bytes.len() as u64) > MAX_STATE {
        return Err(StateError::TooLarge(path.to_path_buf()));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| StateError::Json {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use lossless_engine::fixtures::Dir;
    use lossless_proto::Repeat;

    use super::{MAX_STATE, Saved, load, save};

    fn saved(queue: &[&str], index: Option<usize>) -> Saved {
        Saved {
            queue: queue.iter().map(PathBuf::from).collect(),
            index,
            position_ms: 61_250,
            repeat: Repeat::One,
            random: true,
        }
    }

    #[test]
    fn save_is_atomic() {
        let dir = Dir::new();
        let p = dir.path("state.json");
        std::fs::write(&p, "old").unwrap();
        save(&p, &saved(&["/m/a.flac", "/m/b.flac"], Some(1))).unwrap();
        let back = load(&p).unwrap().unwrap();
        assert_eq!(back, saved(&["/m/a.flac", "/m/b.flac"], Some(1)));
        assert!(!dir.path("state.json.tmp").exists());
        let names: Vec<_> = std::fs::read_dir(&dir.0).unwrap().collect();
        assert_eq!(names.len(), 1);
        let tmp = dir.path("fresh.json");
        save(&tmp, &saved(&[], None)).unwrap();
        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(save(&dir.path("missing/state.json"), &saved(&[], None)).is_err());
    }

    #[test]
    fn save_writes_a_fresh_private_tmp() {
        let dir = Dir::new();
        let p = dir.path("state.json");
        let tmp = dir.path("state.json.tmp");
        std::fs::write(&tmp, "stale").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        save(&p, &saved(&[], None)).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let target = dir.path("target");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &tmp).unwrap();
        save(&p, &saved(&["/m/a.flac"], Some(0))).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        assert!(std::fs::symlink_metadata(&p).unwrap().file_type().is_file());
        assert_eq!(load(&p).unwrap().unwrap(), saved(&["/m/a.flac"], Some(0)));

        std::fs::create_dir(&tmp).unwrap();
        assert!(save(&p, &saved(&[], None)).is_err());
    }

    #[test]
    fn load_missing_is_none() {
        let dir = Dir::new();
        assert!(load(&dir.path("state.json")).unwrap().is_none());
    }

    #[test]
    fn load_rejects_oversized_and_bad_json() {
        let dir = Dir::new();
        let p = dir.path("state.json");
        std::fs::write(&p, r#"{"queue":["/a.flac"],"index":0"#).unwrap();
        let err = load(&p).unwrap_err().to_string();
        let want = format!("parsing {}", p.display());
        assert!(err.starts_with(&want), "{err}");
        let big = std::fs::File::create(&p).unwrap();
        big.set_len(MAX_STATE + 1).unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("larger than"), "{err}");
        std::fs::remove_file(&p).unwrap();
        std::fs::create_dir(&p).unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");
        let file = dir.path("file");
        std::fs::write(&file, "").unwrap();
        let err = load(&file.join("x")).unwrap_err().to_string();
        assert!(err.starts_with("reading"), "{err}");
    }

    #[test]
    fn prune_drops_gone_entries_and_follows_the_current_one() {
        let dir = Dir::new();
        let here = |n: &str| {
            let p = dir.path(n);
            std::fs::write(&p, "").unwrap();
            p.display().to_string()
        };
        let (a, b) = (here("a"), here("b"));
        let gone = dir.path("gone").display().to_string();
        let (s, dropped) = saved(&[&gone, &a, &gone, &b], Some(3)).prune();
        assert_eq!((dropped, s.index, s.position_ms), (2, Some(1), 61_250));
        assert_eq!(s.queue, [PathBuf::from(&a), PathBuf::from(&b)]);
        let (s, dropped) = saved(&[&a, &gone], Some(1)).prune();
        assert_eq!((dropped, s.index, s.position_ms), (1, None, 0));
        let (s, _) = saved(&[&a], Some(4)).prune();
        assert_eq!((s.index, s.position_ms), (None, 0));
    }
}
