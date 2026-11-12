//! Daemon startup: `run` in-process on the ALSA null device, and the
//! `losslessd` binary's environment handling (runs that exit, so coverage
//! profiles are written).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lossless_daemon::run;
use lossless_engine::fixtures::{BLOCK, Dir, flac_at};

fn write_config(dir: &Dir, text: &str) -> PathBuf {
    let p = dir.path("config.toml");
    std::fs::write(&p, text).unwrap();
    p
}

/// Connect once `run` has bound the socket (it binds before serving).
fn connect(socket: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(socket) {
            Ok(s) => return s,
            Err(e) if Instant::now() > deadline => panic!("daemon never listened: {e}"),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn call(stream: &UnixStream, line: &str) -> String {
    (&*stream)
        .write_all(format!("{line}\n").as_bytes())
        .unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    reply
}

#[test]
fn run_serves_the_configured_socket_and_music() {
    let dir = Dir::new();
    std::fs::create_dir(dir.path("music")).unwrap();
    flac_at(&dir.path("music/a.flac"), 44_100, 16, 2, BLOCK);
    let sock = dir.path("cfg.sock");
    let cfg = write_config(
        &dir,
        &format!(
            "device = \"null\"\nmusic_dir = \"{}\"\nsocket = \"{}\"\n",
            dir.path("music").display(),
            sock.display()
        ),
    );
    let unused = dir.path("default.sock");
    thread::spawn(move || run(&cfg, unused, None));
    let c = connect(&sock);
    assert!(call(&c, r#"{"id":1,"cmd":"status"}"#).contains(r#""state":"stopped""#));
    assert!(call(&c, r#"{"id":2,"cmd":"add","paths":["a.flac"]}"#).contains("done"));
    assert!(!dir.path("default.sock").exists());
}

#[test]
fn run_falls_back_to_the_default_socket() {
    let dir = Dir::new();
    let cfg = write_config(&dir, "device = \"null\"\nmixer = \"software\"\n");
    let sock = dir.path("default.sock");
    let s = sock.clone();
    thread::spawn(move || run(&cfg, s, None));
    let c = connect(&sock);
    assert!(call(&c, r#"{"id":1,"cmd":"update"}"#).contains("no music_dir configured"));
}

#[test]
fn run_stops_on_a_bad_config_or_socket() {
    let dir = Dir::new();
    let bad = write_config(&dir, "device = \"file:'|id'\"\n");
    let err = run(&bad, dir.path("s.sock"), None).unwrap_err();
    assert!(format!("{err:#}").contains("not allowed"), "{err:#}");
    assert!(!dir.path("s.sock").exists(), "bound before validating");

    let key = dir.path("key");
    std::fs::write(&key, "secret").unwrap();
    let ok = write_config(&dir, "device = \"null\"\n");
    let err = run(&ok, key.clone(), None).unwrap_err();
    assert!(format!("{err:#}").contains("not a socket"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&key).unwrap(), "secret");
}

fn losslessd(env: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_losslessd"));
    cmd.env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_RUNTIME_DIR");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn failed_with(out: &Output, want: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains(want), "{err}");
}

#[test]
fn binary_reads_the_xdg_config() {
    let dir = Dir::new();
    std::fs::create_dir_all(dir.path("xdg/lossless")).unwrap();
    std::fs::write(
        dir.path("xdg/lossless/config.toml"),
        "device = \"plug:file\"",
    )
    .unwrap();
    failed_with(
        &losslessd(&[("XDG_CONFIG_HOME", &dir.path("xdg"))]),
        "not allowed",
    );
}

#[test]
fn binary_falls_back_to_home_config() {
    let dir = Dir::new();
    std::fs::create_dir_all(dir.path(".config/lossless")).unwrap();
    std::fs::write(
        dir.path(".config/lossless/config.toml"),
        "music_dir = \"m\"",
    )
    .unwrap();
    failed_with(&losslessd(&[("HOME", &dir.0)]), "must be absolute");
}

#[test]
fn binary_without_home_uses_defaults_and_the_runtime_socket() {
    // No HOME or XDG_CONFIG_HOME: /.config/lossless/config.toml, normally
    // absent, so all defaults; a non-socket where the socket goes stops it.
    let dir = Dir::new();
    std::fs::write(dir.path("lossless.sock"), "not a socket").unwrap();
    failed_with(&losslessd(&[("XDG_RUNTIME_DIR", &dir.0)]), "not a socket");
}

fn spawn_with(dir: &Dir, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_losslessd"))
        .env_remove("HOME")
        .env("XDG_CONFIG_HOME", dir.path("xdg"))
        .env_remove("XDG_RUNTIME_DIR")
        .args(args)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn xdg_config(dir: &Dir, text: &str) {
    std::fs::create_dir_all(dir.path("xdg/lossless")).unwrap();
    std::fs::write(dir.path("xdg/lossless/config.toml"), text).unwrap();
}

#[test]
fn binary_logs_nothing_by_default() {
    let dir = Dir::new();
    let sock = dir.path("held.sock");
    let _held = UnixListener::bind(&sock).unwrap();
    xdg_config(
        &dir,
        &format!("device = \"null\"\nsocket = \"{}\"\n", sock.display()),
    );
    let out = spawn_with(&dir, &[]).wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(
        err.starts_with("losslessd: another daemon is listening"),
        "{err}"
    );
}

#[test]
fn log_level_flag_overrides_config() {
    let dir = Dir::new();
    std::fs::create_dir(dir.path("music")).unwrap();
    xdg_config(
        &dir,
        &format!(
            "log_level = \"off\"\ndevice = \"null\"\nsocket = \"{}\"\nmusic_dir = \"{}\"\n",
            dir.path("s.sock").display(),
            dir.path("music").display()
        ),
    );
    let mut child = spawn_with(&dir, &["--log-level", "info"]);
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("INFO") && line.contains("library: 0 tracks") {
                let _ = tx.send(line);
                return;
            }
        }
    });
    let got = rx.recv_timeout(Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    got.expect("no info line on stderr");
}

#[test]
fn bad_flag_prints_usage() {
    let dir = Dir::new();
    for args in [
        &["--verbose"][..],
        &["--log-level"],
        &["--log-level", "loud"],
        &["--log-level", "info", "extra"],
    ] {
        let out = spawn_with(&dir, args).wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        assert!(
            err.contains("usage: losslessd [--log-level off|warn|info|debug]"),
            "{err}"
        );
    }
}
