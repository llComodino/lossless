//! The `lossless` binary against an in-process daemon on a real socket, with
//! a real-time fake sink in place of a DAC.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use lossless_daemon::{Daemon, bind};
use lossless_engine::fixtures::{BLOCK, Dir, PacedSink, flac_at};
use lossless_engine::{Mixer, Player, SoftwareMixer};
use lossless_proto::{Body, Outcome, PlayState, ServerMessage, decode};

const WAIT: Duration = Duration::from_secs(10);

struct Server {
    dir: Dir,
}

impl Server {
    fn start() -> Self {
        Self::start_with(None)
    }

    fn start_with(mixer: Option<Box<dyn Mixer>>) -> Self {
        Self::start_on("sock", mixer)
    }

    /// Listening on `dir/<socket>`.
    fn start_on(socket: &str, mixer: Option<Box<dyn Mixer>>) -> Self {
        let dir = Dir::new();
        std::fs::create_dir_all(dir.path("music/album")).unwrap();
        let listener = bind(&dir.path(socket)).unwrap();
        let (player, events) = Player::spawn_with_mixer(PacedSink::default(), mixer);
        let daemon = Daemon::start(
            player,
            events,
            Some(dir.path("music")),
            dir.path("state.json"),
            Box::new(|| {}),
        )
        .unwrap();
        thread::spawn(move || daemon.serve(listener));
        Self { dir }
    }

    /// Two 16/44.1 tracks in `album/`; 50 blocks play for about 4.6 s.
    fn album(&self) -> Vec<PathBuf> {
        ["01 one.flac", "02 two.flac"]
            .iter()
            .map(|n| {
                let p = self.dir.path("music/album").join(n);
                flac_at(&p, 44_100, 16, 2, 50 * BLOCK).path
            })
            .collect()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lossless"));
        c.arg("--socket").arg(self.dir.path("sock")).args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Stdout of a successful run.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        String::from_utf8(out.stdout).unwrap()
    }

    /// Stderr of a failed run.
    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?} did not fail");
        stderr(&out)
    }

    fn spawn(&self, args: &[&str]) -> Lines {
        Lines::new(self.command(args).stdout(Stdio::piped()).spawn().unwrap())
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A long-running client whose stdout is read line by line.
struct Lines {
    child: Child,
    rx: Receiver<String>,
}

impl Lines {
    fn new(mut child: Child) -> Self {
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, rx }
    }

    fn next(&self, within: Duration) -> Option<String> {
        self.rx.recv_timeout(within).ok()
    }

    /// Skip lines until one contains `want`.
    fn wait_for(&self, want: &str) -> String {
        loop {
            let line = self.next(WAIT).unwrap_or_else(|| panic!("no {want:?}"));
            if line.contains(want) {
                return line;
            }
        }
    }
}

impl Drop for Lines {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn help_and_bad_invocations() {
    let s = Server::start();
    assert!(s.ok(&["--help"]).starts_with("usage: lossless"));
    assert!(s.err(&["--bogus"]).contains("unknown flag --bogus"));
    assert!(s.err(&[]).contains("usage: lossless"));
    assert!(s.err(&["play", "0"]).contains("indexes start at 1"));
}

/// `lossless ARGS` with `XDG_RUNTIME_DIR` set to the server's dir.
fn in_runtime_dir(s: &Server, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lossless"))
        .env("XDG_RUNTIME_DIR", &s.dir.0)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn profile_flag_reaches_that_daemon() {
    let s = Server::start_on("lossless-pw.sock", None);
    for flag in ["-p", "--profile"] {
        let out = in_runtime_dir(&s, &[flag, "pw", "status"]);
        assert!(out.status.success(), "{flag}: {}", stderr(&out));
        assert!(String::from_utf8_lossy(&out.stdout).contains("stopped"));
    }
    let out = in_runtime_dir(&s, &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("lossless.sock"), "{}", stderr(&out));
}

#[test]
fn socket_overrides_profile() {
    let s = Server::start();
    let sock = s.dir.path("sock");
    let sock = sock.to_str().unwrap();
    for args in [
        &["-p", "pw", "--socket", sock, "status"][..],
        &["--socket", sock, "-p", "pw", "status"],
    ] {
        let out = in_runtime_dir(&s, args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn bad_profile_name_fails() {
    let s = Server::start();
    let out = in_runtime_dir(&s, &["-p", "Bad", "status"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out).trim(),
        "Error: \"Bad\": profile names are 1-32 of a-z 0-9 _ -"
    );
    let out = in_runtime_dir(&s, &["-p"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("usage: lossless"), "{}", stderr(&out));
}

#[test]
fn no_daemon_says_so() {
    let dir = Dir::new();
    let out = Command::new(env!("CARGO_BIN_EXE_lossless"))
        .arg("--socket")
        .arg(dir.path("sock"))
        .arg("status")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("is losslessd running?"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn fresh_status_and_empty_queue() {
    let s = Server::start();
    assert_eq!(
        s.ok(&["status"]),
        "stopped  #-  0:00/?  output closed repeat:off random:off\n"
    );
    assert_eq!(s.ok(&["queue"]), "");
}

#[test]
fn playback_session() {
    let s = Server::start();
    s.album();
    assert_eq!(s.ok(&["add", "album"]), "");
    let queue = s.ok(&["queue"]);
    let lines: Vec<&str> = queue.lines().collect();
    assert_eq!(lines.len(), 2, "{queue}");
    assert!(lines[0].starts_with("   1  01 one.flac  "), "{queue}");
    assert!(lines[0].ends_with("  0:04  16-bit/44.1 kHz/2ch"), "{queue}");
    assert!(lines[1].starts_with("   2  02 two.flac  "), "{queue}");

    s.ok(&["play"]);
    s.ok(&["pause"]);
    s.ok(&["seek", "0:02"]);
    let status = s.ok(&["status"]);
    assert!(
        status.starts_with(
            "paused  #1  0:02/0:04  output 16-bit/44.1 kHz/2ch repeat:off random:off\n"
        ),
        "{status}"
    );
    assert_eq!(status.lines().nth(1), Some("01 one.flac"));

    s.ok(&["next"]);
    assert!(s.ok(&["status"]).starts_with("paused  #2  0:00/0:04"));
    s.ok(&["prev"]);
    assert!(s.ok(&["status"]).starts_with("paused  #1  "));
    s.ok(&["play", "2"]);
    assert!(s.ok(&["status"]).starts_with("playing  #2  "));
    s.ok(&["toggle"]);
    assert!(s.ok(&["status"]).starts_with("paused  #2  "));
    s.ok(&["stop"]);
    assert!(s.ok(&["status"]).starts_with("stopped  "));
    assert!(s.ok(&["status"]).contains("output closed"));

    s.ok(&["clear"]);
    assert_eq!(s.ok(&["queue"]), "");
    assert!(s.err(&["add", "nope"]).contains("nope"));
}

#[test]
fn json_prints_the_raw_reply() {
    let s = Server::start();
    let out = s.ok(&["--json", "status"]);
    assert_eq!(out.lines().count(), 1, "{out}");
    match decode::<ServerMessage>(out.trim_end()).unwrap() {
        ServerMessage::Reply {
            id: 1,
            result: Outcome::Ok(Body::Status(st)),
        } => assert_eq!(st.state, PlayState::Stopped),
        other => panic!("not a status reply: {other:?}"),
    }
}

#[test]
fn update_then_search_prints_tracks_with_paths() {
    let s = Server::start();
    let files = s.album();
    // The reply line proves the subscription is in place before `update`.
    let watch = s.spawn(&["--json", "watch"]);
    assert!(watch.wait_for("reply").contains("done"));
    s.ok(&["update"]);
    assert!(watch.wait_for("library_changed").contains("\"tracks\":2"));

    let out = s.ok(&["search", "two"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[0].starts_with("   1  02 two.flac  "), "{out}");
    assert_eq!(lines[1], format!("      {}", files[1].display()));
    assert_eq!(s.ok(&["search", "nothing"]), "");
}

#[test]
fn listing_commands() {
    let s = Server::start();
    s.album();
    let watch = s.spawn(&["--json", "watch"]);
    assert!(watch.wait_for("reply").contains("done"));
    s.ok(&["update"]);
    watch.wait_for("library_changed");

    assert_eq!(s.ok(&["browse"]), "album/\n");
    let out = s.ok(&["browse", "album"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].starts_with("   1  01 one.flac  "), "{out}");
    assert_eq!(s.ok(&["artists"]), "Unknown\n");
    assert_eq!(s.ok(&["albums"]), "Unknown\n");
    assert_eq!(s.ok(&["albums", "Unknown"]), "Unknown\n");
    assert_eq!(s.ok(&["find"]).lines().count(), 4);
    assert_eq!(s.ok(&["find", "--album", "Unknown"]).lines().count(), 4);
    assert_eq!(s.ok(&["find", "--artist", "Nobody"]), "");
    assert!(s.err(&["browse", "nope"]).contains("no such"));
    let json = s.ok(&["--json", "artists"]);
    assert!(json.contains("\"names\""), "{json}");
}

#[test]
fn watch_prints_events() {
    let s = Server::start();
    s.album();
    let watch = s.spawn(&["watch"]);
    // Plain `watch` prints nothing on subscribing: add until it reports.
    let mut seen = false;
    for _ in 0..50 {
        s.ok(&["add", "album/01 one.flac"]);
        if watch.next(Duration::from_millis(200)).as_deref() == Some("queue changed") {
            seen = true;
            break;
        }
    }
    assert!(seen, "watch never reported a queue change");
    s.ok(&["play"]);
    watch.wait_for("track #1");
    watch.wait_for("state Playing at 0:00");
    s.ok(&["stop"]);
    watch.wait_for("state Stopped");
    s.ok(&["update"]);
    watch.wait_for("library changed: 2 tracks (+2 -0)");
}

#[test]
fn closed_stdout_exits_quietly() {
    let s = Server::start();
    s.album();
    // Well over a pipe buffer of queue lines.
    let many = vec!["album/01 one.flac"; 1500];
    s.ok(&[&["add"][..], &many].concat());
    let mut child = s
        .command(&["queue"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    assert!(child.wait().unwrap().success());
}

#[test]
fn queue_editing_commands() {
    let s = Server::start();
    s.album();
    let names = |s: &Server| -> Vec<String> {
        s.ok(&["queue"])
            .lines()
            .map(|l| l.trim().split("  ").nth(1).unwrap().to_string())
            .collect()
    };
    s.ok(&["add", "album/02 two.flac"]);
    s.ok(&["add", "--at", "1", "album/01 one.flac"]);
    assert_eq!(names(&s), ["01 one.flac", "02 two.flac"]);
    s.ok(&["move", "1", "2"]);
    assert_eq!(names(&s), ["02 two.flac", "01 one.flac"]);
    s.ok(&["remove", "2"]);
    assert_eq!(names(&s), ["02 two.flac"]);
    s.ok(&["replace", "album"]);
    assert_eq!(names(&s), ["01 one.flac", "02 two.flac"]);
    for args in [
        &["add", "--at", "0", "album"][..],
        &["remove", "0"],
        &["remove", "1", "0"],
        &["move", "0", "1"],
        &["move", "1", "0"],
    ] {
        assert_eq!(
            s.err(args).trim(),
            "Error: positions start at 1",
            "{args:?}"
        );
    }
}

#[test]
fn mode_commands() {
    let s = Server::start();
    let watch = s.spawn(&["--json", "watch"]);
    assert!(watch.wait_for("reply").contains("done"));
    assert_eq!(s.ok(&["repeat", "one"]), "");
    assert!(watch.wait_for("\"mode\"").contains("\"repeat\":\"one\""));
    s.ok(&["random", "on"]);
    assert!(
        s.ok(&["status"])
            .ends_with("output closed repeat:one random:on\n")
    );
    s.ok(&["repeat", "queue"]);
    s.ok(&["random", "off"]);
    assert!(s.ok(&["status"]).ends_with(" repeat:queue random:off\n"));
    s.ok(&["repeat", "off"]);
    assert!(s.ok(&["status"]).ends_with(" repeat:off random:off\n"));
    let plain = s.spawn(&["watch"]);
    let mut seen = None;
    for _ in 0..50 {
        s.ok(&["random", "on"]);
        if let Some(line) = plain.next(Duration::from_millis(200)) {
            seen = Some(line);
            break;
        }
    }
    assert_eq!(seen.as_deref(), Some("mode repeat:off random:on"));
    for (args, want) in [
        (&["repeat", "all"][..], "repeat takes off, queue or one"),
        (&["repeat"], "repeat takes off, queue or one"),
        (&["random", "yes"], "random takes on or off"),
        (&["random", "on", "off"], "random takes on or off"),
    ] {
        assert_eq!(s.err(args).trim(), format!("Error: {want}"), "{args:?}");
    }
}

#[test]
fn volume_command() {
    let s = Server::start_with(Some(Box::new(SoftwareMixer::new())));
    assert_eq!(s.ok(&["volume"]), "100%\n");
    assert!(
        s.ok(&["status"])
            .ends_with(" repeat:off random:off vol 100%\n")
    );
    let watch = s.spawn(&["watch"]);
    let mut seen = None;
    for _ in 0..50 {
        assert_eq!(s.ok(&["volume", "40"]), "");
        if let Some(line) = watch.next(Duration::from_millis(200)) {
            seen = Some(line);
            break;
        }
    }
    assert_eq!(seen.as_deref(), Some("volume 40%"));
    for (args, want) in [
        (&["volume", "+5"][..], "45%\n"),
        (&["volume", "-50"], "0%\n"),
        (&["volume", "+100"], "100%\n"),
        (&["volume", "-0"], "100%\n"),
        (&["volume", "0"], "0%\n"),
    ] {
        assert_eq!(s.ok(args), "", "{args:?}");
        assert_eq!(s.ok(&["volume"]), want, "{args:?}");
    }
    let json = s.ok(&["--json", "volume", "+7"]);
    let lines: Vec<&str> = json.lines().collect();
    assert_eq!(lines.len(), 2, "{json}");
    assert!(lines[0].contains("\"volume\":0"), "{json}");
    assert!(lines[1].contains("\"id\":2"), "{json}");
    for args in [
        &["volume", "101"][..],
        &["volume", "x"],
        &["volume", "+"],
        &["volume", "++5"],
        &["volume", "+101"],
        &["volume", "1", "2"],
    ] {
        assert_eq!(
            s.err(args).trim(),
            "Error: volume takes N, +N or -N, from 0 to 100",
            "{args:?}"
        );
    }

    let s = Server::start();
    assert_eq!(s.ok(&["volume"]), "no mixer\n");
    assert!(s.ok(&["status"]).ends_with(" random:off\n"));
    assert_eq!(s.err(&["volume", "+5"]).trim(), "Error: no mixer");
    assert_eq!(
        s.err(&["volume", "5"]).trim(),
        "Error: no volume control (mixer = none or unavailable)"
    );
}
