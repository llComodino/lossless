#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, bail};
use lossless_engine::{Command as PlayerCmd, Player};
use lossless_proto::{
    Body, Command, Event, Outcome, Request, ServerMessage, Track, decode, default_socket, encode,
};
use serde::Deserialize;

const AUDIO_EXTENSIONS: &[&str] = &["flac", "mp3"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// ALSA device, e.g. "hw:CARD=PRO2,DEV=0" for bit-perfect, "default" for
    /// the PipeWire path.
    #[serde(default = "default_device")]
    device: String,
    /// Relative paths in `add` resolve against this.
    music_dir: Option<PathBuf>,
    socket: Option<PathBuf>,
}

fn default_device() -> String {
    "default".into()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn expand_tilde(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn config_path() -> PathBuf {
    let base =
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home().join(".config"), PathBuf::from);
    base.join("lossless/config.toml")
}

fn load_config() -> Result<Config> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    cfg.music_dir = cfg.music_dir.map(|d| expand_tilde(&d));
    cfg.socket = cfg.socket.map(|s| expand_tilde(&s));
    Ok(cfg)
}

/// Bind the socket, replacing a stale file but never a live daemon.
fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("another daemon is listening on {}", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

type Writer = Arc<Mutex<UnixStream>>;

fn send(writer: &Writer, msg: &ServerMessage) -> std::io::Result<()> {
    let line = encode(msg).map_err(std::io::Error::other)?;
    let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
    w.write_all(line.as_bytes())
}

/// Forward player events to every subscribed client, dropping dead ones.
fn fan_out(events: Receiver<Event>, subscribers: Arc<Mutex<Vec<Writer>>>) {
    for event in events {
        let msg = ServerMessage::Event { event };
        let mut subs = subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|w| send(w, &msg).is_ok());
    }
}

/// Expand files and directories (recursively, sorted) into playable tracks.
fn collect(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    if meta.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for entry in entries {
            collect(&entry, out)?;
        }
    } else if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
    {
        out.push(path.to_path_buf());
    }
    Ok(())
}

struct State {
    player: Player,
    music_dir: Option<PathBuf>,
    subscribers: Arc<Mutex<Vec<Writer>>>,
}

impl State {
    fn resolve(&self, p: &str) -> PathBuf {
        let p = expand_tilde(Path::new(p));
        match (&self.music_dir, p.is_absolute()) {
            (Some(dir), false) => dir.join(p),
            _ => p,
        }
    }

    fn handle(&self, cmd: Command, writer: &Writer) -> Outcome {
        let ok = |b| Outcome::Ok(b);
        match cmd {
            Command::Status => ok(Body::Status(self.player.snapshot().status)),
            Command::Queue => ok(Body::Queue(
                self.player
                    .snapshot()
                    .queue
                    .iter()
                    .map(|p| Track {
                        path: p.display().to_string(),
                        title: None,
                        artist: None,
                        album: None,
                        duration_ms: None,
                        format: None,
                    })
                    .collect(),
            )),
            Command::Add { paths } => {
                let mut tracks = Vec::new();
                for p in &paths {
                    if let Err(e) = collect(&self.resolve(p), &mut tracks) {
                        return Outcome::Err(format!("{e:#}"));
                    }
                }
                if tracks.is_empty() {
                    return Outcome::Err("no playable files".into());
                }
                self.player.send(PlayerCmd::Add(tracks));
                ok(Body::Done)
            }
            Command::Subscribe => {
                self.subscribers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Arc::clone(writer));
                ok(Body::Done)
            }
            other => {
                self.player.send(match other {
                    Command::Play { index } => PlayerCmd::Play(index),
                    Command::Pause => PlayerCmd::Pause,
                    Command::Toggle => PlayerCmd::Toggle,
                    Command::Stop => PlayerCmd::Stop,
                    Command::Next => PlayerCmd::Next,
                    Command::Previous => PlayerCmd::Previous,
                    Command::Seek { ms } => PlayerCmd::Seek { ms },
                    Command::Clear => PlayerCmd::Clear,
                    Command::Status | Command::Queue | Command::Add { .. } | Command::Subscribe => {
                        unreachable!("handled above")
                    }
                });
                ok(Body::Done)
            }
        }
    }
}

fn serve(state: &State, stream: UnixStream) -> Result<()> {
    let writer: Writer = Arc::new(Mutex::new(stream.try_clone()?));
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match decode::<Request>(&line) {
            Ok(req) => ServerMessage::Reply {
                id: req.id,
                result: state.handle(req.command, &writer),
            },
            Err(e) => ServerMessage::Reply {
                id: 0,
                result: Outcome::Err(format!("bad request: {e}")),
            },
        };
        send(&writer, &reply)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let cfg = load_config()?;
    let socket = cfg.socket.clone().unwrap_or_else(default_socket);
    let listener = bind(&socket)?;
    eprintln!(
        "losslessd: device {} socket {}",
        cfg.device,
        socket.display()
    );

    let (player, events) = Player::spawn(cfg.device);
    let subscribers = Arc::new(Mutex::new(Vec::new()));
    {
        let subs = Arc::clone(&subscribers);
        thread::Builder::new()
            .name("events".into())
            .spawn(move || fan_out(events, subs))?;
    }
    let state = Arc::new(State {
        player,
        music_dir: cfg.music_dir,
        subscribers,
    });

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept: {e}");
                continue;
            }
        };
        let state = Arc::clone(&state);
        thread::spawn(move || {
            if let Err(e) = serve(&state, stream) {
                eprintln!("client: {e:#}");
            }
        });
    }
    Ok(())
}
