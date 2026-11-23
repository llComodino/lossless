//! The daemon as an MPRIS player on a private session bus. Each test starts
//! its own `dbus-daemon` and skips when there is none.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lossless_daemon::{Daemon, Mpris, bind};
use lossless_engine::fixtures::{BLOCK, Dir, PacedSink, flac_at};
use lossless_engine::{Mixer, Player, SoftwareMixer};
use lossless_proto::profile::Profile;
use lossless_proto::{
    Body, Command as Cmd, Outcome, Repeat, Request, ServerMessage, decode, encode,
};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedValue};

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const WAIT: Duration = Duration::from_secs(5);

/// A private bus, gone with the value.
struct Bus {
    child: Child,
    address: String,
    _dir: Dir,
}

impl Bus {
    fn start() -> Option<Bus> {
        let dir = Dir::new();
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:path={}", dir.path("bus").display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        Some(Bus {
            child,
            address: line.trim().to_string(),
            _dir: dir,
        })
    }

    fn client(&self) -> Connection {
        zbus::blocking::connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

macro_rules! bus_or_skip {
    () => {
        match Bus::start() {
            Some(b) => b,
            None => {
                eprintln!("skipped: no dbus-daemon");
                return;
            }
        }
    };
}

/// A daemon on a paced fake sink with two short tracks, on `bus`.
struct Server {
    dir: Dir,
    name: String,
}

impl Server {
    fn start(bus: &Bus, profile: &str, mixer: Option<Box<dyn Mixer>>) -> Server {
        let dir = Dir::new();
        std::fs::create_dir_all(dir.path("music/album")).unwrap();
        for n in ["01 one.flac", "02 two.flac"] {
            flac_at(&dir.path("music/album").join(n), 44_100, 16, 2, 30 * BLOCK);
        }
        let listener = bind(&dir.path("sock")).unwrap();
        let (player, events) = Player::spawn_with_mixer(PacedSink::default(), mixer);
        std::fs::create_dir_all(dir.path("state")).unwrap();
        let profile = Profile::parse(profile).unwrap_or_default();
        let name = if profile.is_default() {
            "org.mpris.MediaPlayer2.lossless".to_string()
        } else {
            format!("org.mpris.MediaPlayer2.lossless.{}", profile.name())
        };
        let daemon = Daemon::start_with_mpris(
            player,
            events,
            Some(dir.path("music")),
            dir.path("state/state.json"),
            Box::new(|| {}),
            Mpris {
                profile,
                address: Some(bus.address.clone()),
            },
        )
        .unwrap();
        thread::spawn(move || daemon.serve(listener));
        Server { dir, name }
    }

    fn sock(&self) -> PathBuf {
        self.dir.path("sock")
    }

    /// One request over the socket; the reply's outcome.
    fn ask(&self, command: Cmd) -> Outcome {
        let mut s = UnixStream::connect(self.sock()).unwrap();
        s.write_all(encode(&Request { id: 1, command }).unwrap().as_bytes())
            .unwrap();
        for line in BufReader::new(s.try_clone().unwrap()).lines() {
            if let Ok(ServerMessage::Reply { id: 1, result }) = decode(&line.unwrap()) {
                return result;
            }
        }
        panic!("no reply");
    }

    fn add_album(&self) {
        assert!(matches!(
            self.ask(Cmd::Add {
                paths: vec!["album".into()],
                at: None
            }),
            Outcome::Ok(_)
        ));
    }

    fn status(&self) -> lossless_proto::Status {
        match self.ask(Cmd::Status) {
            Outcome::Ok(Body::Status(s)) => *s,
            o => panic!("{o:?}"),
        }
    }

    fn player<'a>(&self, c: &'a Connection) -> Proxy<'a> {
        fresh(c, &self.name, PLAYER)
    }
}

/// A proxy that asks the daemon every time: zbus caches properties and
/// refreshes them only from change signals, and `Position` has none.
fn fresh<'a>(c: &'a Connection, name: &str, iface: &'static str) -> Proxy<'a> {
    zbus::blocking::proxy::Builder::new(c)
        .destination(name.to_string())
        .unwrap()
        .path(PATH)
        .unwrap()
        .interface(iface)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .unwrap()
}

fn until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn has_owner(c: &Connection, name: &str) -> bool {
    zbus::blocking::fdo::DBusProxy::new(c)
        .unwrap()
        .name_has_owner(name.try_into().unwrap())
        .unwrap()
}

fn status_of(p: &Proxy) -> String {
    p.get_property::<String>("PlaybackStatus").unwrap()
}

fn trackid(p: &Proxy) -> String {
    let m: HashMap<String, OwnedValue> = p.get_property("Metadata").unwrap();
    m.get("mpris:trackid")
        .map(|v| ObjectPath::try_from(v.clone()).unwrap().to_string())
        .unwrap_or_default()
}

#[test]
fn registers_default_and_profile_names() {
    let bus = bus_or_skip!();
    let a = Server::start(&bus, "default", None);
    let b = Server::start(&bus, "pw", None);
    let c = bus.client();
    until("names", || has_owner(&c, &a.name) && has_owner(&c, &b.name));
}

#[test]
fn root_properties() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "pw", None);
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    let root = fresh(&c, &s.name, "org.mpris.MediaPlayer2");
    assert_eq!(
        root.get_property::<String>("Identity").unwrap(),
        "lossless (pw)"
    );
    assert!(!root.get_property::<bool>("CanQuit").unwrap());
    assert!(!root.get_property::<bool>("HasTrackList").unwrap());
}

#[test]
fn controls_drive_the_player() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "default", None);
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    s.add_album();
    let p = s.player(&c);
    assert!(p.get_property::<bool>("CanGoNext").unwrap());
    p.call_method("PlayPause", &()).unwrap();
    until("playing", || status_of(&p) == "Playing");
    p.call_method("Next", &()).unwrap();
    until("track 1", || trackid(&p) == "/org/lossless/track/1");
    p.call_method("Pause", &()).unwrap();
    until("paused", || status_of(&p) == "Paused");
    p.call_method("Stop", &()).unwrap();
    until("stopped", || status_of(&p) == "Stopped");
}

#[test]
fn metadata_fields() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "default", None);
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    s.add_album();
    let p = s.player(&c);
    p.call_method("Play", &()).unwrap();
    until("track 0", || trackid(&p) == "/org/lossless/track/0");
    let m: HashMap<String, OwnedValue> = p.get_property("Metadata").unwrap();
    let url = String::try_from(m["xesam:url"].clone()).unwrap();
    assert!(
        url.starts_with("file://") && url.ends_with("01%20one.flac"),
        "{url}"
    );
    assert!(i64::try_from(m["mpris:length"].clone()).unwrap() > 0);
}

#[test]
fn position_reads_live() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "default", None);
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    s.add_album();
    let p = s.player(&c);
    p.call_method("Play", &()).unwrap();
    until("playing", || status_of(&p) == "Playing");
    let a: i64 = p.get_property("Position").unwrap();
    thread::sleep(Duration::from_millis(300));
    let b: i64 = p.get_property("Position").unwrap();
    assert!(b > a, "{a} -> {b}");
    p.call_method("Pause", &()).unwrap();
    until("paused", || status_of(&p) == "Paused");
    let a: i64 = p.get_property("Position").unwrap();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(p.get_property::<i64>("Position").unwrap(), a);
}

#[test]
fn seek_and_set_position() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "default", None);
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    s.add_album();
    let p = s.player(&c);
    p.call_method("Play", &()).unwrap();
    p.call_method("Pause", &()).unwrap();
    until("paused", || status_of(&p) == "Paused");
    let start: i64 = p.get_property("Position").unwrap();
    p.call_method("Seek", &(1_000_000i64)).unwrap();
    let pos: i64 = p.get_property("Position").unwrap();
    assert!((pos - start - 1_000_000).abs() < 50_000, "{start} -> {pos}");
    let wrong = ObjectPath::try_from("/org/lossless/track/9").unwrap();
    p.call_method("SetPosition", &(wrong, 0i64)).unwrap();
    assert_eq!(p.get_property::<i64>("Position").unwrap(), pos);
    let right = ObjectPath::try_from("/org/lossless/track/0").unwrap();
    p.call_method("SetPosition", &(right, 0i64)).unwrap();
    assert!(p.get_property::<i64>("Position").unwrap() < 50_000);
}

#[test]
fn loop_and_shuffle_round_trip() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "default", None);
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    let p = s.player(&c);
    p.set_property("LoopStatus", "Track").unwrap();
    assert_eq!(s.status().repeat, Repeat::One);
    assert_eq!(p.get_property::<String>("LoopStatus").unwrap(), "Track");
    p.set_property("Shuffle", true).unwrap();
    assert!(s.status().random);
    assert!(p.set_property("LoopStatus", "Bogus").is_err());
}

#[test]
fn volume_without_mixer_errors() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "default", None);
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    let p = s.player(&c);
    assert_eq!(p.get_property::<f64>("Volume").unwrap(), 1.0);
    assert!(p.set_property("Volume", 0.5).is_err());
}

#[test]
fn volume_with_software_mixer() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "default", Some(Box::new(SoftwareMixer::default())));
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    let p = s.player(&c);
    p.set_property("Volume", 0.4).unwrap();
    assert_eq!(s.status().volume, Some(40));
}

#[test]
fn open_uri_not_supported() {
    let bus = bus_or_skip!();
    let s = Server::start(&bus, "default", None);
    let c = bus.client();
    until("name", || has_owner(&c, &s.name));
    let e = s
        .player(&c)
        .call_method("OpenUri", &("file:///x",))
        .unwrap_err();
    assert!(e.to_string().contains("NotSupported"), "{e}");
}

#[test]
fn name_taken_keeps_serving() {
    let bus = bus_or_skip!();
    let a = Server::start(&bus, "default", None);
    let c = bus.client();
    until("name", || has_owner(&c, &a.name));
    let b = Server::start(&bus, "default", None);
    assert!(matches!(b.ask(Cmd::Status), Outcome::Ok(Body::Status(_))));
}
