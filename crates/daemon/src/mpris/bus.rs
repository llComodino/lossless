use std::collections::HashMap;
use std::sync::{Arc, Weak};

use lossless_engine::{Command as PlayerCmd, Snapshot};
use lossless_proto::profile::Profile;
use zbus::blocking::Connection;
use zbus::fdo;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use super::map::{self, Change};
use crate::State;

const PATH: &str = "/org/mpris/MediaPlayer2";

/// Where to register: the session bus, or `address` (tests).
pub struct Mpris {
    pub profile: Profile,
    pub address: Option<String>,
}

/// The daemon's connection, owning its name while it lives.
pub(crate) struct Bus {
    conn: Connection,
}

impl Bus {
    /// Serves both interfaces and requests the name; a taken name is an
    /// error, as is a missing bus.
    pub(crate) fn connect(m: &Mpris, state: Weak<State>) -> zbus::Result<Bus> {
        let builder = match &m.address {
            Some(a) => zbus::blocking::connection::Builder::address(a.as_str())?,
            None => zbus::blocking::connection::Builder::session()?,
        };
        let conn = builder
            .serve_at(
                PATH,
                Root {
                    identity: map::identity(&m.profile),
                },
            )?
            .serve_at(PATH, Player { state })?
            .name(map::bus_name(&m.profile))?
            .build()?;
        Ok(Bus { conn })
    }

    pub(crate) fn emit(&self, _changes: &[Change], _snap: &Snapshot) {
        let _ = &self.conn;
    }
}

struct Root {
    identity: String,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}

    fn quit(&self) {}

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn identity(&self) -> String {
        self.identity.clone()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    state: Weak<State>,
}

fn failed(e: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

fn owned<'a>(v: impl Into<Value<'a>>) -> Option<OwnedValue> {
    v.into().try_into_owned().ok()
}

/// The current entry's metadata; empty without one.
pub(crate) fn metadata(d: &State, snap: &Snapshot) -> HashMap<String, OwnedValue> {
    let mut m = HashMap::new();
    let Some(i) = snap.status.index else {
        return m;
    };
    let Some(path) = snap.queue.get(i) else {
        return m;
    };
    let t = d.track(path);
    let mut put = |k: &str, v: Option<OwnedValue>| {
        if let Some(v) = v {
            m.insert(k.to_string(), v);
        }
    };
    put(
        "mpris:trackid",
        ObjectPath::try_from(map::track_id(Some(i)))
            .ok()
            .and_then(owned),
    );
    let length = snap.status.duration_ms.or(t.duration_ms);
    put(
        "mpris:length",
        length
            .and_then(|ms| i64::try_from(ms.saturating_mul(1000)).ok())
            .and_then(owned),
    );
    put("xesam:title", t.title.and_then(owned));
    put("xesam:artist", t.artist.map(|a| vec![a]).and_then(owned));
    put("xesam:album", t.album.and_then(owned));
    put(
        "xesam:trackNumber",
        t.track_number
            .and_then(|n| i32::try_from(n).ok())
            .and_then(owned),
    );
    put(
        "xesam:url",
        owned(map::file_url(&path.display().to_string())),
    );
    m
}

impl Player {
    fn daemon(&self) -> fdo::Result<Arc<State>> {
        self.state
            .upgrade()
            .ok_or_else(|| fdo::Error::Failed("daemon stopping".into()))
    }

    fn send(&self, cmd: PlayerCmd) -> fdo::Result<()> {
        self.daemon()?.player.send(cmd).map_err(failed)
    }

    fn snap(&self) -> fdo::Result<Snapshot> {
        Ok(self.daemon()?.player.snapshot())
    }

    fn position_us(snap: &Snapshot) -> i64 {
        i64::try_from(snap.status.position_ms.saturating_mul(1000)).unwrap_or(i64::MAX)
    }

    fn seek_to_us(&self, us: i64) -> fdo::Result<()> {
        let ms = u64::try_from(us.max(0) / 1000).unwrap_or(0);
        self.send(PlayerCmd::Seek { ms })
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) -> fdo::Result<()> {
        self.send(PlayerCmd::Next)
    }

    fn previous(&self) -> fdo::Result<()> {
        self.send(PlayerCmd::Previous)
    }

    fn pause(&self) -> fdo::Result<()> {
        self.send(PlayerCmd::Pause)
    }

    fn play_pause(&self) -> fdo::Result<()> {
        self.send(PlayerCmd::Toggle)
    }

    fn stop(&self) -> fdo::Result<()> {
        self.send(PlayerCmd::Stop)
    }

    fn play(&self) -> fdo::Result<()> {
        self.send(PlayerCmd::Play(None))
    }

    /// Relative, in microseconds; past the end moves on, as the engine does.
    fn seek(&self, offset: i64) -> fdo::Result<()> {
        let snap = self.snap()?;
        if snap.status.index.is_none() {
            return Ok(());
        }
        self.seek_to_us(Self::position_us(&snap).saturating_add(offset))
    }

    /// Ignored unless `track_id` is the current entry and `position` is
    /// within it, as the spec asks.
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> fdo::Result<()> {
        let snap = self.snap()?;
        let length = snap
            .status
            .duration_ms
            .and_then(|ms| i64::try_from(ms.saturating_mul(1000)).ok())
            .unwrap_or(i64::MAX);
        if track_id.as_str() != map::track_id(snap.status.index)
            || snap.status.index.is_none()
            || !(0..=length).contains(&position)
        {
            return Ok(());
        }
        self.seek_to_us(position)
    }

    fn open_uri(&self, _uri: &str) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "lossless plays its queue only".into(),
        ))
    }

    #[zbus(property)]
    fn playback_status(&self) -> fdo::Result<String> {
        Ok(map::playback_status(self.snap()?.status.state).into())
    }

    #[zbus(property)]
    fn loop_status(&self) -> fdo::Result<String> {
        Ok(map::loop_status(self.snap()?.status.repeat).into())
    }

    #[zbus(property)]
    fn set_loop_status(&self, v: &str) -> fdo::Result<()> {
        let repeat = map::parse_loop(v)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("LoopStatus {v:?}")))?;
        self.send(PlayerCmd::SetMode {
            repeat: Some(repeat),
            random: None,
        })
    }

    #[zbus(property)]
    fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.snap()?.status.random)
    }

    #[zbus(property)]
    fn set_shuffle(&self, v: bool) -> fdo::Result<()> {
        self.send(PlayerCmd::SetMode {
            repeat: None,
            random: Some(v),
        })
    }

    #[zbus(property)]
    fn volume(&self) -> fdo::Result<f64> {
        Ok(self.snap()?.status.volume.map_or(1.0, map::volume_to_mpris))
    }

    #[zbus(property)]
    fn set_volume(&self, v: f64) -> fdo::Result<()> {
        if self.snap()?.status.volume.is_none() {
            return Err(fdo::Error::NotSupported("no mixer".into()));
        }
        self.send(PlayerCmd::SetVolume(map::volume_from_mpris(v)))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> fdo::Result<i64> {
        Ok(Self::position_us(&self.snap()?))
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn set_rate(&self, _v: f64) {}

    #[zbus(property(emits_changed_signal = "const"))]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        let d = self.daemon()?;
        Ok(metadata(&d, &d.player.snapshot()))
    }

    #[zbus(property)]
    fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(!self.snap()?.queue.is_empty())
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(!self.snap()?.queue.is_empty())
    }

    #[zbus(property)]
    fn can_play(&self) -> fdo::Result<bool> {
        Ok(!self.snap()?.queue.is_empty())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}
