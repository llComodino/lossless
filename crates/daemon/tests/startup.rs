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
use lossless_proto::profile::Profile;

/// In-process daemons never register on the real session bus.
fn write_config(dir: &Dir, text: &str) -> PathBuf {
    let p = dir.path("config.toml");
    std::fs::write(&p, format!("{text}\nmpris = false\n")).unwrap();
    p
}

/// Nothing listens here: a daemon built with `mpris` must not reach the
/// real session bus (zbus would fall back to /run/user/UID/bus).
const NO_BUS: &str = "unix:path=/nonexistent/lossless-test-bus";

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
    let state = dir.path("state.json");
    thread::spawn(move || run(&cfg, unused, state, None, &Profile::default()));
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
    let state = dir.path("state.json");
    thread::spawn(move || run(&cfg, s, state, None, &Profile::default()));
    let c = connect(&sock);
    assert!(call(&c, r#"{"id":1,"cmd":"update"}"#).contains("no music_dir configured"));
}

#[test]
fn a_missing_mixer_card_runs_without_volume() {
    let dir = Dir::new();
    let sock = dir.path("s.sock");
    let cfg = write_config(
        &dir,
        &format!(
            "device = \"hw:CARD=NoSuchCard9,DEV=0\"\nmixer = \"hardware\"\nsocket = \"{}\"\n",
            sock.display()
        ),
    );
    let state = dir.path("state.json");
    thread::spawn(move || {
        run(
            &cfg,
            PathBuf::from("/nonexistent"),
            state,
            None,
            &Profile::default(),
        )
    });
    let c = connect(&sock);
    let reply = call(&c, r#"{"id":1,"cmd":"set_volume","percent":50}"#);
    assert!(reply.contains(r#""code":"no_mixer""#), "{reply}");
    assert!(call(&c, r#"{"id":2,"cmd":"status"}"#).contains(r#""volume":null"#));
}

#[test]
fn a_missing_mixer_card_is_logged() {
    let dir = Dir::new();
    xdg_config(
        &dir,
        &format!(
            "device = \"hw:CARD=NoSuchCard9,DEV=0\"\nsocket = \"{}\"\n",
            dir.path("s.sock").display()
        ),
    );
    let mut child = spawn_with(&dir, &["--log-level", "warn"]);
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.starts_with("WARN") && line.contains("hw:NoSuchCard9") {
                let _ = tx.send(line);
                return;
            }
        }
    });
    let got = rx.recv_timeout(Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    let line = got.expect("no mixer warning on stderr");
    assert!(
        line.ends_with("; no volume control until it opens"),
        "{line}"
    );
}

#[test]
fn run_stops_on_a_bad_config_or_socket() {
    let dir = Dir::new();
    let bad = write_config(&dir, "device = \"file:'|id'\"\n");
    let err = run(
        &bad,
        dir.path("s.sock"),
        dir.path("state.json"),
        None,
        &Profile::default(),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("not allowed"), "{err:#}");
    assert!(!dir.path("s.sock").exists(), "bound before validating");

    let key = dir.path("key");
    std::fs::write(&key, "secret").unwrap();
    let ok = write_config(&dir, "device = \"null\"\n");
    let err = run(
        &ok,
        key.clone(),
        dir.path("state.json"),
        None,
        &Profile::default(),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("not a socket"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&key).unwrap(), "secret");

    let err = run(
        &ok,
        dir.path("s.sock"),
        key.join("state.json"),
        None,
        &Profile::default(),
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.starts_with("preparing state directory"), "{msg}");
    assert!(!dir.path("s.sock").exists(), "bound before the state check");
}

#[test]
fn run_stops_on_a_read_only_state_dir() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new();
    let ro = dir.path("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::write(ro.join("probe"), "").is_ok() {
        return; // root ignores the mode
    }
    let cfg = write_config(&dir, "device = \"null\"\n");
    let err = run(
        &cfg,
        dir.path("s.sock"),
        ro.join("state.json"),
        None,
        &Profile::default(),
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.starts_with("preparing state directory"), "{msg}");
    assert!(msg.contains("ermission denied"), "{msg}");
    assert!(!dir.path("s.sock").exists(), "bound before the state check");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn losslessd(env: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_losslessd"));
    cmd.env_remove("HOME")
        .env("DBUS_SESSION_BUS_ADDRESS", NO_BUS)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("XDG_STATE_HOME");
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
    let env = [("XDG_RUNTIME_DIR", &*dir.0), ("XDG_STATE_HOME", &*dir.0)];
    failed_with(&losslessd(&env), "not a socket");
}

fn spawn_with(dir: &Dir, args: &[&str]) -> std::process::Child {
    spawn_env(dir, args, &[("XDG_STATE_HOME", &dir.path("state"))])
}

fn spawn_env(dir: &Dir, args: &[&str], env: &[(&str, &Path)]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_losslessd"))
        .env_remove("HOME")
        .env("DBUS_SESSION_BUS_ADDRESS", NO_BUS)
        .env("XDG_CONFIG_HOME", dir.path("xdg"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("XDG_STATE_HOME")
        .envs(env.iter().copied())
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
            err.contains("usage: losslessd [--profile NAME] [--log-level off|warn|info|debug]"),
            "{err}"
        );
    }
}

/// The first stderr line containing `want`, within the usual wait. The rest
/// is drained, so the daemon never logs into a closed pipe.
fn stderr_line(child: &mut std::process::Child, want: &'static str) -> Option<String> {
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains(want) {
                let _ = tx.send(line);
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

#[test]
fn restore_survives_gone_and_corrupt_state() {
    for (text, want) in [
        (
            r#"{"queue":["/gone/a.flac","/gone/b.flac"],"index":1,"position_ms":5,"repeat":"off","random":false}"#,
            "state: dropped 2 missing entries",
        ),
        (r#"{"queue":["/gone/a.flac"],"ind"#, "state: parsing"),
    ] {
        let dir = Dir::new();
        let sock = dir.path("s.sock");
        xdg_config(
            &dir,
            &format!("device = \"null\"\nsocket = \"{}\"\n", sock.display()),
        );
        std::fs::create_dir_all(dir.path("state/lossless")).unwrap();
        std::fs::write(dir.path("state/lossless/state.json"), text).unwrap();
        let mut child = spawn_with(&dir, &["--log-level", "warn"]);
        let line = stderr_line(&mut child, "state:");
        let c = connect(&sock);
        let q = call(&c, r#"{"id":1,"cmd":"queue"}"#);
        let _ = child.kill();
        let _ = child.wait();
        let line = line.expect("no state warning on stderr");
        assert!(line.starts_with("WARN") && line.contains(want), "{line}");
        assert!(q.contains(r#""data":[]"#), "{q}");
    }
}

#[test]
fn binary_state_falls_back_to_home_without_an_absolute_xdg_dir() {
    let dir = Dir::new();
    xdg_config(
        &dir,
        &format!(
            "device = \"null\"\nsocket = \"{}\"\n",
            dir.path("s.sock").display()
        ),
    );
    std::fs::create_dir_all(dir.path(".local/state/lossless")).unwrap();
    std::fs::write(dir.path(".local/state/lossless/state.json"), "{").unwrap();
    let mut child = spawn_env(
        &dir,
        &["--log-level", "warn"],
        &[("HOME", &dir.0), ("XDG_STATE_HOME", Path::new("relative"))],
    );
    let line = stderr_line(&mut child, "state:");
    let _ = child.kill();
    let _ = child.wait();
    let line = line.expect("no state warning on stderr");
    assert!(line.contains(".local/state/lossless/state.json"), "{line}");
}

#[test]
fn run_restores_modes_when_every_entry_is_gone() {
    let dir = Dir::new();
    let sock = dir.path("s.sock");
    let cfg = write_config(
        &dir,
        &format!("device = \"null\"\nsocket = \"{}\"\n", sock.display()),
    );
    let state = dir.path("state.json");
    std::fs::write(
        &state,
        r#"{"queue":["/gone/a.flac"],"index":0,"position_ms":5,"repeat":"one","random":true}"#,
    )
    .unwrap();
    let unused = dir.path("default.sock");
    thread::spawn(move || run(&cfg, unused, state, None, &Profile::default()));
    let c = connect(&sock);
    let st = call(&c, r#"{"id":1,"cmd":"status"}"#);
    assert!(st.contains(r#""repeat":"one""#), "{st}");
    assert!(st.contains(r#""random":true"#), "{st}");
    assert!(call(&c, r#"{"id":2,"cmd":"queue"}"#).contains(r#""data":[]"#));
}

const USAGE: &str = "usage: losslessd [--profile NAME] [--log-level off|warn|info|debug]";

/// A daemon child killed when dropped, so a failed assert never leaks it.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// XDG dirs under `dir`: config `xdg`, state `state`, runtime `run`.
fn profile_env(dir: &Dir) -> [(&'static str, PathBuf); 2] {
    std::fs::create_dir_all(dir.path("run")).unwrap();
    [
        ("XDG_STATE_HOME", dir.path("state")),
        ("XDG_RUNTIME_DIR", dir.path("run")),
    ]
}

fn profile_config(dir: &Dir, name: &str) {
    std::fs::create_dir_all(dir.path("xdg/lossless")).unwrap();
    std::fs::write(
        dir.path(&format!("xdg/lossless/{name}.toml")),
        "device = \"null\"\n",
    )
    .unwrap();
}

fn spawn_profile(dir: &Dir, args: &[&str]) -> std::process::Child {
    let env = profile_env(dir);
    let env: Vec<(&str, &Path)> = env.iter().map(|(k, v)| (*k, v.as_path())).collect();
    spawn_env(dir, args, &env)
}

fn start(dir: &Dir, args: &[&str]) -> Daemon {
    Daemon(spawn_profile(dir, args))
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn named_profile_uses_its_own_files() {
    let dir = Dir::new();
    profile_config(&dir, "pw");
    let track = dir.path("a.flac");
    flac_at(&track, 44_100, 16, 2, BLOCK);
    let _d = start(&dir, &["--profile", "pw"]);
    let c = connect(&dir.path("run/lossless-pw.sock"));
    assert!(call(&c, r#"{"id":1,"cmd":"status"}"#).contains(r#""state":"stopped""#));
    let add = format!(r#"{{"id":2,"cmd":"add","paths":["{}"]}}"#, track.display());
    assert!(call(&c, &add).contains("done"));
    call(&c, r#"{"id":3,"cmd":"pause"}"#);
    wait_for(&dir.path("state/lossless/pw/state.json"));
    assert!(!dir.path("state/lossless/state.json").exists());
    assert!(!dir.path("run/lossless.sock").exists());
}

#[test]
fn two_profiles_run_side_by_side() {
    let dir = Dir::new();
    profile_config(&dir, "config");
    profile_config(&dir, "pw");
    let track = dir.path("a.flac");
    flac_at(&track, 44_100, 16, 2, BLOCK);
    let _a = start(&dir, &[]);
    let _b = start(&dir, &["--profile", "pw"]);
    let a = connect(&dir.path("run/lossless.sock"));
    let b = connect(&dir.path("run/lossless-pw.sock"));
    let add = format!(r#"{{"id":1,"cmd":"add","paths":["{}"]}}"#, track.display());
    assert!(call(&b, &add).contains("done"));
    assert!(call(&a, r#"{"id":2,"cmd":"queue"}"#).contains(r#""data":[]"#));
    let q = call(&b, r#"{"id":3,"cmd":"queue"}"#);
    assert!(q.contains("a.flac"), "{q}");
}

#[test]
fn same_profile_twice_fails_at_bind() {
    let dir = Dir::new();
    profile_config(&dir, "pw");
    let _first = start(&dir, &["--profile", "pw"]);
    let sock = dir.path("run/lossless-pw.sock");
    let c = connect(&sock);
    let out = spawn_profile(&dir, &["--profile", "pw"])
        .wait_with_output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("another daemon is listening"), "{err}");
    assert!(call(&c, r#"{"id":1,"cmd":"status"}"#).contains("stopped"));
}

#[test]
fn bad_profile_name_is_refused() {
    let dir = Dir::new();
    let out = spawn_with(&dir, &["--profile", "Bad"])
        .wait_with_output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert_eq!(
        err.trim_end(),
        "losslessd: \"Bad\": profile names are 1-32 of a-z 0-9 _ -"
    );
}

#[test]
fn flags_in_any_order() {
    for args in [
        &["--profile", "pw", "--log-level", "warn"][..],
        &["--log-level", "warn", "--profile", "pw"],
    ] {
        let dir = Dir::new();
        profile_config(&dir, "pw");
        let _d = start(&dir, args);
        let c = connect(&dir.path("run/lossless-pw.sock"));
        assert!(
            call(&c, r#"{"id":1,"cmd":"status"}"#).contains("stopped"),
            "{args:?}"
        );
    }
    let dir = Dir::new();
    for args in [
        &["--profile", "pw", "--profile", "pw"][..],
        &["--profile"],
        &["--log-level", "warn", "--profile"],
        &["--log-level", "warn", "--log-level", "warn"],
    ] {
        let out = spawn_with(&dir, args).wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains(USAGE), "{args:?}: {err}");
    }
}

#[cfg(feature = "mpris")]
mod mpris {
    use super::*;

    /// Killed and reaped on drop, so a failed assert leaves nothing running
    /// (an orphan would also hold the test's output pipe open).
    struct Reap(std::process::Child);

    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A private bus for one test; `None` without `dbus-daemon`.
    fn private_bus(dir: &Dir) -> Option<(Reap, String)> {
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
        Some((Reap(child), line.trim().to_string()))
    }

    fn owned(address: &str, name: &str) -> bool {
        let c = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .build()
            .unwrap();
        zbus::blocking::fdo::DBusProxy::new(&c)
            .unwrap()
            .name_has_owner(name.try_into().unwrap())
            .unwrap()
    }

    fn daemon(dir: &Dir, address: &str, extra: &str) -> Reap {
        std::fs::create_dir_all(dir.path("xdg/lossless")).unwrap();
        std::fs::write(
            dir.path("xdg/lossless/t1.toml"),
            format!(
                "device = \"null\"\nsocket = \"{}\"\n{extra}",
                dir.path("s.sock").display()
            ),
        )
        .unwrap();
        Reap(spawn_env(
            dir,
            &["--profile", "t1"],
            &[
                ("XDG_STATE_HOME", &dir.path("state")),
                ("DBUS_SESSION_BUS_ADDRESS", Path::new(address)),
            ],
        ))
    }

    const NAME: &str = "org.mpris.MediaPlayer2.lossless.t1";

    #[test]
    fn losslessd_registers_on_session_bus() {
        let dir = Dir::new();
        let Some((_bus, address)) = private_bus(&dir) else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        let _child = daemon(&dir, &address, "");
        let _ = connect(&dir.path("s.sock"));
        let end = Instant::now() + Duration::from_secs(5);
        while !owned(&address, NAME) {
            assert!(Instant::now() < end, "{NAME} never registered");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn mpris_false_stays_off_the_bus() {
        let dir = Dir::new();
        let Some((_bus, address)) = private_bus(&dir) else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        let _child = daemon(&dir, &address, "mpris = false\n");
        let c = connect(&dir.path("s.sock"));
        assert!(call(&c, r#"{"id":1,"cmd":"status"}"#).contains("status"));
        assert!(!owned(&address, NAME), "registered despite mpris = false");
    }
}
