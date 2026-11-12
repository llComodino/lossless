//! The daemon over a real unix socket. The player runs on a real-time fake
//! sink, so timing behaves like a DAC without needing one.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use lossless_daemon::{Daemon, MAX_REQUEST, bind, parse_config};
use lossless_engine::Player;
use lossless_engine::fixtures::{BLOCK, Dir, PacedSink, flac_at};
use lossless_proto::{
    Body, Command, ErrorCode, Event, Outcome, PlayState, Repeat, Request, ServerMessage, WireError,
    decode, encode,
};

const WAIT: Duration = Duration::from_secs(10);

/// A daemon serving on `dir/sock`, with music in `dir/music`.
struct Server {
    dir: Dir,
}

impl Server {
    fn start(with_music: bool) -> Self {
        let dir = Dir::new();
        let music = dir.path("music");
        for d in ["album", ".incomplete", "empty"] {
            std::fs::create_dir_all(music.join(d)).unwrap();
        }
        let listener = bind(&dir.path("sock")).unwrap();
        let (player, events) = Player::spawn_with(PacedSink::default());
        let daemon =
            Daemon::start(player, events, with_music.then_some(music), Box::new(|| {})).unwrap();
        thread::spawn(move || daemon.serve(listener));
        Self { dir }
    }

    fn music(&self) -> PathBuf {
        self.dir.path("music")
    }

    fn socket(&self) -> PathBuf {
        self.dir.path("sock")
    }

    fn connect(&self) -> Client {
        Client::connect(&self.socket())
    }

    /// Two 16/44.1 tracks of `blocks` blocks in `album/`, plus files `add`
    /// must skip. 20 blocks play for about 1.9 s each.
    fn album(&self, blocks: usize) -> Vec<PathBuf> {
        let m = self.music();
        std::fs::write(m.join("album/cover.jpg"), b"jpeg").unwrap();
        std::fs::write(m.join(".incomplete/x.flac"), b"partial").unwrap();
        ["01 one.flac", "02 two.flac"]
            .iter()
            .map(|n| flac_at(&m.join("album").join(n), 44_100, 16, 2, blocks * BLOCK).path)
            .collect()
    }
}

/// A protocol client that keeps events it reads while waiting for replies.
struct Client {
    writer: UnixStream,
    lines: Receiver<String>,
    next_id: u64,
    events: Vec<Event>,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).unwrap();
        let reader = BufReader::new(writer.try_clone().unwrap());
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            writer,
            lines,
            next_id: 1,
            events: Vec::new(),
        }
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
    }

    fn recv(&mut self) -> ServerMessage {
        let line = self
            .lines
            .recv_timeout(WAIT)
            .expect("no message from daemon");
        decode(&line).unwrap()
    }

    /// Next reply, keeping any events that arrive first.
    fn reply(&mut self) -> (u64, Outcome) {
        loop {
            match self.recv() {
                ServerMessage::Reply { id, result } => return (id, result),
                ServerMessage::Event { event } => self.events.push(event),
            }
        }
    }

    fn call(&mut self, command: Command) -> Outcome {
        let id = self.next_id;
        self.next_id += 1;
        self.raw(encode(&Request { id, command }).unwrap().as_bytes());
        let (got, result) = self.reply();
        assert_eq!(got, id, "reply for the wrong request");
        result
    }

    fn ok(&mut self, command: Command) -> Body {
        match self.call(command) {
            Outcome::Ok(body) => body,
            Outcome::Err(e) => panic!("unexpected error: {e}"),
        }
    }

    fn err(&mut self, command: Command) -> WireError {
        match self.call(command) {
            Outcome::Err(e) => e,
            Outcome::Ok(body) => panic!("unexpected success: {body:?}"),
        }
    }

    /// Wait for an event matching `want`, keeping the ones before it.
    fn wait_for(&mut self, want: impl Fn(&Event) -> bool) -> Event {
        if let Some(i) = self.events.iter().position(&want) {
            return self.events.remove(i);
        }
        loop {
            if let ServerMessage::Event { event } = self.recv() {
                if want(&event) {
                    return event;
                }
                self.events.push(event);
            }
        }
    }
}

fn status(c: &mut Client) -> lossless_proto::Status {
    match c.ok(Command::Status) {
        Body::Status(s) => *s,
        other => panic!("not a status: {other:?}"),
    }
}

fn queue(c: &mut Client) -> Vec<String> {
    match c.ok(Command::Queue) {
        Body::Queue(tracks) => tracks.into_iter().map(|t| t.path).collect(),
        other => panic!("not a queue: {other:?}"),
    }
}

#[test]
fn fresh_daemon_is_stopped_and_empty() {
    let s = Server::start(true);
    let mut c = s.connect();
    let st = status(&mut c);
    assert_eq!(st.state, PlayState::Stopped);
    assert_eq!((st.index, st.output, st.track), (None, None, None));
    assert!(queue(&mut c).is_empty());
}

#[test]
fn add_resolves_folders_relative_to_music_dir() {
    let s = Server::start(true);
    let files = s.album(1);
    let mut c = s.connect();
    c.ok(Command::Add {
        paths: vec!["album".into()],
        at: None,
    });
    let q = queue(&mut c);
    let want: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
    assert_eq!(q, want, "sorted, audio only, no hidden or cover files");
    // Absolute paths work too.
    c.ok(Command::Add {
        paths: vec![files[0].display().to_string()],
        at: None,
    });
    assert_eq!(queue(&mut c).len(), 3);
}

#[test]
fn add_reports_errors() {
    let s = Server::start(true);
    s.album(1);
    let mut c = s.connect();
    assert!(
        c.err(Command::Add {
            paths: vec!["nope".into()],
            at: None,
        })
        .message
        .contains("nope")
    );
    let e = c.err(Command::Add {
        paths: vec!["empty".into(), "empty".into()],
        at: None,
    });
    assert_eq!(e.code, ErrorCode::NotFound);
    assert_eq!(e.message, "no playable files in empty, empty");
    let e = c.err(Command::Add {
        paths: vec![],
        at: None,
    });
    assert_eq!(
        (e.code, e.message.as_str()),
        (ErrorCode::NotFound, "no playable files")
    );
    let e = c.err(Command::Add {
        paths: vec!["album/cover.jpg".into()],
        at: None,
    });
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(e.message.ends_with("cover.jpg: not a flac or mp3 file"));
    // A failing path rejects the whole request.
    c.err(Command::Add {
        paths: vec!["album".into(), "nope".into()],
        at: None,
    });
    assert!(queue(&mut c).is_empty());
}

#[test]
fn play_past_the_queue_is_out_of_range() {
    let s = Server::start(true);
    s.album(1);
    let mut c = s.connect();
    c.ok(Command::Add {
        paths: vec!["album".into()],
        at: None,
    });
    let e = c.err(Command::Play { index: Some(5) });
    assert_eq!(e.code, ErrorCode::OutOfRange);
    assert_eq!(e.message, "index 5 out of range: queue has 2 entries");
}

#[test]
fn error_codes_name_the_problem() {
    let s = Server::start(true);
    let mut c = s.connect();
    for line in [&b"garbage\n"[..], b"\xff\xfe\n"] {
        c.raw(line);
        let (id, result) = c.reply();
        assert_eq!(id, 0);
        assert!(matches!(result, Outcome::Err(e) if e.code == ErrorCode::BadRequest));
    }
    let e = c.err(Command::Add {
        paths: vec!["nope".into()],
        at: None,
    });
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = c.err(Command::Play { index: Some(9) });
    assert_eq!(e.code, ErrorCode::OutOfRange);
    assert_eq!(e.message, "index 9 out of range: queue has 0 entries");
    let none = Server::start(false);
    let e = none.connect().err(Command::Update);
    assert_eq!(e.code, ErrorCode::NoMusicDir);
}

#[test]
fn update_scans_and_search_finds() {
    let s = Server::start(true);
    s.album(1);
    let mut c = s.connect();
    c.ok(Command::Subscribe);
    c.ok(Command::Update);
    let ev = c.wait_for(|e| matches!(e, Event::LibraryChanged { .. }));
    assert_eq!(ev, Event::LibraryChanged { tracks: 2 });
    let hits = match c.ok(Command::Search {
        query: "TWO".into(),
    }) {
        Body::Tracks(t) => t,
        other => panic!("{other:?}"),
    };
    assert_eq!(hits.len(), 1);
    assert!(hits[0].path.ends_with("02 two.flac"));
    let f = hits[0].format.expect("format read from the file");
    assert_eq!((f.rate, f.bits, f.channels), (44_100, 16, 2));
    assert!(hits[0].duration_ms.is_some());
    match c.ok(Command::Search { query: "".into() }) {
        Body::Tracks(t) => assert!(t.is_empty()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn update_without_music_dir_is_an_error() {
    let s = Server::start(false);
    let mut c = s.connect();
    let e = c.err(Command::Update);
    assert_eq!(
        (e.code, e.message.as_str()),
        (ErrorCode::NoMusicDir, "no music_dir configured")
    );
}

#[test]
fn playback_events_reach_subscribers_in_order() {
    let s = Server::start(true);
    s.album(1);
    let mut c = s.connect();
    c.ok(Command::Subscribe);
    c.ok(Command::Add {
        paths: vec!["album".into()],
        at: None,
    });
    c.ok(Command::Play { index: None });
    c.wait_for(|e| *e == Event::TrackChanged { index: None });
    let order: Vec<Event> = c
        .events
        .iter()
        .filter(|e| !matches!(e, Event::QueueChanged))
        .cloned()
        .collect();
    assert_eq!(order.first(), Some(&Event::TrackChanged { index: Some(0) }));
    assert!(order.contains(&Event::TrackChanged { index: Some(1) }));
    assert!(order.contains(&Event::State {
        state: PlayState::Playing,
        position_ms: 0
    }));
    // Events go out after the status they describe is published.
    let st = status(&mut c);
    assert_eq!((st.state, st.index), (PlayState::Stopped, None));
}

#[test]
fn status_names_the_current_track() {
    let s = Server::start(true);
    let files = s.album(20);
    let mut c = s.connect();
    c.ok(Command::Add {
        paths: vec!["album".into()],
        at: None,
    });
    c.ok(Command::Next);
    assert_eq!(status(&mut c).index, None, "next with nothing loaded");
    c.ok(Command::Play { index: Some(1) });
    let st = status(&mut c);
    assert_eq!((st.state, st.index), (PlayState::Playing, Some(1)));
    let t = st.track.expect("current track");
    assert_eq!(t.path, files[1].display().to_string());
    assert_eq!(t.format.map(|f| f.rate), Some(44_100));
    c.ok(Command::Pause);
    let paused = status(&mut c);
    assert_eq!(paused.state, PlayState::Paused);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        status(&mut c).position_ms,
        paused.position_ms,
        "position holds while paused"
    );
    c.ok(Command::Seek { ms: 1_000 });
    assert_eq!(status(&mut c).position_ms, 1_000);
    c.ok(Command::Stop);
    let st = status(&mut c);
    assert_eq!((st.state, st.output), (PlayState::Stopped, None));
    c.ok(Command::Clear);
    assert!(queue(&mut c).is_empty());
}

#[test]
fn bad_requests_get_errors_and_the_connection_survives() {
    let s = Server::start(true);
    let mut c = s.connect();
    for line in [
        &b"not json\n"[..],
        b"{\"id\":5,\"cmd\":\"fly\"}\n",
        b"{\"cmd\":\"status\"}\n",
        b"\xff\xfe\xfd\n",
        b"[1,2,3]\n",
    ] {
        c.raw(line);
        let (id, result) = c.reply();
        assert_eq!(id, 0);
        match result {
            Outcome::Err(e) => assert_eq!(e.code, ErrorCode::BadRequest, "{line:?}"),
            Outcome::Ok(_) => panic!("{line:?} accepted"),
        }
    }
    c.raw(b"\n   \n");
    assert_eq!(status(&mut c).state, PlayState::Stopped);
}

#[test]
fn oversized_request_closes_only_that_connection() {
    let s = Server::start(true);
    let mut big = UnixStream::connect(s.socket()).unwrap();
    big.set_read_timeout(Some(WAIT)).unwrap();
    big.write_all(&vec![b'x'; MAX_REQUEST + 1]).unwrap();
    let mut out = Vec::new();
    big.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("\"code\":\"bad_request\""), "{text}");
    assert!(text.contains("longer than"), "{text}");
    assert_eq!(status(&mut s.connect()).state, PlayState::Stopped);
}

#[test]
fn a_dead_subscriber_does_not_stop_the_others() {
    let s = Server::start(true);
    s.album(1);
    let mut gone = s.connect();
    gone.ok(Command::Subscribe);
    drop(gone);
    let mut c = s.connect();
    c.ok(Command::Subscribe);
    c.ok(Command::Add {
        paths: vec!["album".into()],
        at: None,
    });
    c.wait_for(|e| *e == Event::QueueChanged);
    c.ok(Command::Add {
        paths: vec!["album".into()],
        at: None,
    });
    c.wait_for(|e| *e == Event::QueueChanged);
}

#[test]
fn blank_lines_are_skipped() {
    let s = Server::start(false);
    let mut c = s.connect();
    c.raw(b"\n   \n\t\n");
    // `call` checks the reply id: blank lines produced no reply of their own.
    assert_eq!(status(&mut c).state, PlayState::Stopped);
}

#[test]
fn queued_library_tracks_come_from_the_index() {
    let s = Server::start(true);
    s.album(1);
    let mut c = s.connect();
    c.ok(Command::Subscribe);
    c.ok(Command::Update);
    c.wait_for(|e| matches!(e, Event::LibraryChanged { .. }));
    let Body::Tracks(indexed) = c.ok(Command::Search {
        query: "one".into(),
    }) else {
        panic!("not tracks")
    };
    c.ok(Command::Add {
        paths: vec!["album/01 one.flac".into()],
        at: None,
    });
    let Body::Queue(queued) = c.ok(Command::Queue) else {
        panic!("not a queue")
    };
    assert_eq!(queued, indexed);
}

#[test]
fn tracks_outside_the_library_are_read_on_demand() {
    let s = Server::start(false);
    let files = s.album(1);
    let mut c = s.connect();
    c.ok(Command::Add {
        paths: vec![files[0].display().to_string()],
        at: None,
    });
    match c.ok(Command::Queue) {
        Body::Queue(tracks) => {
            assert_eq!(tracks.len(), 1);
            assert!(tracks[0].duration_ms.is_some(), "not read: {:?}", tracks[0]);
            assert!(tracks[0].format.is_some());
        }
        other => panic!("not a queue: {other:?}"),
    }
}

#[test]
fn bind_refuses_a_live_daemon_and_replaces_a_stale_socket() {
    let s = Server::start(true);
    assert!(bind(&s.socket()).is_err(), "took over a live daemon");
    let stale = s.dir.path("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert!(stale.exists());
    assert!(bind(&stale).is_ok(), "stale socket not replaced");
}

#[test]
fn bind_never_removes_anything_but_a_socket() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new();
    let key = dir.path("id_ed25519");
    std::fs::write(&key, b"secret").unwrap();
    let err = bind(&key).unwrap_err().to_string();
    assert!(err.contains("not a socket"), "{err}");
    assert_eq!(std::fs::read(&key).unwrap(), b"secret");

    let link = dir.path("link.sock");
    std::os::unix::fs::symlink(&key, &link).unwrap();
    assert!(bind(&link).is_err(), "followed a symlink");
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(std::fs::read(&key).unwrap(), b"secret");

    let dangling = dir.path("dangling.sock");
    std::os::unix::fs::symlink(dir.path("nowhere"), &dangling).unwrap();
    assert!(bind(&dangling).is_err(), "replaced a dangling symlink");
    assert!(!dir.path("nowhere").exists());

    let under_file = key.join("x.sock");
    assert!(
        bind(&under_file)
            .unwrap_err()
            .to_string()
            .contains("checking")
    );

    std::fs::create_dir(dir.path("d")).unwrap();
    assert!(bind(&dir.path("d")).is_err());
    assert!(dir.path("d").is_dir());

    let long = dir.path(&"s".repeat(120));
    assert!(bind(&long).unwrap_err().to_string().contains("binding"));

    let locked = dir.path("locked");
    std::fs::create_dir(&locked).unwrap();
    drop(std::os::unix::net::UnixListener::bind(locked.join("stale.sock")).unwrap());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let err = bind(&locked.join("stale.sock")).map(drop);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(err.unwrap_err().to_string().contains("removing stale"));

    let sock = dir.path("ok.sock");
    let _l = bind(&sock).unwrap();
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "socket mode {mode:o}");
}

#[test]
fn config_parsing() {
    let home = std::env::var("HOME").unwrap();
    let cfg = parse_config("").unwrap();
    assert_eq!(cfg.device, "default");
    assert_eq!((cfg.music_dir, cfg.socket), (None, None));
    let cfg = parse_config(
        "device = \"hw:CARD=PRO2,DEV=0\"\nmusic_dir = \"~/m\"\nsocket = \"/run/x.sock\"",
    )
    .unwrap();
    assert_eq!(cfg.device, "hw:CARD=PRO2,DEV=0");
    assert_eq!(cfg.music_dir, Some(Path::new(&home).join("m")));
    assert_eq!(cfg.socket, Some(PathBuf::from("/run/x.sock")));
    assert!(parse_config("devise = \"x\"").is_err(), "typo accepted");
    assert!(parse_config("device = ").is_err());
}

#[test]
fn queue_edits_over_the_socket() {
    let s = Server::start(true);
    let files = s.album(1);
    let names: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
    let mut c = s.connect();
    let add = |paths: &[&str], at| Command::Add {
        paths: paths.iter().map(|p| p.to_string()).collect(),
        at,
    };
    c.ok(add(&["album/02 two.flac"], None));
    c.ok(add(&["album/01 one.flac"], Some(0)));
    assert_eq!(queue(&mut c), names);
    let e = c.err(add(&["album/01 one.flac"], Some(3)));
    assert_eq!(e.code, ErrorCode::OutOfRange);
    c.ok(Command::Move { from: 0, to: 1 });
    assert_eq!(queue(&mut c), [names[1].clone(), names[0].clone()]);
    let e = c.err(Command::Move { from: 0, to: 2 });
    assert_eq!(e.code, ErrorCode::OutOfRange);
    let e = c.err(Command::Remove {
        indices: vec![0, 0],
    });
    assert_eq!(e.code, ErrorCode::BadRequest);
    assert_eq!(queue(&mut c).len(), 2);
    c.ok(Command::Remove { indices: vec![0] });
    assert_eq!(queue(&mut c), [names[0].clone()]);
    let e = c.err(Command::Remove { indices: vec![5] });
    assert_eq!(e.code, ErrorCode::OutOfRange);
    let e = c.err(Command::Replace {
        paths: vec!["empty".into()],
    });
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = c.err(Command::Replace { paths: vec![] });
    assert_eq!(e.code, ErrorCode::NotFound);
    assert_eq!(queue(&mut c), [names[0].clone()], "queue untouched");
    c.ok(Command::Replace {
        paths: vec!["album".into()],
    });
    assert_eq!(queue(&mut c), names);
}

#[test]
fn modes_over_the_socket() {
    let s = Server::start(true);
    let mut c = s.connect();
    let st = status(&mut c);
    assert_eq!((st.repeat, st.random), (Repeat::Off, false));
    c.ok(Command::Subscribe);
    for (repeat, random, want) in [
        (Some(Repeat::One), None, (Repeat::One, false)),
        (None, Some(true), (Repeat::One, true)),
        (None, None, (Repeat::One, true)),
    ] {
        c.ok(Command::SetMode { repeat, random });
        let st = status(&mut c);
        assert_eq!((st.repeat, st.random), want);
        let ev = c.wait_for(|e| matches!(e, Event::Mode { .. }));
        assert_eq!(
            ev,
            Event::Mode {
                repeat: want.0,
                random: want.1
            }
        );
    }
    c.ok(Command::Clear);
    c.wait_for(|e| *e == Event::QueueChanged);
    assert!(
        !c.events.iter().any(|e| matches!(e, Event::Mode { .. })),
        "one mode event per set_mode: {:?}",
        c.events
    );
}
