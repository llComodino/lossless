//! The real `losslessd-tui` in an 80x24 pseudo-terminal against an in-process
//! daemon on a real-time fake sink.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lossless_daemon::{Daemon, bind};
use lossless_engine::Player;
use lossless_engine::fixtures::{BLOCK, Dir, PacedSink, flac_at};
use lossless_proto::{Body, Command, Outcome, PlayState, Request, ServerMessage, decode, encode};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const WAIT: Duration = Duration::from_secs(10);

struct Kill(Box<dyn portable_pty::Child + Send + Sync>);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ask(sock: &std::path::Path, command: Command) -> Body {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let req = Request { id: 1, command };
    s.write_all(encode(&req).unwrap().as_bytes()).unwrap();
    for line in BufReader::new(s).lines() {
        let line = line.expect("reading the reply");
        if let ServerMessage::Reply { id: 1, result } = decode(&line).unwrap() {
            match result {
                Outcome::Ok(body) => return body,
                other => panic!("{other:?}"),
            }
        }
    }
    panic!("no reply");
}

fn status(sock: &std::path::Path) -> PlayState {
    match ask(sock, Command::Status) {
        Body::Status(st) => st.state,
        other => panic!("{other:?}"),
    }
}

fn wait_for(rx: &Receiver<Vec<u8>>, seen: &mut Vec<u8>, want: &str) {
    let end = Instant::now() + WAIT;
    while !String::from_utf8_lossy(seen).contains(want) {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(chunk) => seen.extend(chunk),
            Err(_) => panic!("no {want:?} on screen: {}", String::from_utf8_lossy(seen)),
        }
    }
}

#[test]
fn keys_drive_the_daemon() {
    let dir = Dir::new();
    let album = dir.path("music/album");
    std::fs::create_dir_all(&album).unwrap();
    for n in ["01 one.flac", "02 two.flac"] {
        flac_at(&album.join(n), 44_100, 16, 2, 50 * BLOCK);
    }
    let sock = dir.path("sock");
    let listener = bind(&sock).unwrap();
    let (player, events) = Player::spawn_with_mixer(PacedSink::default(), None);
    let daemon = Daemon::start(
        player,
        events,
        Some(dir.path("music")),
        dir.path("state.json"),
        Box::new(|| {}),
    )
    .unwrap();
    thread::spawn(move || daemon.serve(listener));

    ask(&sock, Command::Update);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_losslessd-tui"));
    cmd.arg("--socket");
    cmd.arg(&sock);
    cmd.env("XDG_RUNTIME_DIR", dir.path(""));
    cmd.env("TERM", "xterm-256color");
    let mut child = Kill(pair.slave.spawn_command(cmd).unwrap());
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            if tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut keys = pair.master.take_writer().unwrap();
    let mut seen = Vec::new();

    keys.write_all(b"2").unwrap();
    wait_for(&rx, &mut seen, "album/");
    keys.write_all(b"l").unwrap();
    wait_for(&rx, &mut seen, "01 one");
    keys.write_all(b"\r").unwrap();

    let end = Instant::now() + WAIT;
    while status(&sock) != PlayState::Playing {
        assert!(Instant::now() < end, "never playing");
        while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
            seen.extend(chunk);
        }
    }

    keys.write_all(b"q").unwrap();
    let end = Instant::now() + WAIT;
    let code = loop {
        while rx.try_recv().is_ok() {}
        if let Some(st) = child.0.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < end, "tui did not exit");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(code.success(), "{code:?}");
}
