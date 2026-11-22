//! Artist/album grouping shared by the daemon and the TUI. Names group
//! case-insensitively; the first casing seen is kept for display.

use std::collections::{BTreeMap, BTreeSet};

use crate::Track;

pub const UNKNOWN: &str = "Unknown";

type Group = (String, BTreeSet<usize>);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    artists: BTreeMap<String, Group>,
    albums: BTreeMap<(String, String), Group>,
}

impl Catalog {
    pub fn build(tracks: &[Track]) -> Self {
        let mut catalog = Self::default();
        for (i, track) in tracks.iter().enumerate() {
            catalog.insert(i, track);
        }
        catalog
    }

    pub fn insert(&mut self, index: usize, track: &Track) {
        let artist = track.artist.as_deref().unwrap_or(UNKNOWN);
        let album = track.album.as_deref().unwrap_or(UNKNOWN);
        self.artists
            .entry(artist.to_lowercase())
            .or_insert_with(|| (artist.to_string(), BTreeSet::new()))
            .1
            .insert(index);
        self.albums
            .entry((artist.to_lowercase(), album.to_lowercase()))
            .or_insert_with(|| (album.to_string(), BTreeSet::new()))
            .1
            .insert(index);
    }

    pub fn artists(&self) -> Vec<String> {
        self.artists
            .values()
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Album names of one artist, or of everyone when `artist` is `None`.
    pub fn albums(&self, artist: Option<&str>) -> Vec<String> {
        let artist = artist.map(str::to_lowercase);
        let mut out: Vec<(&str, &String)> = self
            .albums
            .iter()
            .filter(|((a, _), _)| artist.as_ref().is_none_or(|x| x == a))
            .map(|((_, album), (name, _))| (album.as_str(), name))
            .collect();
        out.sort();
        out.dedup_by(|a, b| a.0 == b.0);
        out.into_iter().map(|(_, name)| name.clone()).collect()
    }

    /// Indices into `tracks` (the slice the catalog was built from),
    /// ordered by album, disc, track number, path.
    pub fn find(&self, tracks: &[Track], artist: Option<&str>, album: Option<&str>) -> Vec<usize> {
        let artist = artist.map(str::to_lowercase);
        let album = album.map(str::to_lowercase);
        let mut hits: Vec<(&str, usize)> = self
            .albums
            .iter()
            .filter(|((a, b), _)| {
                artist.as_ref().is_none_or(|x| x == a) && album.as_ref().is_none_or(|x| x == b)
            })
            .flat_map(|((_, b), (_, ids))| ids.iter().map(move |&i| (b.as_str(), i)))
            .collect();
        hits.sort_by(|&(ab, ai), &(bb, bi)| {
            let (a, b) = (&tracks[ai], &tracks[bi]);
            ab.cmp(bb)
                .then(a.disc_number.cmp(&b.disc_number))
                .then(a.track_number.cmp(&b.track_number))
                .then_with(|| a.path.cmp(&b.path))
        });
        hits.into_iter().map(|(_, i)| i).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(path: &str, artist: Option<&str>, album: Option<&str>, disc: u32, no: u32) -> Track {
        Track {
            path: path.into(),
            title: None,
            artist: artist.map(Into::into),
            album: album.map(Into::into),
            track_number: Some(no),
            disc_number: Some(disc),
            duration_ms: None,
            format: None,
        }
    }

    #[test]
    fn groups_case_insensitively_first_casing_wins() {
        let tracks = [
            t("1", Some("ABBA"), Some("x"), 1, 1),
            t("2", Some("Abba"), Some("x"), 1, 2),
        ];
        assert_eq!(Catalog::build(&tracks).artists(), ["ABBA"]);
    }

    #[test]
    fn missing_tags_group_under_unknown() {
        let tracks = [t("1", None, None, 1, 1), t("2", Some("A"), None, 1, 1)];
        let c = Catalog::build(&tracks);
        assert_eq!(c.artists(), ["A", UNKNOWN]);
        assert_eq!(c.find(&tracks, Some("unknown"), Some(UNKNOWN)), [0]);
        assert_eq!(c.find(&tracks, Some("A"), None), [1]);
    }

    #[test]
    fn albums_of_everyone_dedup_by_name() {
        let tracks = [
            t("1", Some("A"), Some("Same"), 1, 1),
            t("2", Some("B"), Some("same"), 1, 1),
            t("3", Some("B"), Some("Other"), 1, 1),
        ];
        let c = Catalog::build(&tracks);
        assert_eq!(c.albums(None), ["Other", "Same"]);
        assert_eq!(c.albums(Some("b")), ["Other", "same"]);
    }

    #[test]
    fn find_orders_by_album_disc_track_path() {
        let tracks = [
            t("z", Some("A"), Some("beta"), 1, 1),
            t("d2t1", Some("A"), Some("alpha"), 2, 1),
            t("d1t2", Some("A"), Some("Alpha"), 1, 2),
            t("d1t1b", Some("A"), Some("alpha"), 1, 1),
            t("d1t1a", Some("A"), Some("alpha"), 1, 1),
        ];
        let c = Catalog::build(&tracks);
        assert_eq!(c.find(&tracks, Some("a"), None), [4, 3, 2, 1, 0]);
        assert_eq!(c.find(&tracks, None, Some("BETA")), [0]);
    }

    #[test]
    fn build_equals_inserts() {
        let tracks = [t("1", Some("A"), Some("x"), 1, 1), t("2", None, None, 1, 1)];
        let mut c = Catalog::default();
        for (i, tr) in tracks.iter().enumerate() {
            c.insert(i, tr);
        }
        assert_eq!(Catalog::build(&tracks), c);
    }
}
