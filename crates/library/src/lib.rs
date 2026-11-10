#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::prelude::*;
use lossless_proto::{StreamFormat, Track};

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
    let meta = std::fs::metadata(path).with_context(|| path.display().to_string())?;
    if meta.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| path.display().to_string())?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| !is_hidden(p))
            .collect();
        entries.sort();
        for entry in entries {
            collect(&entry, out)?;
        }
    } else if is_audio(path) {
        out.push(path.to_path_buf());
    }
    Ok(())
}

/// Tags and audio properties of one file. Unreadable tags leave the fields
/// empty rather than failing: the file may still play.
pub fn read_track(path: &Path) -> Track {
    describe(path.display().to_string(), lofty::read_from_path(path).ok())
}

/// Like `read_track`, for a file already in memory; `path` is only recorded.
pub fn read_track_from(path: &str, bytes: &[u8]) -> Track {
    let file = lofty::probe::Probe::new(std::io::Cursor::new(bytes))
        .guess_file_type()
        .ok()
        .and_then(|p| p.read().ok());
    describe(path.to_string(), file)
}

fn describe(path: String, file: Option<lofty::file::TaggedFile>) -> Track {
    let mut track = Track {
        path,
        title: None,
        artist: None,
        album: None,
        track_number: None,
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
