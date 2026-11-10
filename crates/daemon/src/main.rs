#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use anyhow::{Context, Result, bail};
use lossless_engine::{Command as PlayerCmd, Player};
use lossless_library::{Library, collect, read_track};
use lossless_proto::{
    Body, Command, Event, Outcome, Request, ServerMessage, Track, decode, default_socket, encode,
};
use serde::Deserialize;

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

type Subscribers = Arc<Mutex<Vec<Writer>>>;

fn broadcast(subscribers: &Subscribers, event: Event) {
    let msg = ServerMessage::Event { event };
    let mut subs = subscribers.lock().unwrap_or_else(|e| e.into_inner());
    subs.retain(|w| send(w, &msg).is_ok());
}

/// Forward player events to every subscribed client, dropping dead ones.
fn fan_out(events: Receiver<Event>, subscribers: Subscribers) {
    for event in events {
        broadcast(&subscribers, event);
    }
}

/// Rescan `root` in the background unless a scan is already running.
fn spawn_scan(
    root: PathBuf,
    library: Arc<RwLock<Library>>,
    scanning: Arc<AtomicBool>,
    subscribers: Subscribers,
) {
    if scanning.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        match Library::scan(&root) {
            Ok(lib) => {
                let tracks = lib.len();
                *library.write().unwrap_or_else(|e| e.into_inner()) = lib;
                eprintln!("library: {tracks} tracks in {}", root.display());
                broadcast(&subscribers, Event::LibraryChanged { tracks });
            }
            Err(e) => eprintln!("library: scan failed: {e:#}"),
        }
        scanning.store(false, Ordering::SeqCst);
    });
}

struct State {
    player: Player,
    music_dir: Option<PathBuf>,
    subscribers: Subscribers,
    library: Arc<RwLock<Library>>,
    scanning: Arc<AtomicBool>,
}

impl State {
    fn resolve(&self, p: &str) -> PathBuf {
        let p = expand_tilde(Path::new(p));
        match (&self.music_dir, p.is_absolute()) {
            (Some(dir), false) => dir.join(p),
            _ => p,
        }
    }

    /// Library entry for `path`, or its tags read now if it isn't indexed.
    fn track(&self, path: &Path) -> Track {
        let lib = self.library.read().unwrap_or_else(|e| e.into_inner());
        match lib.get(&path.display().to_string()) {
            Some(t) => t.clone(),
            None => read_track(path),
        }
    }

    fn rescan(&self) -> Result<(), String> {
        let root = self
            .music_dir
            .clone()
            .ok_or_else(|| "no music_dir configured".to_string())?;
        spawn_scan(
            root,
            Arc::clone(&self.library),
            Arc::clone(&self.scanning),
            Arc::clone(&self.subscribers),
        );
        Ok(())
    }

    fn handle(&self, cmd: Command, writer: &Writer) -> Outcome {
        let ok = |b| Outcome::Ok(b);
        match cmd {
            Command::Status => {
                let snap = self.player.snapshot();
                let mut status = snap.status;
                status.track = status
                    .index
                    .and_then(|i| snap.queue.get(i))
                    .map(|p| self.track(p));
                ok(Body::Status(status))
            }
            Command::Queue => ok(Body::Queue(
                self.player
                    .snapshot()
                    .queue
                    .iter()
                    .map(|p| self.track(p))
                    .collect(),
            )),
            Command::Search { query } => {
                let lib = self.library.read().unwrap_or_else(|e| e.into_inner());
                ok(Body::Tracks(
                    lib.search(&query).into_iter().cloned().collect(),
                ))
            }
            Command::Update => match self.rescan() {
                Ok(()) => ok(Body::Done),
                Err(e) => Outcome::Err(e),
            },
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
                    Command::Status
                    | Command::Queue
                    | Command::Add { .. }
                    | Command::Subscribe
                    | Command::Search { .. }
                    | Command::Update => unreachable!("handled above"),
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
        library: Arc::new(RwLock::new(Library::default())),
        scanning: Arc::new(AtomicBool::new(false)),
    });
    if state.music_dir.is_some() {
        let _ = state.rescan();
    }

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
