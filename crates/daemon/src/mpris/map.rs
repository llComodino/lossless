use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use lossless_proto::profile::Profile;
use lossless_proto::{Event, PlayState, Repeat};

/// `org.mpris.MediaPlayer2.lossless[.<profile>]`. A bus name element may
/// not start with a digit, and `-` is discouraged, so `-` becomes `_` and a
/// leading digit gets a `_` before it.
pub(crate) fn bus_name(p: &Profile) -> String {
    let base = "org.mpris.MediaPlayer2.lossless";
    if p.is_default() {
        return base.into();
    }
    let elem: String = p
        .name()
        .chars()
        .map(|c| if c == '-' { '_' } else { c })
        .collect();
    let lead = if elem.starts_with(|c: char| c.is_ascii_digit()) {
        "_"
    } else {
        ""
    };
    format!("{base}.{lead}{elem}")
}

pub(crate) fn identity(p: &Profile) -> String {
    if p.is_default() {
        "lossless".into()
    } else {
        format!("lossless ({})", p.name())
    }
}

pub(crate) fn loop_status(r: Repeat) -> &'static str {
    match r {
        Repeat::Off => "None",
        Repeat::Queue => "Playlist",
        Repeat::One => "Track",
    }
}

pub(crate) fn parse_loop(s: &str) -> Option<Repeat> {
    match s {
        "None" => Some(Repeat::Off),
        "Playlist" => Some(Repeat::Queue),
        "Track" => Some(Repeat::One),
        _ => None,
    }
}

pub(crate) fn playback_status(s: PlayState) -> &'static str {
    match s {
        PlayState::Playing => "Playing",
        PlayState::Paused => "Paused",
        PlayState::Stopped => "Stopped",
    }
}

pub(crate) fn volume_to_mpris(percent: u8) -> f64 {
    f64::from(percent) / 100.0
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0..=100"
)]
pub(crate) fn volume_from_mpris(v: f64) -> Option<u8> {
    (!v.is_nan()).then(|| (v.clamp(0.0, 1.0) * 100.0).round() as u8)
}

pub(crate) fn track_id(index: Option<usize>) -> String {
    match index {
        Some(i) => format!("/org/lossless/track/{i}"),
        None => "/org/mpris/MediaPlayer2/TrackList/NoTrack".into(),
    }
}

/// `file://` plus `path`, every byte outside `A-Za-z0-9-._~/` encoded.
pub(crate) fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            url.push(char::from(b));
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// What an event batch changed, for signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    PlaybackStatus,
    Metadata,
    /// CanGoNext, CanGoPrevious, CanPlay.
    Queue,
    /// LoopStatus, Shuffle.
    Modes,
    Volume,
    Seeked(u64),
}

/// What the last events said, to tell a seek from a state change.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seen {
    pub state: PlayState,
    pub index: Option<usize>,
    /// The track the last `State` event belonged to: a `State` after a
    /// track change, even in a later batch, is not a seek.
    pub index_at_state: Option<usize>,
}

impl Default for Seen {
    fn default() -> Seen {
        Seen {
            state: PlayState::Stopped,
            index: None,
            index_at_state: None,
        }
    }
}

pub(crate) fn changes(seen: &mut Seen, batch: &[Event]) -> Vec<Change> {
    let mut out = Vec::new();
    for e in batch {
        match e {
            Event::TrackChanged { index } => {
                seen.index = *index;
                out.extend([Change::Metadata, Change::Queue]);
            }
            Event::QueueChanged => out.extend([Change::Queue, Change::Metadata]),
            Event::Mode { .. } => out.push(Change::Modes),
            Event::Volume { .. } => out.push(Change::Volume),
            Event::State { state, position_ms } => {
                let new_track = seen.index != seen.index_at_state;
                if *state != seen.state {
                    out.push(Change::PlaybackStatus);
                }
                if new_track {
                    if *position_ms > 0 {
                        out.push(Change::Seeked(*position_ms));
                    }
                } else if *state == seen.state {
                    out.push(Change::Seeked(*position_ms));
                }
                seen.state = *state;
                seen.index_at_state = seen.index;
            }
            Event::LibraryChanged { .. } | Event::Error(_) => {}
        }
    }
    let mut unique = Vec::new();
    for c in out {
        if !unique.contains(&c) {
            unique.push(c);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> Profile {
        Profile::parse(name).unwrap()
    }

    #[test]
    fn bus_name_default() {
        assert_eq!(
            bus_name(&Profile::default()),
            "org.mpris.MediaPlayer2.lossless"
        );
    }

    #[test]
    fn bus_name_profiles() {
        for (name, want) in [
            ("pw", "lossless.pw"),
            ("a-b", "lossless.a_b"),
            ("2ch", "lossless._2ch"),
            ("x_9", "lossless.x_9"),
        ] {
            assert_eq!(bus_name(&p(name)), format!("org.mpris.MediaPlayer2.{want}"));
        }
        for name in ["a".repeat(32), "9".repeat(32), "-".repeat(32)] {
            let n = bus_name(&p(&name));
            assert!(
                zbus::names::WellKnownName::try_from(n.as_str()).is_ok(),
                "{n}"
            );
        }
    }

    #[test]
    fn identity_names() {
        assert_eq!(identity(&Profile::default()), "lossless");
        assert_eq!(identity(&p("pw")), "lossless (pw)");
    }

    #[test]
    fn loop_round_trips() {
        for r in [Repeat::Off, Repeat::Queue, Repeat::One] {
            assert_eq!(parse_loop(loop_status(r)), Some(r));
        }
        assert_eq!(loop_status(Repeat::Off), "None");
        assert_eq!(loop_status(Repeat::Queue), "Playlist");
        assert_eq!(loop_status(Repeat::One), "Track");
        assert_eq!(parse_loop("x"), None);
    }

    #[test]
    fn playback_states() {
        assert_eq!(playback_status(PlayState::Playing), "Playing");
        assert_eq!(playback_status(PlayState::Paused), "Paused");
        assert_eq!(playback_status(PlayState::Stopped), "Stopped");
    }

    #[test]
    fn volume_from_mpris_clamps() {
        assert_eq!(volume_from_mpris(0.5), Some(50));
        assert_eq!(volume_from_mpris(-1.0), Some(0));
        assert_eq!(volume_from_mpris(7.0), Some(100));
        assert_eq!(volume_from_mpris(f64::NAN), None);
        assert_eq!(volume_from_mpris(0.333), Some(33));
        assert_eq!(volume_to_mpris(100), 1.0);
        assert_eq!(volume_to_mpris(40), 0.4);
    }

    #[test]
    fn track_ids() {
        assert_eq!(track_id(Some(3)), "/org/lossless/track/3");
        assert_eq!(track_id(None), "/org/mpris/MediaPlayer2/TrackList/NoTrack");
        assert!(zbus::zvariant::ObjectPath::try_from(track_id(Some(usize::MAX))).is_ok());
    }

    #[test]
    fn file_url_encodes() {
        assert_eq!(
            file_url(Path::new("/m/A B/01 #1 100%.flac")),
            "file:///m/A%20B/01%20%231%20100%25.flac"
        );
        assert_eq!(
            file_url(Path::new("/m/Bj\u{f6}rk.flac")),
            "file:///m/Bj%C3%B6rk.flac"
        );
        assert_eq!(file_url(Path::new("/m/a-b_c.d~e")), "file:///m/a-b_c.d~e");
    }

    #[test]
    fn file_url_keeps_non_utf8_bytes() {
        use std::ffi::OsStr;
        let p = Path::new(OsStr::from_bytes(b"/m/Bj\xf6rk.flac"));
        assert_eq!(file_url(p), "file:///m/Bj%F6rk.flac");
    }

    fn state(state: PlayState, position_ms: u64) -> Event {
        Event::State { state, position_ms }
    }

    fn track(i: usize) -> Event {
        Event::TrackChanged { index: Some(i) }
    }

    fn playing_at(index: usize) -> Seen {
        Seen {
            state: PlayState::Playing,
            index: Some(index),
            index_at_state: Some(index),
        }
    }

    #[test]
    fn state_change_is_playback_status() {
        let mut s = Seen::default();
        assert_eq!(
            changes(&mut s, &[state(PlayState::Playing, 0)]),
            [Change::PlaybackStatus]
        );
    }

    #[test]
    fn same_state_is_a_seek() {
        let mut s = playing_at(0);
        assert_eq!(
            changes(&mut s, &[state(PlayState::Playing, 60_000)]),
            [Change::Seeked(60_000)]
        );
    }

    #[test]
    fn paused_seek_is_a_seek() {
        let mut s = Seen {
            state: PlayState::Paused,
            ..playing_at(0)
        };
        assert_eq!(
            changes(&mut s, &[state(PlayState::Paused, 5_000)]),
            [Change::Seeked(5_000)]
        );
    }

    #[test]
    fn track_change_then_state_in_one_batch() {
        let mut s = playing_at(0);
        assert_eq!(
            changes(&mut s, &[track(1), state(PlayState::Playing, 0)]),
            [Change::Metadata, Change::Queue]
        );
    }

    #[test]
    fn split_batch_track_change_is_not_a_seek() {
        let mut s = playing_at(0);
        assert_eq!(
            changes(&mut s, &[track(1)]),
            [Change::Metadata, Change::Queue]
        );
        assert_eq!(changes(&mut s, &[state(PlayState::Playing, 0)]), []);
        assert_eq!(
            changes(&mut s, &[state(PlayState::Playing, 9_000)]),
            [Change::Seeked(9_000)]
        );
    }

    #[test]
    fn track_change_with_offset_is_seeked() {
        let mut s = Seen::default();
        assert_eq!(
            changes(&mut s, &[track(2), state(PlayState::Paused, 30_000)]),
            [
                Change::Metadata,
                Change::Queue,
                Change::PlaybackStatus,
                Change::Seeked(30_000)
            ]
        );
    }

    #[test]
    fn modes_volume_queue() {
        let mut s = Seen::default();
        let mode = Event::Mode {
            repeat: Repeat::One,
            random: true,
        };
        assert_eq!(changes(&mut s, &[mode]), [Change::Modes]);
        assert_eq!(
            changes(&mut s, &[Event::Volume { percent: 3 }]),
            [Change::Volume]
        );
        assert_eq!(
            changes(&mut s, &[Event::QueueChanged]),
            [Change::Queue, Change::Metadata]
        );
        let lib = Event::LibraryChanged {
            generation: 1,
            tracks: 0,
            added: vec![],
            removed: vec![],
        };
        assert_eq!(changes(&mut s, &[lib]), []);
    }

    #[test]
    fn changes_are_deduped() {
        let mut s = Seen::default();
        assert_eq!(
            changes(
                &mut s,
                &[Event::QueueChanged, track(0), Event::QueueChanged]
            ),
            [Change::Queue, Change::Metadata]
        );
    }
}
