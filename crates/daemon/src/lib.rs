#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread;

use anyhow::{Context, Result, bail};
use lossless_engine::{Command as PlayerCmd, Player};
use lossless_library::{Library, collect, read_track};
use lossless_proto::{
    Body, Command, Event, Outcome, Request, ServerMessage, Track, decode, encode,
};

mod config;

use config::expand_tilde;
pub use config::{Config, config_path, load_config, parse_config};

/// Bind the socket, owner-only. An existing path is replaced only if it is
/// itself a stale socket: never a live daemon, a symlink or any other file.
pub fn bind(path: &Path) -> Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("checking {}", path.display())),
        Ok(meta) if !meta.file_type().is_socket() => {
            bail!("{} exists and is not a socket", path.display())
        }
        Ok(_) => {
            if UnixStream::connect(path).is_ok() {
                bail!("another daemon is listening on {}", path.display());
            }
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale {}", path.display()))?;
        }
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    Ok(listener)
}

type Writer = Arc<Mutex<UnixStream>>;

fn send(writer: &Writer, msg: &ServerMessage) -> std::io::Result<()> {
    let line = encode(msg).map_err(std::io::Error::other)?;
    let mut w = writer.lock().unwrap_or_else(PoisonError::into_inner);
    w.write_all(line.as_bytes())
}

type Subscribers = Arc<Mutex<Vec<Writer>>>;

fn broadcast(subscribers: &Subscribers, event: Event) {
    let msg = ServerMessage::Event { event };
    let mut subs = subscribers.lock().unwrap_or_else(PoisonError::into_inner);
    subs.retain(|w| send(w, &msg).is_ok());
}

/// Forward player events to every subscribed client, dropping dead ones.
fn fan_out(events: Receiver<Event>, subscribers: Subscribers) {
    for event in events {
        broadcast(&subscribers, event);
    }
}

/// Clears a flag when dropped, including while unwinding from a panic.
struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Rescan `root` in the background unless a scan is already running. If the
/// scan panics (say, on a malformed tag), the flag is still cleared so later
/// updates aren't silently ignored.
fn spawn_scan(
    root: PathBuf,
    library: Arc<RwLock<Library>>,
    scanning: Arc<AtomicBool>,
    subscribers: Subscribers,
) -> Option<thread::JoinHandle<()>> {
    if scanning.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(thread::spawn(move || {
        let _clear = ClearOnDrop(scanning);
        match Library::scan(&root) {
            Ok(lib) => {
                let tracks = lib.len();
                *library.write().unwrap_or_else(PoisonError::into_inner) = lib;
                eprintln!("library: {tracks} tracks in {}", root.display());
                broadcast(&subscribers, Event::LibraryChanged { tracks });
            }
            Err(e) => eprintln!("library: scan failed: {e:#}"),
        }
    }))
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
        let lib = self.library.read().unwrap_or_else(PoisonError::into_inner);
        match lib.get(&path.display().to_string()) {
            Some(t) => t.clone(),
            None => read_track(path),
        }
    }

    /// Start a scan; `None` if one is already running.
    fn rescan(&self) -> Result<Option<thread::JoinHandle<()>>, String> {
        let root = self
            .music_dir
            .clone()
            .ok_or_else(|| "no music_dir configured".to_string())?;
        Ok(spawn_scan(
            root,
            Arc::clone(&self.library),
            Arc::clone(&self.scanning),
            Arc::clone(&self.subscribers),
        ))
    }

    fn handle(&self, cmd: Command, writer: &Writer) -> Outcome {
        let ok = |b| Outcome::Ok(b);
        let play = |c| {
            self.player.send(c);
            Outcome::Ok(Body::Done)
        };
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
                let lib = self.library.read().unwrap_or_else(PoisonError::into_inner);
                ok(Body::Tracks(
                    lib.search(&query).into_iter().cloned().collect(),
                ))
            }
            Command::Update => match self.rescan() {
                Ok(_) => ok(Body::Done),
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
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Arc::clone(writer));
                ok(Body::Done)
            }
            Command::Play { index } => play(PlayerCmd::Play(index)),
            Command::Pause => play(PlayerCmd::Pause),
            Command::Toggle => play(PlayerCmd::Toggle),
            Command::Stop => play(PlayerCmd::Stop),
            Command::Next => play(PlayerCmd::Next),
            Command::Previous => play(PlayerCmd::Previous),
            Command::Seek { ms } => play(PlayerCmd::Seek { ms }),
            Command::Clear => play(PlayerCmd::Clear),
        }
    }
}

/// Longest accepted request line. Real requests are tiny; the cap stops a
/// client from making the daemon buffer an endless line.
pub const MAX_REQUEST: usize = 64 * 1024;

fn bad(msg: String) -> ServerMessage {
    ServerMessage::Reply {
        id: 0,
        result: Outcome::Err(msg),
    }
}

fn serve(state: &State, stream: UnixStream) -> Result<()> {
    let writer: Writer = Arc::new(Mutex::new(stream.try_clone()?));
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut reader)
            .take(MAX_REQUEST as u64 + 1)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        if n > MAX_REQUEST {
            send(
                &writer,
                &bad(format!("request longer than {MAX_REQUEST} bytes")),
            )?;
            return Ok(());
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            send(&writer, &bad("request is not UTF-8".into()))?;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match decode::<Request>(line) {
            Ok(req) => ServerMessage::Reply {
                id: req.id,
                result: state.handle(req.command, &writer),
            },
            Err(e) => bad(format!("bad request: {e}")),
        };
        send(&writer, &reply)?;
    }
}

/// Load the config at `config`, bind its socket (or `default_socket`), open
/// its device and serve until the listener fails.
pub fn run(config: &Path, default_socket: PathBuf) -> Result<()> {
    let cfg = load_config(config)?;
    let socket = cfg.socket.unwrap_or(default_socket);
    let listener = bind(&socket)?;
    eprintln!(
        "losslessd: device {} socket {}",
        cfg.device,
        socket.display()
    );
    let (player, events) = Player::spawn(cfg.device);
    let daemon = Daemon::start(player, events, cfg.music_dir)?;
    if let Err(e) = daemon.rescan() {
        eprintln!("library: {e}");
    }
    daemon.serve(listener);
    Ok(())
}

/// A running daemon: player, library and subscribers, ready to serve.
pub struct Daemon {
    state: Arc<State>,
}

impl Daemon {
    /// Wire a player (and its event stream) to a new daemon.
    pub fn start(
        player: Player,
        events: Receiver<Event>,
        music_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        {
            let subs = Arc::clone(&subscribers);
            thread::Builder::new()
                .name("events".into())
                .spawn(move || fan_out(events, subs))?;
        }
        Ok(Self {
            state: Arc::new(State {
                player,
                music_dir,
                subscribers,
                library: Arc::new(RwLock::new(Library::default())),
                scanning: Arc::new(AtomicBool::new(false)),
            }),
        })
    }

    /// Start a background scan of the music directory, if there is one.
    pub fn rescan(&self) -> Result<(), String> {
        self.state.rescan().map(drop)
    }

    /// Accept clients until the listener fails; one thread per client.
    pub fn serve(&self, listener: UnixListener) {
        self.accept(listener.incoming());
    }

    /// Serve each connection; a failed accept is logged and skipped.
    fn accept(&self, incoming: impl IntoIterator<Item = std::io::Result<UnixStream>>) {
        for stream in incoming {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("accept: {e}");
                    continue;
                }
            };
            let state = Arc::clone(&self.state);
            thread::spawn(move || {
                if let Err(e) = serve(&state, stream) {
                    eprintln!("client: {e:#}");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::ClearOnDrop;

    #[test]
    fn scan_flag_is_cleared_when_the_scan_panics() {
        let flag = Arc::new(AtomicBool::new(true));
        let guard = ClearOnDrop(Arc::clone(&flag));
        let crashed = std::thread::spawn(move || {
            let _clear = guard;
            panic!("malformed tag");
        })
        .join();
        assert!(crashed.is_err());
        assert!(!flag.load(Ordering::SeqCst));
    }
}
