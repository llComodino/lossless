#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::thread;

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use lossless_engine::{Command as PlayerCmd, Output, Player, Snapshot, choose};
use lossless_library::{Library, collect, read_track};
use lossless_proto::profile::Profile;
use lossless_proto::{
    Body, Command, Event, Outcome, Property, Request, ServerMessage, Track, decode, encode,
};

mod config;
mod error;
mod logger;
#[cfg(feature = "mpris")]
mod mpris;
pub mod state;

use config::expand_tilde;
pub use config::{
    Config, LogLevel, MAX_CONFIG, check_device, check_path, load_config, parse_config,
};
pub use error::RequestError;
pub use logger::init_logging;

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

/// Events a subscriber may fall behind by (past the socket buffer) before
/// it counts as stalled.
const BACKLOG: usize = 256;

struct Subscriber {
    tx: SyncSender<Arc<str>>,
    /// The connection, to close it when the subscriber is dropped.
    stream: UnixStream,
    writer: Writer,
}

type Subscribers = Arc<Mutex<Vec<Subscriber>>>;

/// A writer thread per subscriber, so one that stops reading blocks only
/// itself; it ends when its channel is dropped or a write fails. Once per
/// connection. If no thread can be had the connection is closed.
fn subscribe(subscribers: &Subscribers, writer: &Writer) {
    let mut subs = subscribers.lock().unwrap_or_else(PoisonError::into_inner);
    if subs.iter().any(|s| Arc::ptr_eq(&s.writer, writer)) {
        return;
    }
    let lock = writer.lock().unwrap_or_else(PoisonError::into_inner);
    let Ok(stream) = lock.try_clone() else {
        let _ = lock.shutdown(Shutdown::Both);
        return;
    };
    drop(lock);
    let (tx, rx) = sync_channel::<Arc<str>>(BACKLOG);
    let thread_writer = Arc::clone(writer);
    let spawned = thread::Builder::new().stack_size(64 * 1024).spawn(move || {
        for line in rx {
            let mut w = thread_writer.lock().unwrap_or_else(PoisonError::into_inner);
            if w.write_all(line.as_bytes()).is_err() {
                break;
            }
        }
    });
    if let Err(e) = spawned {
        warn!("no writer thread for a subscriber: {e}");
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    subs.push(Subscriber {
        tx,
        stream,
        writer: Arc::clone(writer),
    });
}

/// Forgets the subscription of a connection that ended.
fn unsubscribe(subscribers: &Subscribers, writer: &Writer) {
    subscribers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|s| !Arc::ptr_eq(&s.writer, writer));
}

/// Never blocks. A subscriber whose backlog is full has stopped reading: its
/// connection is closed, so the client sees EOF and resyncs on reconnect.
fn broadcast(subscribers: &Subscribers, event: Event) {
    let mut subs = subscribers.lock().unwrap_or_else(PoisonError::into_inner);
    if subs.is_empty() {
        return;
    }
    let line: Arc<str> = match encode(&ServerMessage::Event { event }) {
        Ok(line) => line.into(),
        Err(e) => {
            warn!("event not sent: {e}");
            return;
        }
    };
    subs.retain(|s| match s.tx.try_send(Arc::clone(&line)) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            warn!("closing a subscriber that stopped reading");
            let _ = s.stream.shutdown(Shutdown::Both);
            false
        }
        Err(TrySendError::Disconnected(_)) => false,
    });
}

/// What to persist from `snap`. Paths go through UTF-8 (JSON can't carry
/// anything else); one that isn't valid is saved mangled and dropped as
/// missing on restore, so the rest of the queue still survives.
fn saved(snap: &Snapshot) -> state::Saved {
    state::Saved {
        queue: snap
            .queue
            .iter()
            .map(|p| PathBuf::from(p.to_string_lossy().into_owned()))
            .collect(),
        index: snap.status.index,
        position_ms: snap.status.position_ms,
        repeat: snap.status.repeat,
        random: snap.status.random,
    }
}

/// Forward player events to every subscribed client, dropping dead ones, and
/// save the state once per batch of queued events if any changes it (before
/// broadcasting, so a client that saw the event can rely on the file). The stream ends only when the
/// player thread is gone, so `on_player_gone` runs.
fn fan_out(
    events: Receiver<Event>,
    mut save: impl FnMut(),
    subscribers: Subscribers,
    on_player_gone: Box<dyn FnOnce() + Send>,
) {
    while let Ok(first) = events.recv() {
        let batch: Vec<Event> = std::iter::once(first).chain(events.try_iter()).collect();
        if batch.iter().any(|e| {
            matches!(
                e,
                Event::QueueChanged
                    | Event::TrackChanged { .. }
                    | Event::State { .. }
                    | Event::Mode { .. }
            )
        }) {
            save();
        }
        for event in batch {
            broadcast(&subscribers, event);
        }
    }
    on_player_gone();
}

/// Save the daemon's current state, if it is still running.
fn save_state(daemon: &Weak<State>) {
    if let Some(d) = daemon.upgrade()
        && let Err(e) = state::save(&d.state_file, &saved(&d.player.snapshot()))
    {
        warn!("state: saving {}: {e}", d.state_file.display());
    }
}

/// Clears a flag when dropped, including while unwinding from a panic.
struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Install `new` as the library, bumping its generation, and describe the
/// change. The write lock is released before this returns, so the caller
/// broadcasts without blocking readers.
fn swap(library: &RwLock<Library>, mut new: Library) -> Event {
    let mut guard = library.write().unwrap_or_else(PoisonError::into_inner);
    let changes = new.replaces(&guard);
    let (generation, tracks) = (new.generation(), new.len());
    *guard = new;
    Event::LibraryChanged {
        generation,
        tracks,
        added: changes.added,
        removed: changes.removed,
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
                let event = swap(&library, lib);
                if let Event::LibraryChanged { tracks, .. } = &event {
                    info!("library: {tracks} tracks in {}", root.display());
                }
                broadcast(&subscribers, event);
            }
            Err(e) => warn!("library: scan failed: {e}"),
        }
    }))
}

struct State {
    player: Player,
    state_file: PathBuf,
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

    /// Start a scan; `ScanRunning` if one is already running.
    fn rescan(&self) -> Result<thread::JoinHandle<()>, RequestError> {
        let root = self.music_dir.clone().ok_or(RequestError::NoMusicDir)?;
        spawn_scan(
            root,
            Arc::clone(&self.library),
            Arc::clone(&self.scanning),
            Arc::clone(&self.subscribers),
        )
        .ok_or(RequestError::ScanRunning)
    }

    fn tracks(&self, paths: &[String]) -> Result<Vec<PathBuf>, RequestError> {
        let mut tracks = Vec::new();
        for p in paths {
            collect(&self.resolve(p), &mut tracks)?;
        }
        if tracks.is_empty() {
            return Err(RequestError::NoPlayableFiles(paths.to_vec()));
        }
        Ok(tracks)
    }

    fn handle(&self, cmd: Command, writer: &Writer) -> Result<Body, RequestError> {
        let play = |c| {
            self.player
                .send(c)
                .map(|()| Body::Done)
                .map_err(RequestError::from)
        };
        match cmd {
            Command::Status => {
                let snap = self.player.snapshot();
                let mut status = snap.status;
                status.track = status
                    .index
                    .and_then(|i| snap.queue.get(i))
                    .map(|p| self.track(p));
                Ok(Body::Status(Box::new(status)))
            }
            Command::Queue => Ok(Body::Queue(
                self.player
                    .snapshot()
                    .queue
                    .iter()
                    .map(|p| self.track(p))
                    .collect(),
            )),
            Command::Tracks => {
                let lib = self.library.read().unwrap_or_else(PoisonError::into_inner);
                Ok(Body::Library {
                    generation: lib.generation(),
                    tracks: lib.tracks().to_vec(),
                })
            }
            Command::Search { query } => {
                let lib = self.library.read().unwrap_or_else(PoisonError::into_inner);
                Ok(Body::Tracks(
                    lib.search(&query).into_iter().cloned().collect(),
                ))
            }
            Command::Browse { path } => {
                if self.music_dir.is_none() {
                    return Err(RequestError::NoMusicDir);
                }
                let lib = self.library.read().unwrap_or_else(PoisonError::into_inner);
                Ok(Body::Entries(lib.browse(&path)?))
            }
            Command::Artists => {
                let lib = self.library.read().unwrap_or_else(PoisonError::into_inner);
                Ok(Body::Names(lib.artists()))
            }
            Command::Albums { artist } => {
                let lib = self.library.read().unwrap_or_else(PoisonError::into_inner);
                Ok(Body::Names(lib.albums(artist.as_deref())))
            }
            Command::Find { artist, album } => {
                let lib = self.library.read().unwrap_or_else(PoisonError::into_inner);
                Ok(Body::Tracks(
                    lib.find(artist.as_deref(), album.as_deref())
                        .into_iter()
                        .cloned()
                        .collect(),
                ))
            }
            Command::Update => self.rescan().map(|_| Body::Done),
            Command::Get {
                property: Property::MusicDir,
            } => self
                .music_dir
                .as_ref()
                .map(|d| Body::Value(d.display().to_string()))
                .ok_or(RequestError::NoMusicDir),
            Command::Add { paths, at } => play(PlayerCmd::Add {
                paths: self.tracks(&paths)?,
                at,
            }),
            Command::Replace { paths } => play(PlayerCmd::Replace(self.tracks(&paths)?)),
            Command::Remove { indices } => play(PlayerCmd::Remove(indices)),
            Command::Move { from, to } => play(PlayerCmd::Move { from, to }),
            Command::Subscribe => {
                subscribe(&self.subscribers, writer);
                Ok(Body::Done)
            }
            Command::Play { index } => play(PlayerCmd::Play(index)),
            Command::Pause => play(PlayerCmd::Pause),
            Command::Toggle => play(PlayerCmd::Toggle),
            Command::Stop => play(PlayerCmd::Stop),
            Command::Next => play(PlayerCmd::Next),
            Command::Previous => play(PlayerCmd::Previous),
            Command::Seek { ms } => play(PlayerCmd::Seek { ms }),
            Command::Clear => play(PlayerCmd::Clear),
            Command::SetMode { repeat, random } => play(PlayerCmd::SetMode { repeat, random }),
            Command::SetVolume { percent } => play(PlayerCmd::SetVolume(percent)),
        }
    }
}

/// Longest accepted request line: room for queue edits naming a whole
/// artist, while a client still can't make the daemon buffer an endless
/// line. The buffer grows only as bytes arrive.
pub const MAX_REQUEST: usize = 4 * 1024 * 1024;

fn bad(msg: String) -> ServerMessage {
    ServerMessage::Reply {
        id: 0,
        result: Outcome::Err(RequestError::BadRequest(msg).to_wire()),
    }
}

fn serve(state: &State, stream: UnixStream) -> Result<()> {
    let writer: Writer = Arc::new(Mutex::new(stream.try_clone()?));
    let result = session(state, &writer, stream);
    unsubscribe(&state.subscribers, &writer);
    result
}

fn session(state: &State, writer: &Writer, stream: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        buf.shrink_to(64 * 1024);
        let n = (&mut reader)
            .take(MAX_REQUEST as u64 + 1)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        if n > MAX_REQUEST {
            send(
                writer,
                &bad(format!("request longer than {MAX_REQUEST} bytes")),
            )?;
            return Ok(());
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            send(writer, &bad("request is not UTF-8".into()))?;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match decode::<Request>(line) {
            Ok(req) => {
                debug!("request {}: {:?}", req.id, req.command);
                let result = match state.handle(req.command, writer) {
                    Ok(body) => Outcome::Ok(body),
                    Err(e) => Outcome::Err(e.to_wire()),
                };
                let label = if matches!(result, Outcome::Ok(_)) {
                    "ok"
                } else {
                    "error"
                };
                debug!("reply {}: {label}", req.id);
                ServerMessage::Reply { id: req.id, result }
            }
            Err(e) => bad(format!("bad request: {e}")),
        };
        send(writer, &reply)?;
    }
}

/// Load the config at `config`, check the state directory for `state` is
/// writable, bind its socket (or `default_socket`), open its device, restore
/// the session saved at `state` and serve until the listener fails.
pub fn run(
    config: &Path,
    default_socket: PathBuf,
    state: PathBuf,
    log_override: Option<LogLevel>,
    profile: &Profile,
) -> Result<()> {
    let cfg = load_config(config)?;
    init_logging(log_override.unwrap_or(cfg.log_level));
    #[cfg(not(feature = "mpris"))]
    if cfg.mpris {
        info!("mpris: not built in; ignoring the mpris setting");
    }
    let _ = profile;
    let dir = state.parent().unwrap_or(Path::new("/"));
    state::prepare(&state)
        .with_context(|| format!("preparing state directory {}", dir.display()))?;
    let socket = cfg.socket.unwrap_or(default_socket);
    let listener = bind(&socket)?;
    info!("device {} socket {}", cfg.device, socket.display());
    let mixer = choose(&cfg.device, cfg.mixer, &cfg.mixer_control).open();
    let (player, events) = Player::spawn_with_mixer(Output::new(cfg.device), mixer);
    let on_player_gone = Box::new(|| {
        eprintln!("losslessd: player thread died; exiting");
        std::process::exit(70);
    });
    let daemon = Daemon::start(player, events, cfg.music_dir, state, on_player_gone)?;
    if let Err(e) = daemon.rescan() {
        warn!("library: {e}");
    }
    daemon.serve(listener);
    Ok(())
}

/// Put back the session saved at `path`, minus entries whose files are gone
/// (the modes survive even if all of them are).
fn restore(player: &Player, path: &Path) -> Result<()> {
    let saved = match state::load(path) {
        Ok(Some(s)) => s,
        Ok(None) => return Ok(()),
        Err(e) => {
            warn!("state: {e}; starting with an empty queue");
            return Ok(());
        }
    };
    let (s, dropped) = saved.prune();
    if dropped > 0 {
        warn!("state: dropped {dropped} missing entries");
    }
    if s.queue.is_empty() {
        player.send(PlayerCmd::SetMode {
            repeat: Some(s.repeat),
            random: Some(s.random),
        })?;
        return Ok(());
    }
    info!("state: restored {} entries", s.queue.len());
    player.send(PlayerCmd::Restore {
        queue: s.queue,
        index: s.index,
        position_ms: s.position_ms,
        repeat: s.repeat,
        random: s.random,
    })?;
    Ok(())
}

/// A running daemon: player, library and subscribers, ready to serve.
pub struct Daemon {
    state: Arc<State>,
    sessions: Arc<Sessions>,
}

/// Live client connections, so `shutdown` can close them.
#[derive(Default)]
struct Sessions {
    live: Mutex<Live>,
    ended: Condvar,
}

#[derive(Default)]
struct Live {
    stopping: bool,
    next: u64,
    streams: HashMap<u64, UnixStream>,
    /// Where `serve` listens, to wake its `accept` on shutdown.
    addr: Option<PathBuf>,
}

impl Sessions {
    fn lock(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Track `stream` until `end`; `None` once shutting down.
    fn register(&self, stream: &UnixStream) -> Option<u64> {
        let mut live = self.lock();
        if live.stopping {
            return None;
        }
        let id = live.next;
        live.next += 1;
        if let Ok(clone) = stream.try_clone() {
            live.streams.insert(id, clone);
        }
        Some(id)
    }

    fn end(&self, id: u64) {
        self.lock().streams.remove(&id);
        self.ended.notify_all();
    }
}

impl Daemon {
    /// Wire a player (and its event stream) to a new daemon, restoring the
    /// session saved at `state_file`. Its directory must exist (`run`
    /// prepares it); the file may be missing or broken (then the queue starts
    /// empty).
    pub fn start(
        player: Player,
        events: Receiver<Event>,
        music_dir: Option<PathBuf>,
        state_file: PathBuf,
        on_player_gone: Box<dyn FnOnce() + Send>,
    ) -> Result<Self> {
        restore(&player, &state_file)?;
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(State {
            player,
            state_file,
            music_dir,
            subscribers: Arc::clone(&subscribers),
            library: Arc::new(RwLock::new(Library::default())),
            scanning: Arc::new(AtomicBool::new(false)),
        });
        let weak = Arc::downgrade(&state);
        thread::Builder::new()
            .name("events".into())
            .spawn(move || {
                fan_out(
                    events,
                    move || save_state(&weak),
                    subscribers,
                    on_player_gone,
                )
            })?;
        Ok(Self {
            state,
            sessions: Arc::default(),
        })
    }

    /// Start a background scan of the music directory, if there is one.
    pub fn rescan(&self) -> Result<(), RequestError> {
        self.state.rescan().map(drop)
    }

    /// Accept clients until the listener fails or `shutdown`; one thread
    /// per client.
    pub fn serve(&self, listener: UnixListener) {
        self.sessions.lock().addr = listener
            .local_addr()
            .ok()
            .and_then(|a| a.as_pathname().map(Path::to_path_buf));
        self.accept(listener.incoming());
    }

    /// Stop accepting, close every client connection and wait until their
    /// sessions have ended. Clients see EOF, as if the daemon had exited.
    pub fn shutdown(&self) {
        let mut live = self.sessions.lock();
        live.stopping = true;
        for stream in live.streams.values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let addr = live.addr.take();
        drop(live);
        if let Some(addr) = addr {
            let _ = UnixStream::connect(addr);
        }
        let live = self.sessions.lock();
        let _live = self
            .sessions
            .ended
            .wait_while(live, |l| !l.streams.is_empty())
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Serve each connection; a failed accept is logged and skipped.
    fn accept(&self, incoming: impl IntoIterator<Item = std::io::Result<UnixStream>>) {
        for stream in incoming {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    warn!("accept: {e}");
                    continue;
                }
            };
            let Some(id) = self.sessions.register(&stream) else {
                return;
            };
            let state = Arc::clone(&self.state);
            let sessions = Arc::clone(&self.sessions);
            thread::spawn(move || {
                if let Err(e) = serve(&state, stream) {
                    warn!("client: {e:#}");
                }
                sessions.end(id);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, RwLock};

    use lossless_engine::fixtures::{Dir, PacedSink, flac};
    use lossless_engine::{Command as PlayerCmd, Player};
    use lossless_library::{Library, collect};
    use lossless_proto::{PlayState, Repeat};

    use super::{
        ClearOnDrop, Daemon, MAX_REQUEST, Subscribers, saved, serve, spawn_scan, subscribe, swap,
        unsubscribe,
    };

    fn scan(root: PathBuf, busy: bool) -> (Option<std::thread::JoinHandle<()>>, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(busy));
        let library = Arc::new(RwLock::new(Library::default()));
        let subs = Arc::new(Mutex::new(Vec::new()));
        (spawn_scan(root, library, Arc::clone(&flag), subs), flag)
    }

    #[test]
    fn swap_releases_the_write_lock_before_the_broadcast() {
        let library = RwLock::new(Library::default());
        let event = swap(&library, Library::default());
        assert!(library.try_write().is_ok());
        assert!(matches!(
            event,
            lossless_proto::Event::LibraryChanged {
                generation: 1,
                tracks: 0,
                ..
            }
        ));
    }

    #[test]
    fn no_second_scan_while_one_runs() {
        let (handle, flag) = scan(PathBuf::from("/nonexistent"), true);
        assert!(handle.is_none());
        // The running scan still owns the flag.
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn a_failed_scan_keeps_the_library_and_sends_nothing() {
        let dir = Dir::new();
        let library = Arc::new(RwLock::new(Library::default()));
        swap(&library, Library::default());
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let subs: Subscribers = Arc::new(Mutex::new(Vec::new()));
        subscribe(&subs, &Arc::new(Mutex::new(ours)));
        let flag = Arc::new(AtomicBool::new(false));
        let handle = spawn_scan(
            dir.path("missing"),
            Arc::clone(&library),
            flag,
            Arc::clone(&subs),
        );
        handle.unwrap().join().unwrap();
        assert_eq!(library.read().unwrap().generation(), 1);
        theirs.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 1];
        let read = std::io::Read::read(&mut theirs, &mut buf);
        assert_eq!(read.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn a_connection_that_ends_is_unsubscribed() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(ours));
        let subs: Subscribers = Arc::new(Mutex::new(Vec::new()));
        subscribe(&subs, &writer);
        subscribe(&subs, &writer);
        assert_eq!(subs.lock().unwrap().len(), 1);
        unsubscribe(&subs, &writer);
        assert!(subs.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_scan_clears_the_flag() {
        let dir = Dir::new();
        let (handle, flag) = scan(dir.path("missing"), false);
        handle.unwrap().join().unwrap();
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn a_client_gone_before_its_error_reply_ends_only_its_session() {
        let dir = Dir::new();
        let (player, events) = Player::spawn_with(PacedSink::default());
        let daemon =
            Daemon::start(player, events, None, dir.path("s.json"), Box::new(|| {})).unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        client.shutdown(std::net::Shutdown::Read).unwrap();
        let writer = std::thread::spawn(move || client.write_all(&vec![b'x'; MAX_REQUEST + 1]));
        let err = serve(&daemon.state, server).unwrap_err();
        assert!(err.to_string().contains("pipe"), "{err}");
        writer.join().unwrap().unwrap();
    }

    #[test]
    fn a_failed_accept_does_not_stop_serving() {
        let dir = Dir::new();
        let (player, events) = Player::spawn_with(PacedSink::default());
        let daemon =
            Daemon::start(player, events, None, dir.path("s.json"), Box::new(|| {})).unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        daemon.accept([
            Err(std::io::Error::other("too many open files")),
            Ok(server),
        ]);
        client
            .write_all(b"{\"id\":7,\"cmd\":\"status\"}\n")
            .unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        assert!(line.contains("\"id\":7"), "{line}");
    }

    fn reply_line(daemon: &Daemon, request: &str) -> String {
        let (server, mut client) = UnixStream::pair().unwrap();
        client.write_all(request.as_bytes()).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        serve(&daemon.state, server).unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        line
    }

    #[test]
    fn update_while_scanning_is_scan_running() {
        let (player, events) = Player::spawn_with(PacedSink::default());
        let dir = Dir::new();
        let daemon = Daemon::start(
            player,
            events,
            Some(dir.path("music")),
            dir.path("s.json"),
            Box::new(|| {}),
        )
        .unwrap();
        daemon.state.scanning.store(true, Ordering::SeqCst);
        let line = reply_line(&daemon, "{\"id\":3,\"cmd\":\"update\"}\n");
        assert!(line.contains("\"id\":3"), "{line}");
        assert!(line.contains("\"code\":\"scan_running\""), "{line}");
        assert!(line.contains("a scan is already running"), "{line}");
        assert_eq!(
            daemon.rescan().unwrap_err().to_string(),
            "a scan is already running"
        );
    }

    #[test]
    fn player_death_is_reported() {
        let dir = Dir::new();
        let fixture = flac(&dir, "a.flac", 44_100, 16, 2, 4096);
        let mut sink = PacedSink::default();
        sink.die = true;
        let (player, events) = Player::spawn_with(sink);
        let (tx, rx) = std::sync::mpsc::channel();
        let gone = Box::new(move || tx.send(()).unwrap());
        let daemon = Daemon::start(player, events, None, dir.path("s.json"), gone).unwrap();
        let mut tracks = Vec::new();
        collect(&fixture.path, &mut tracks).unwrap();
        daemon
            .state
            .player
            .send(PlayerCmd::Add {
                paths: tracks,
                at: None,
            })
            .unwrap();
        daemon.state.player.send(PlayerCmd::Play(None)).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    }

    fn restored(dir: &Dir, text: &str) -> lossless_engine::Snapshot {
        let state = dir.path("state.json");
        std::fs::write(&state, text).unwrap();
        let (player, events) = Player::spawn_with(PacedSink::default());
        let daemon = Daemon::start(player, events, None, state, Box::new(|| {})).unwrap();
        daemon.state.player.snapshot()
    }

    #[test]
    fn restore_drops_gone_paths_and_survives_corrupt_state() {
        let dir = Dir::new();
        let a = flac(&dir, "a.flac", 44_100, 16, 2, 4 * 4096).path;
        let text = format!(
            r#"{{"queue":["/gone.flac","{}"],"index":1,"position_ms":20,"repeat":"one","random":false}}"#,
            a.display()
        );
        let snap = restored(&dir, &text);
        assert_eq!(&*snap.queue, [a]);
        assert_eq!(
            (
                snap.status.state,
                snap.status.index,
                snap.status.position_ms
            ),
            (PlayState::Paused, Some(0), 20)
        );
        assert_eq!(snap.status.repeat, Repeat::One);
        for text in [
            r#"{"queue":["/gone.flac"],"index":0,"position_ms":0,"repeat":"off","random":false}"#,
            r#"{"queue":"#,
        ] {
            let snap = restored(&dir, text);
            assert!(snap.queue.is_empty());
            assert_eq!(snap.status.state, PlayState::Stopped);
        }
    }

    #[test]
    fn non_utf8_paths_are_saved_lossily() {
        use std::os::unix::ffi::OsStrExt;
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/m/\xff.flac"));
        let (player, _events) = Player::spawn_with(PacedSink::default());
        let mut snap = player.snapshot();
        snap.queue = vec![odd, PathBuf::from("/m/a.flac")].into();
        snap.status.index = Some(1);
        let s = saved(&snap);
        assert_eq!(s.queue[1], PathBuf::from("/m/a.flac"));
        assert!(s.queue[0].to_str().is_some());
        assert_eq!(s.index, Some(1));
    }

    /// Saves made while fanning out the events `cmd` produced.
    fn saves_for(cmd: PlayerCmd) -> usize {
        let dir = Dir::new();
        let a = flac(&dir, "a.flac", 44_100, 16, 2, 4 * 4096).path;
        let (player, events) = Player::spawn_with(PacedSink::default());
        player
            .send(PlayerCmd::Add {
                paths: vec![a.clone()],
                at: None,
            })
            .unwrap();
        while events.try_recv().is_ok() {}
        player.send(cmd).unwrap();
        drop(player);
        let mut saves = 0;
        let subs = Arc::new(Mutex::new(Vec::new()));
        super::fan_out(events, || saves += 1, subs, Box::new(|| {}));
        saves
    }

    #[test]
    fn one_save_per_batch_of_events() {
        assert_eq!(saves_for(PlayerCmd::Clear), 1);
        let dir = Dir::new();
        let a = flac(&dir, "a.flac", 44_100, 16, 2, 4 * 4096).path;
        let restore = PlayerCmd::Restore {
            queue: vec![a],
            index: Some(0),
            position_ms: 20,
            repeat: Repeat::Off,
            random: false,
        };
        assert_eq!(saves_for(restore), 1);
        assert_eq!(saves_for(PlayerCmd::VolumeChanged(5)), 0);
    }

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
