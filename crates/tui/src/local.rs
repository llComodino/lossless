use std::collections::HashSet;
use std::path::Path;

use lossless_proto::Track;
use lossless_proto::catalog::Catalog;

#[derive(Debug, PartialEq, Eq)]
pub enum Delta {
    Applied,
    Stale,
    Refetch,
}

/// The library as of `generation`, kept in daemon order; `seen` is the
/// newest event generation that arrived while unloaded.
#[derive(Default)]
pub struct Local {
    pub tracks: Vec<Track>,
    pub catalog: Catalog,
    generation: Option<u64>,
    seen: u64,
}

impl Local {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// A `Tracks` reply. `Refetch` when it predates an event that arrived
    /// while unloaded.
    pub fn replace(&mut self, generation: u64, tracks: Vec<Track>) -> Delta {
        if self.generation.is_some_and(|g| generation < g) {
            return Delta::Stale;
        }
        self.generation = Some(generation);
        self.tracks = tracks;
        self.catalog = Catalog::build(&self.tracks);
        if generation < self.seen {
            Delta::Refetch
        } else {
            Delta::Applied
        }
    }

    /// A `LibraryChanged` event: applies only the next generation.
    pub fn apply(&mut self, generation: u64, added: Vec<Track>, removed: Vec<String>) -> Delta {
        let Some(current) = self.generation else {
            self.seen = self.seen.max(generation);
            return Delta::Stale;
        };
        if generation <= current {
            return Delta::Stale;
        }
        if generation != current + 1 {
            return Delta::Refetch;
        }
        let gone: HashSet<&str> = removed
            .iter()
            .map(String::as_str)
            .chain(added.iter().map(|t| t.path.as_str()))
            .collect();
        self.tracks.retain(|t| !gone.contains(t.path.as_str()));
        self.tracks.extend(added);
        self.tracks
            .sort_by(|a, b| Path::new(&a.path).cmp(Path::new(&b.path)));
        self.catalog = Catalog::build(&self.tracks);
        self.generation = Some(generation);
        Delta::Applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str, title: &str) -> Track {
        Track {
            path: path.into(),
            title: Some(title.into()),
            artist: Some("Ann".into()),
            album: None,
            track_number: None,
            disc_number: None,
            duration_ms: None,
            format: None,
        }
    }

    fn paths(local: &Local) -> Vec<&str> {
        local.tracks.iter().map(|t| t.path.as_str()).collect()
    }

    fn loaded(generation: u64, p: &[&str]) -> Local {
        let mut local = Local::default();
        let tracks = p.iter().map(|p| track(p, "t")).collect();
        assert_eq!(local.replace(generation, tracks), Delta::Applied);
        local
    }

    #[test]
    fn first_reply_loads() {
        let local = loaded(3, &["/m/a.flac", "/m/b.flac"]);
        assert_eq!(paths(&local), ["/m/a.flac", "/m/b.flac"]);
        assert_eq!(local.catalog, Catalog::build(&local.tracks));
        assert_ne!(local.catalog, Catalog::default());
    }

    #[test]
    fn older_reply_is_stale() {
        let mut local = loaded(3, &["/m/a.flac"]);
        assert_eq!(
            local.replace(2, vec![track("/m/old.flac", "t")]),
            Delta::Stale
        );
        assert_eq!(paths(&local), ["/m/a.flac"]);
        assert_eq!(
            local.replace(3, vec![track("/m/b.flac", "t")]),
            Delta::Applied
        );
        assert_eq!(paths(&local), ["/m/b.flac"]);
    }

    #[test]
    fn stale_event_is_ignored() {
        let mut local = loaded(3, &["/m/a.flac"]);
        for g in [2, 3] {
            let delta = local.apply(g, vec![track("/m/b.flac", "t")], vec!["/m/a.flac".into()]);
            assert_eq!(delta, Delta::Stale);
        }
        assert_eq!(paths(&local), ["/m/a.flac"]);
    }

    #[test]
    fn next_event_applies() {
        let mut local = loaded(3, &["/m/a.flac", "/m/c.flac"]);
        let delta = local.apply(4, vec![track("/m/b.flac", "t")], vec!["/m/c.flac".into()]);
        assert_eq!(delta, Delta::Applied);
        assert_eq!(paths(&local), ["/m/a.flac", "/m/b.flac"]);
        assert_eq!(local.catalog, Catalog::build(&local.tracks));
        assert_eq!(
            local.apply(4, vec![], vec!["/m/a.flac".into()]),
            Delta::Stale
        );
        assert_eq!(
            local.apply(5, vec![], vec!["/m/a.flac".into()]),
            Delta::Applied
        );
        assert_eq!(paths(&local), ["/m/b.flac"]);
    }

    #[test]
    fn gap_refetches() {
        let mut local = loaded(3, &["/m/a.flac"]);
        assert_eq!(
            local.apply(5, vec![track("/m/b.flac", "t")], vec![]),
            Delta::Refetch
        );
        assert_eq!(paths(&local), ["/m/a.flac"]);
        assert_eq!(
            local.replace(5, vec![track("/m/b.flac", "t")]),
            Delta::Applied
        );
        assert_eq!(local.apply(6, vec![], vec![]), Delta::Applied);
    }

    #[test]
    fn delta_upserts_by_path() {
        let mut local = loaded(1, &["/m/a.flac", "/m/b.flac", "/m/c.flac"]);
        let delta = local.apply(2, vec![track("/m/b.flac", "retagged")], vec![]);
        assert_eq!(delta, Delta::Applied);
        assert_eq!(paths(&local), ["/m/a.flac", "/m/b.flac", "/m/c.flac"]);
        assert_eq!(local.tracks[1].title.as_deref(), Some("retagged"));
    }

    #[test]
    fn delta_keeps_daemon_order() {
        let order = [
            "/m/AC/x.flac",
            "/m/AC DC/x.flac",
            "/m/a/x.flac",
            "/m/a-b/x.flac",
        ];
        let mut sorted = order.to_vec();
        sorted.sort_by(|a, b| std::path::Path::new(a).cmp(std::path::Path::new(b)));
        assert_eq!(sorted, order);
        let mut local = loaded(1, &[]);
        let added = [
            "/m/a-b/x.flac",
            "/m/AC DC/x.flac",
            "/m/a/x.flac",
            "/m/AC/x.flac",
        ];
        let added = added.iter().map(|p| track(p, "t")).collect();
        assert_eq!(local.apply(2, added, vec![]), Delta::Applied);
        assert_eq!(paths(&local), order);
        let fresh = loaded(2, &order);
        assert_eq!(local.tracks, fresh.tracks);
        assert_eq!(local.catalog, fresh.catalog);
    }

    #[test]
    fn older_reply_after_an_unloaded_event_refetches() {
        let mut local = Local::default();
        assert_eq!(local.apply(5, vec![], vec![]), Delta::Stale);
        assert_eq!(
            local.replace(4, vec![track("/m/a.flac", "t")]),
            Delta::Refetch
        );
        assert_eq!(paths(&local), ["/m/a.flac"]);
        assert_eq!(local.replace(5, vec![]), Delta::Applied);
    }

    #[test]
    fn clear_unloads() {
        let mut local = Local::default();
        local.apply(9, vec![], vec![]);
        local.replace(9, vec![track("/m/a.flac", "t")]);
        local.clear();
        assert!(local.tracks.is_empty());
        assert_eq!(local.catalog, Catalog::default());
        assert_eq!(local.replace(1, vec![]), Delta::Applied);
    }
}
