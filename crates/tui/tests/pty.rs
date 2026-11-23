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

struct Session {
    _dir: Dir,
    sock: std::path::PathBuf,
    child: Kill,
    rx: Receiver<Vec<u8>>,
    keys: Box<dyn Write + Send>,
    seen: Vec<u8>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

fn session(names: &[&str]) -> Session {
    session_with(names, None)
}

/// `tui_ron` becomes `$XDG_CONFIG_HOME/lossless/tui.ron`; the config home is
/// always the test's own, never the user's.
fn session_with(names: &[&str], tui_ron: Option<&str>) -> Session {
    let dir = Dir::new();
    if let Some(text) = tui_ron {
        std::fs::create_dir_all(dir.path("config/lossless")).unwrap();
        std::fs::write(dir.path("config/lossless/tui.ron"), text).unwrap();
    }
    let album = dir.path("music/album");
    std::fs::create_dir_all(&album).unwrap();
    for n in names {
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
    cmd.env("XDG_CONFIG_HOME", dir.path("config"));
    cmd.env("TERM", "xterm-256color");
    let child = Kill(pair.slave.spawn_command(cmd).unwrap());
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
    let keys = pair.master.take_writer().unwrap();
    let seen = Vec::new();

    Session {
        _dir: dir,
        sock,
        child,
        rx,
        keys,
        seen,
        _master: pair.master,
    }
}

#[test]
fn keys_drive_the_daemon() {
    let Session {
        sock,
        mut child,
        rx,
        mut keys,
        mut seen,
        _dir,
        _master,
    } = session(&["01 one.flac", "02 two.flac"]);
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

#[test]
fn typing_reranks_rows_live() {
    let mut s = session(&["01 one.flac", "02 two.flac", "03 three.flac"]);
    s.keys.write_all(b"4").unwrap();
    wait_for(&s.rx, &mut s.seen, "three.flac");
    s.keys.write_all(b"/thr").unwrap();
    let mut after = Vec::new();
    let end = Instant::now() + WAIT;
    let first_row = loop {
        let text = String::from_utf8_lossy(&after).into_owned();
        if text.contains("\x1b[5;13Hhree.flac") {
            break true;
        }
        let left = end.saturating_duration_since(Instant::now());
        match s.rx.recv_timeout(left) {
            Ok(chunk) => after.extend(chunk),
            Err(_) => break false,
        }
    };
    assert!(
        first_row,
        "three.flac never took row 1: {}",
        String::from_utf8_lossy(&after)
    );
    s.seen.extend(&after);
    assert!(
        String::from_utf8_lossy(&s.seen).contains("48;2;24;24;24"),
        "no #181818 background"
    );
    s.keys.write_all(b"\x1b").unwrap();
    wait_for(&s.rx, &mut Vec::new(), "01 one.flac");
    s.keys.write_all(b"q").unwrap();
    let end = Instant::now() + WAIT;
    while s.child.0.try_wait().unwrap().is_none() {
        while s.rx.try_recv().is_ok() {}
        assert!(Instant::now() < end, "tui did not exit");
        thread::sleep(Duration::from_millis(20));
    }
}

fn exits_within(
    child: &mut Kill,
    rx: &Receiver<Vec<u8>>,
    d: Duration,
) -> Option<portable_pty::ExitStatus> {
    let end = Instant::now() + d;
    loop {
        while rx.try_recv().is_ok() {}
        if let Some(st) = child.0.try_wait().unwrap() {
            return Some(st);
        }
        if Instant::now() >= end {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn tui_ron_remaps_quit() {
    let mut s = session_with(&["01 one.flac"], Some(r#"(keys: {"quit": ["Q"]})"#));
    wait_for(&s.rx, &mut s.seen, "Queue");
    s.keys.write_all(b"q").unwrap();
    assert!(
        exits_within(&mut s.child, &s.rx, Duration::from_millis(500)).is_none(),
        "q still quits"
    );
    s.keys.write_all(b"Q").unwrap();
    let code = exits_within(&mut s.child, &s.rx, WAIT).expect("Q did not quit");
    assert!(code.success(), "{code:?}");
}

#[cfg(feature = "scripts")]
mod scripts {
    use super::*;

    const RON: &str = r#"(scripts: [
        (name: "ask", command: "echo name?; read n; echo \"hello $n\" > \"$XDG_RUNTIME_DIR/hello\"; echo bye", key: "m", mode: Modal),
        (name: "quiet", command: "echo y > \"$XDG_RUNTIME_DIR/ran\"", key: "s", mode: Modal),
        (name: "term", command: "sleep 1; read x; echo \"got:$x\" > \"$XDG_RUNTIME_DIR/out\"", key: "t", mode: Terminal),
        (name: "fail", command: "echo oops; exit 3", key: "f", mode: Terminal),
        (name: "nap", command: "sleep 5", key: "c", mode: Terminal),
    ])"#;

    fn started() -> Session {
        let mut s = session_with(&["01 one.flac"], Some(RON));
        wait_for(&s.rx, &mut s.seen, "Queue");
        s
    }

    fn quits(s: &mut Session) {
        s.keys.write_all(b"q").unwrap();
        let end = Instant::now() + WAIT;
        let code = loop {
            while let Ok(c) = s.rx.try_recv() {
                s.seen.extend(c);
            }
            if let Some(st) = s.child.0.try_wait().unwrap() {
                break st;
            }
            assert!(Instant::now() < end, "q did not quit");
            thread::sleep(Duration::from_millis(20));
        };
        while let Ok(c) = s.rx.recv_timeout(Duration::from_millis(100)) {
            s.seen.extend(c);
        }
        let text = String::from_utf8_lossy(&s.seen);
        let tail: String = text
            .chars()
            .rev()
            .take(300)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(code.success(), "{code:?}: ...{tail}");
    }

    fn wait_file(path: &std::path::Path) -> String {
        let end = Instant::now() + WAIT;
        loop {
            if let Ok(t) = std::fs::read_to_string(path)
                && !t.is_empty()
            {
                return t;
            }
            assert!(Instant::now() < end, "{} never written", path.display());
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn modal_script_round_trip() {
        let mut s = started();
        s.keys.write_all(b"xm").unwrap();
        wait_for(&s.rx, &mut s.seen, "name?");
        s.keys.write_all(b"bo\r").unwrap();
        assert_eq!(wait_file(&s._dir.path("hello")), "hello bo\n");
        thread::sleep(Duration::from_millis(300));
        // The box stays (it printed); the first key closes it, so q must not
        // quit yet.
        s.keys.write_all(b"q").unwrap();
        assert!(
            exits_within(&mut s.child, &s.rx, Duration::from_millis(500)).is_none(),
            "q quit through the open box"
        );
        quits(&mut s);
    }

    #[test]
    fn silent_script_auto_closes() {
        let mut s = started();
        s.keys.write_all(b"xs").unwrap();
        assert_eq!(wait_file(&s._dir.path("ran")), "y\n");
        thread::sleep(Duration::from_millis(300));
        quits(&mut s);
    }

    #[test]
    fn terminal_script_owns_stdin() {
        let mut s = started();
        s.keys.write_all(b"xt").unwrap();
        thread::sleep(Duration::from_millis(300));
        s.keys.write_all(b"z\r").unwrap();
        assert_eq!(wait_file(&s._dir.path("out")), "got:z\n");
        thread::sleep(Duration::from_millis(300));
        quits(&mut s);
    }

    #[test]
    fn terminal_failure_waits_for_a_key() {
        let mut s = started();
        s.keys.write_all(b"xf").unwrap();
        wait_for(&s.rx, &mut s.seen, "[exited 3, press any key]");
        s.keys.write_all(b"q").unwrap();
        assert!(
            exits_within(&mut s.child, &s.rx, Duration::from_millis(500)).is_none(),
            "the key that ended the pause also quit"
        );
        quits(&mut s);
    }

    #[test]
    fn ctrl_c_in_terminal_script_spares_the_tui() {
        let mut s = started();
        s.keys.write_all(b"xc").unwrap();
        thread::sleep(Duration::from_millis(500));
        s.keys.write_all(b"\x03").unwrap();
        wait_for(&s.rx, &mut s.seen, "[killed by signal 2, press any key]");
        s.keys.write_all(b" ").unwrap();
        quits(&mut s);
    }
}
