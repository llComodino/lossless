use std::collections::BTreeSet;
use std::path::Path;

use lossless_proto::{Entry, Track};

use crate::{Error, Library, Result};

pub use lossless_proto::catalog::UNKNOWN;

impl Library {
    /// Folders and tracks directly under `rel`, a path relative to the music
    /// root. A folder exists exactly when some track lies beneath it.
    pub fn browse(&self, rel: &str) -> Result<Vec<Entry>> {
        let not_found = || Error::NotFound {
            path: self.root.join(rel.trim_start_matches('/')),
        };
        let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        if segments.iter().any(|s| matches!(*s, "." | "..")) {
            return Err(not_found());
        }
        let mut dirs = BTreeSet::new();
        let mut files = Vec::new();
        let mut found = segments.is_empty();
        for track in &self.tracks {
            let Ok(under) = Path::new(&track.path).strip_prefix(&self.root) else {
                continue;
            };
            let parts: Vec<_> = under.iter().map(|p| p.to_string_lossy()).collect();
            if parts.len() <= segments.len() || parts.iter().zip(&segments).any(|(a, b)| a != b) {
                continue;
            }
            found = true;
            let next = &parts[segments.len()];
            if parts.len() == segments.len() + 1 {
                files.push((next.to_lowercase(), track));
            } else {
                dirs.insert((next.to_lowercase(), next.to_string()));
            }
        }
        if !found {
            return Err(not_found());
        }
        files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
        let prefix = segments.join("/");
        let dirs = dirs.into_iter().map(|(_, name)| Entry::Dir {
            path: if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            },
            name,
        });
        Ok(dirs
            .chain(files.into_iter().map(|(_, t)| Entry::Track(t.clone())))
            .collect())
    }

    pub fn artists(&self) -> Vec<String> {
        self.catalog.artists()
    }

    /// Album names of one artist, or of everyone when `artist` is `None`.
    pub fn albums(&self, artist: Option<&str>) -> Vec<String> {
        self.catalog.albums(artist)
    }

    pub fn find(&self, artist: Option<&str>, album: Option<&str>) -> Vec<&Track> {
        self.catalog
            .find(&self.tracks, artist, album)
            .into_iter()
            .map(|i| &self.tracks[i])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/m";

    fn t(rel: &str, artist: Option<&str>, album: Option<&str>, disc: u32, no: u32) -> Track {
        Track {
            path: format!("{ROOT}/{rel}"),
            title: Some(rel.into()),
            artist: artist.map(Into::into),
            album: album.map(Into::into),
            track_number: Some(no),
            disc_number: Some(disc),
            duration_ms: None,
            format: None,
        }
    }

    fn lib(tracks: Vec<Track>) -> Library {
        Library::from_tracks(Path::new(ROOT), tracks)
    }

    fn sample() -> Library {
        lib(vec![
            t(
                "Björk/Homogenic/02 Joga.flac",
                Some("Björk"),
                Some("Homogenic"),
                1,
                2,
            ),
            t(
                "Björk/Homogenic/01 Hunter.flac",
                Some("Björk"),
                Some("Homogenic"),
                1,
                1,
            ),
            t("a/b/x.flac", Some("Muse"), Some("Absolution"), 1, 1),
            t("a/Z/y.flac", Some("muse"), Some("Origin"), 1, 1),
            t("a/c.flac", Some("Queen"), Some("Innuendo"), 1, 1),
            t("top.flac", None, None, 1, 1),
        ])
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Dir { name, .. } => format!("{name}/"),
                Entry::Track(t) => t.path.rsplit('/').next().unwrap().to_string(),
            })
            .collect()
    }

    #[test]
    fn browse_lists_dirs_then_tracks() {
        let l = sample();
        assert_eq!(names(&l.browse("").unwrap()), ["a/", "Björk/", "top.flac"]);
        let a = l.browse("a").unwrap();
        assert_eq!(names(&a), ["b/", "Z/", "c.flac"]);
        assert_eq!(
            a[1],
            Entry::Dir {
                name: "Z".into(),
                path: "a/Z".into()
            }
        );
        assert_eq!(
            names(&l.browse("Björk/Homogenic").unwrap()),
            ["01 Hunter.flac", "02 Joga.flac"]
        );
    }

    #[test]
    fn browse_paths_are_confined_and_normalized() {
        let l = sample();
        assert_eq!(l.browse("").unwrap(), l.browse("/").unwrap());
        assert_eq!(l.browse("a/b/").unwrap(), l.browse("a/b").unwrap());
        assert_eq!(l.browse("//a//b").unwrap(), l.browse("a/b").unwrap());
        for bad in ["a/../b", "./a", "/etc", "..", "a/.", "nope", "a/b/x.flac"] {
            let e = l.browse(bad).unwrap_err();
            assert!(matches!(e, Error::NotFound { .. }), "{bad}");
            assert_eq!(e.code(), lossless_proto::ErrorCode::NotFound);
        }
        assert_eq!(l.browse("Björk/Homogenic").unwrap().len(), 2);
        assert!(lib(vec![]).browse("").unwrap().is_empty());
    }

    #[test]
    fn artists_are_sorted_case_insensitively_with_unknown() {
        let l = lib(vec![
            t("1.flac", Some("queen"), Some("x"), 1, 1),
            t("2.flac", Some("Abba"), Some("x"), 1, 1),
            t("3.flac", Some("Queen"), Some("x"), 1, 1),
            t("4.flac", None, Some("x"), 1, 1),
            t("5.flac", Some("Zed"), Some("x"), 1, 1),
        ]);
        assert_eq!(l.artists(), ["Abba", "queen", "Unknown", "Zed"]);
    }

    #[test]
    fn albums_for_one_artist_and_all() {
        let l = sample();
        assert_eq!(l.albums(Some("MUSE")), ["Absolution", "Origin"]);
        assert_eq!(
            l.albums(None),
            ["Absolution", "Homogenic", "Innuendo", "Origin", "Unknown"]
        );
        assert!(l.albums(Some("nobody")).is_empty());
    }

    #[test]
    fn find_sorts_by_album_disc_track_path() {
        let l = lib(vec![
            t("z.flac", Some("A"), Some("beta"), 1, 1),
            t("d2t1.flac", Some("A"), Some("alpha"), 2, 1),
            t("d1t2.flac", Some("A"), Some("Alpha"), 1, 2),
            t("d1t1b.flac", Some("A"), Some("alpha"), 1, 1),
            t("d1t1a.flac", Some("A"), Some("alpha"), 1, 1),
        ]);
        let paths = |f: Vec<&Track>| {
            f.iter()
                .map(|t| t.path.trim_start_matches("/m/").to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths(l.find(Some("a"), None)),
            [
                "d1t1a.flac",
                "d1t1b.flac",
                "d1t2.flac",
                "d2t1.flac",
                "z.flac"
            ]
        );
        assert_eq!(paths(l.find(None, Some("BETA"))), ["z.flac"]);
        assert_eq!(l.find(None, None).len(), 5);
        assert!(l.find(Some("b"), None).is_empty());
    }

    #[test]
    fn unknown_matches_untagged() {
        let l = lib(vec![
            t("u.flac", None, None, 1, 1),
            t("v.flac", Some("A"), None, 1, 1),
            t("w.flac", Some("A"), Some("X"), 1, 1),
        ]);
        assert_eq!(l.find(Some("Unknown"), None).len(), 1);
        assert_eq!(l.find(Some("unknown"), Some(UNKNOWN)).len(), 1);
        assert_eq!(l.find(Some("A"), Some("Unknown")).len(), 1);
        assert_eq!(l.find(None, Some("Unknown")).len(), 2);
    }
}
