#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lofty::prelude::*;
use lossless_proto::{StreamFormat, Track};

mod error;

pub use error::{Error, Result};

pub const AUDIO_EXTENSIONS: &[&str] = &["flac", "mp3"];

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

/// Expand files and directories (recursively, sorted, skipping dot entries)
/// into playable files.
pub fn collect(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let io = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = std::fs::metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound {
                path: path.to_path_buf(),
            }
        } else {
            io(source)
        }
    })?;
    if meta.is_dir() {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path).map_err(io)? {
            let p = entry.map_err(io)?.path();
            if !is_hidden(&p) {
                entries.push(p);
            }
        }
        entries.sort();
        for entry in entries {
            collect_nested(&entry, out)?;
        }
    } else if is_audio(path) {
        out.push(path.to_path_buf());
    } else {
        return Err(Error::NotPlayable {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Inside a directory, non-audio files are skipped rather than rejected.
fn collect_nested(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() || is_audio(path) {
        collect(path, out)
    } else {
        Ok(())
    }
}

/// Parse with the parser the extension names, never by sniffing content:
/// lofty's sniffer routes bytes that look like ADTS to its AAC parser, which
/// can loop forever on crafted input, and only FLAC and MP3 are indexed.
fn parse<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    path: &Path,
) -> Option<lofty::file::TaggedFile> {
    use lofty::config::ParseOptions;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let opts = ParseOptions::new();
    match ext.as_str() {
        "flac" => lofty::flac::FlacFile::read_from(reader, opts)
            .ok()
            .map(Into::into),
        "mp3" => lofty::mpeg::MpegFile::read_from(reader, opts)
            .ok()
            .map(Into::into),
        _ => None,
    }
}

/// Tags and audio properties of one file. Unreadable tags leave the fields
/// empty rather than failing: the file may still play.
pub fn read_track(path: &Path) -> Track {
    let file = std::fs::File::open(path)
        .ok()
        .and_then(|f| parse(&mut std::io::BufReader::new(f), path));
    describe(path.display().to_string(), file)
}

/// Like `read_track`, for a file already in memory. `path` is recorded and
/// its extension picks the parser.
pub fn read_track_from(path: &str, bytes: &[u8]) -> Track {
    let file = parse(&mut std::io::Cursor::new(bytes), Path::new(path));
    describe(path.to_string(), file)
}

fn describe(path: String, file: Option<lofty::file::TaggedFile>) -> Track {
    let mut track = Track {
        path,
        title: None,
        artist: None,
        album: None,
        track_number: None,
        disc_number: None,
        duration_ms: None,
        format: None,
    };
    let Some(file) = file else {
        return track;
    };
    let props = file.properties();
    track.duration_ms = Some(props.duration().as_millis() as u64);
    if let (Some(rate), Some(channels)) = (props.sample_rate(), props.channels()) {
        track.format = Some(StreamFormat {
            rate,
            bits: props.bit_depth().map_or(32, u16::from),
            channels: channels.into(),
        });
    }
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        let text = |s: Option<std::borrow::Cow<'_, str>>| {
            s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        };
        track.title = text(tag.title());
        track.artist = text(tag.artist());
        track.album = text(tag.album());
        track.track_number = tag.track();
        track.disc_number = tag.disk();
    }
    track
}

#[derive(Default)]
pub struct Library {
    tracks: Vec<Track>,
    by_path: HashMap<String, usize>,
    /// Lowercased artist/album/title/path per track, for search.
    haystack: Vec<String>,
}

impl Library {
    pub fn scan(root: &Path) -> Result<Self> {
        let mut files = Vec::new();
        collect(root, &mut files)?;
        let mut lib = Self::default();
        for path in files {
            lib.insert(read_track(&path));
        }
        Ok(lib)
    }

    fn insert(&mut self, track: Track) {
        let hay = [
            track.artist.as_deref(),
            track.album.as_deref(),
            track.title.as_deref(),
            Some(track.path.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
        self.by_path.insert(track.path.clone(), self.tracks.len());
        self.tracks.push(track);
        self.haystack.push(hay);
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn get(&self, path: &str) -> Option<&Track> {
        self.by_path.get(path).map(|&i| &self.tracks[i])
    }

    /// Tracks whose artist, album, title or path contain every word of
    /// `query`, case-insensitively, in library order.
    pub fn search(&self, query: &str) -> Vec<&Track> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return Vec::new();
        }
        self.tracks
            .iter()
            .zip(&self.haystack)
            .filter(|(_, hay)| words.iter().all(|w| hay.contains(w.as_str())))
            .map(|(t, _)| t)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str, artist: &str, album: &str, title: &str) -> Track {
        Track {
            path: path.into(),
            title: Some(title.into()),
            artist: Some(artist.into()),
            album: Some(album.into()),
            track_number: None,
            disc_number: None,
            duration_ms: None,
            format: None,
        }
    }

    #[test]
    fn search_needs_every_word() {
        let mut lib = Library::default();
        lib.insert(track("/m/muse/a/08.flac", "Muse", "Absolution", "Hysteria"));
        lib.insert(track(
            "/m/muse/b/01.flac",
            "Muse",
            "Origin of Symmetry",
            "New Born",
        ));
        lib.insert(track("/m/queen/x.flac", "Queen", "Innuendo", "Innuendo"));
        let titles = |q: &str| {
            lib.search(q)
                .iter()
                .map(|t| t.title.clone().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(titles("muse"), ["Hysteria", "New Born"]);
        assert_eq!(titles("MUSE hyst"), ["Hysteria"]);
        assert_eq!(titles("queen"), ["Innuendo"]);
        assert!(titles("muse queen").is_empty());
        assert!(titles("   ").is_empty());
        assert_eq!(
            lib.get("/m/queen/x.flac").unwrap().artist.as_deref(),
            Some("Queen")
        );
    }

    #[test]
    fn tags_from_disk_and_memory_agree() {
        use lossless_engine::fixtures::{BLOCK, Dir, flac};
        let d = Dir::new();
        let f = flac(&d, "a.flac", 96_000, 24, 2, 2 * BLOCK);
        let on_disk = read_track(&f.path);
        let bytes = std::fs::read(&f.path).unwrap();
        let in_memory = read_track_from(&f.path.display().to_string(), &bytes);
        assert_eq!(on_disk, in_memory);
        let fmt = on_disk.format.expect("format");
        assert_eq!((fmt.rate, fmt.bits, fmt.channels), (96_000, 24, 2));
        assert_eq!(on_disk.duration_ms, Some(2 * BLOCK as u64 * 1000 / 96_000));
        let junk = read_track_from("junk", &[0x5a; 64]);
        assert_eq!(
            (junk.format, junk.duration_ms, junk.title),
            (None, None, None)
        );
    }

    /// Found by the `tags` fuzzer: lofty's content sniffing calls these
    /// ADTS/AAC-looking bytes and its AAC parser never returns (0.24 and
    /// 0.25.5). Named .flac or .mp3 they must fail fast; anything else isn't
    /// parsed at all.
    #[test]
    fn adts_looking_bytes_do_not_hang() {
        let inputs: [&[u8]; 2] = [
            &[
                0x00, 0x00, 0x00, 0xd9, 0xff, 0xd9, 0xff, 0xf8, 0xf9, 0x00, 0x00, 0x00, 0xd9, 0xff,
                0xd9, 0xff, 0xf8, 0xf9, 0x00, 0x00,
            ],
            &[
                0xff, 0xf9, 0x00, 0x00, 0x00, 0x4b, 0xf0, 0xff, 0xf9, 0x00, 0x00, 0x00, 0x4b, 0x04,
                0xff, 0xf9, 0x00, 0x08, 0x00, 0x00, 0x00, 0xff, 0xf9, 0x00, 0x10, 0x00, 0x00, 0x00,
                0xf0,
            ],
        ];
        for bytes in inputs {
            for name in ["x.flac", "x.mp3", "x.aac", "x"] {
                let bytes = bytes.to_vec();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || tx.send(read_track_from(name, &bytes)).unwrap());
                let t = rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|_| panic!("tag reader hung on {name}"));
                assert_eq!(t.format, None);
            }
        }
    }

    #[test]
    fn disc_numbers_are_read() {
        use lofty::tag::{Tag, TagType};
        use lossless_engine::fixtures::{BLOCK, Dir, flac};
        let d = Dir::new();
        let f = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        assert_eq!(read_track(&f.path).disc_number, None);
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.insert_text(ItemKey::DiscNumber, "2".into());
        tag.save_to_path(&f.path, lofty::config::WriteOptions::default())
            .unwrap();
        assert_eq!(read_track(&f.path).disc_number, Some(2));
    }

    #[test]
    fn other_extensions_are_not_parsed() {
        use lossless_engine::fixtures::{BLOCK, Dir, flac};
        let d = Dir::new();
        let f = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let bytes = std::fs::read(&f.path).unwrap();
        assert!(read_track_from("a.flac", &bytes).format.is_some());
        assert!(read_track_from("a.FLAC", &bytes).format.is_some());
        assert!(read_track_from("a.ogg", &bytes).format.is_none());
        assert!(read_track_from("a", &bytes).format.is_none());
    }

    #[test]
    fn collect_skips_hidden_and_non_audio() {
        let root = std::env::temp_dir().join(format!("lossless-collect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("album")).unwrap();
        std::fs::create_dir_all(root.join(".incomplete")).unwrap();
        for f in [
            "album/02.flac",
            "album/01.FLAC",
            "album/cover.jpg",
            ".incomplete/x.flac",
        ] {
            std::fs::write(root.join(f), b"").unwrap();
        }
        let mut out = Vec::new();
        collect(&root, &mut out).unwrap();
        let names: Vec<_> = out
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["album/01.FLAC", "album/02.flac"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn collect_errors_name_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("lossless-errs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locked")).unwrap();
        std::fs::write(root.join("cover.jpg"), b"").unwrap();
        let mut out = Vec::new();

        let missing = root.join("nope");
        let e = collect(&missing, &mut out).unwrap_err();
        assert!(matches!(&e, Error::NotFound { path } if *path == missing));
        assert_eq!(e.code(), lossless_proto::ErrorCode::NotFound);
        assert_eq!(
            e.to_string(),
            format!("{}: no such file or folder", missing.display())
        );

        let cover = root.join("cover.jpg");
        let e = collect(&cover, &mut out).unwrap_err();
        assert!(matches!(&e, Error::NotPlayable { path } if *path == cover));
        assert_eq!(e.code(), lossless_proto::ErrorCode::NotFound);
        assert_eq!(
            e.to_string(),
            format!("{}: not a flac or mp3 file", cover.display())
        );

        let locked = root.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let res = collect(&locked, &mut out);
        let scan = Library::scan(&locked);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = res.unwrap_err();
        assert!(matches!(&e, Error::Io { path, .. } if *path == locked));
        assert_eq!(e.code(), lossless_proto::ErrorCode::Io);
        assert!(
            e.to_string()
                .starts_with(&format!("{}: ", locked.display()))
        );
        assert_eq!(e.to_wire().message, e.to_string());
        assert!(scan.is_err());
        assert!(out.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}

#[cfg(test)]
mod props {
    use proptest::prelude::*;

    use super::*;

    fn library(entries: &[(String, String, String)]) -> Library {
        let mut lib = Library::default();
        for (i, (artist, album, title)) in entries.iter().enumerate() {
            lib.insert(Track {
                path: format!("/m/{i}.flac"),
                title: Some(title.clone()),
                artist: Some(artist.clone()),
                album: Some(album.clone()),
                track_number: None,
                disc_number: None,
                duration_ms: None,
                format: None,
            });
        }
        lib
    }

    fn entries() -> impl Strategy<Value = Vec<(String, String, String)>> {
        proptest::collection::vec((".{0,12}", ".{0,12}", ".{0,12}"), 0..20)
    }

    proptest! {
        /// Every hit contains every query word, and nothing outside the
        /// library comes back.
        #[test]
        fn hits_contain_every_word(es in entries(), query in ".{0,20}") {
            let lib = library(&es);
            let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
            let hits = lib.search(&query);
            prop_assert!(hits.len() <= lib.len());
            for hit in hits {
                let i = lib.by_path[&hit.path];
                for w in &words {
                    prop_assert!(lib.haystack[i].contains(w.as_str()));
                }
            }
        }

        /// Search finds every track whose fields contain the query.
        #[test]
        fn search_finds_what_is_there(es in entries(), pick in any::<prop::sample::Index>()) {
            prop_assume!(!es.is_empty());
            let lib = library(&es);
            let (_, _, title) = &es[pick.index(es.len())];
            prop_assume!(!title.trim().is_empty());
            let path = format!("/m/{}.flac", pick.index(es.len()));
            prop_assert!(lib.search(title).iter().any(|t| t.path == path));
        }

        #[test]
        fn ascii_search_ignores_case(es in entries(), query in "[a-zA-Z ]{0,10}") {
            let lib = library(&es);
            let paths = |q: &str| lib.search(q).iter().map(|t| t.path.clone()).collect::<Vec<_>>();
            prop_assert_eq!(paths(&query.to_uppercase()), paths(&query.to_lowercase()));
        }
    }
}
