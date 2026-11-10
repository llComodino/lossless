use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lossless_proto::{Event, PlayState, Status};

use crate::output::Output;
use crate::source::Source;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Add(Vec<PathBuf>),
    Clear,
    /// Start or resume; with an index, jump to that queue entry.
    Play(Option<usize>),
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek {
        ms: u64,
    },
    Shutdown,
}

/// What the player looks like right now.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: Status,
    pub queue: Arc<[PathBuf]>,
}

/// Written by the player thread only when something happens. The position is
/// not stored but derived on read from `anchor`: no periodic updates.
struct Shared {
    status: Status,
    queue: Arc<[PathBuf]>,
    /// While playing: when `status.position_ms` was (or will be) heard.
    anchor: Option<Instant>,
}

/// Handle to the playback thread. Dropping it shuts the thread down.
pub struct Player {
    tx: Sender<Command>,
    shared: Arc<Mutex<Shared>>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Start the playback thread on `device`. Events arrive on the returned
    /// receiver.
    pub fn spawn(device: impl Into<String>) -> (Self, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            status: Status {
                state: PlayState::Stopped,
                index: None,
                position_ms: 0,
                duration_ms: None,
                output: None,
                track: None,
            },
            queue: Arc::from(Vec::new()),
            anchor: None,
        }));
        let worker = Worker {
            output: Output::new(device),
            queue: Vec::new(),
            index: None,
            state: PlayState::Stopped,
            source: None,
            written: 0,
            anchor_ms: 0,
            anchor_at: Instant::now(),
            rx,
            events: events_tx,
            shared: Arc::clone(&shared),
            dirty: false,
            queue_dirty: false,
        };
        let thread = thread::Builder::new()
            .name("player".into())
            .spawn(move || worker.run())
            .expect("spawning player thread");
        (
            Self {
                tx,
                shared,
                thread: Some(thread),
            },
            events_rx,
        )
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    pub fn snapshot(&self) -> Snapshot {
        let shared = lock(&self.shared);
        let mut status = shared.status.clone();
        if let Some(at) = shared.anchor {
            let elapsed = Instant::now().saturating_duration_since(at).as_millis() as u64;
            status.position_ms += elapsed;
            if let Some(d) = status.duration_ms {
                status.position_ms = status.position_ms.min(d);
            }
        }
        Snapshot {
            status,
            queue: Arc::clone(&shared.queue),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Worker {
    output: Output,
    queue: Vec<PathBuf>,
    index: Option<usize>,
    state: PlayState,
    source: Option<Source>,
    /// Frames of the current track handed to the output.
    written: u64,
    /// Position heard at `anchor_at` (in the future right after a gapless
    /// track change, while the previous track's tail is still buffered).
    anchor_ms: u64,
    anchor_at: Instant,
    rx: Receiver<Command>,
    events: Sender<Event>,
    shared: Arc<Mutex<Shared>>,
    dirty: bool,
    queue_dirty: bool,
}

impl Worker {
    fn run(mut self) {
        loop {
            // Idle: block until a command arrives. Playing: take whatever is
            // queued without waiting; the device paces the loop because
            // writes block until it has room.
            let cmd = if self.state == PlayState::Playing {
                match self.rx.try_recv() {
                    Ok(cmd) => Some(cmd),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Command::Shutdown),
                }
            } else {
                Some(self.rx.recv().unwrap_or(Command::Shutdown))
            };
            if let Some(cmd) = cmd {
                if cmd == Command::Shutdown {
                    self.output.close();
                    return;
                }
                self.handle(cmd);
            }
            if self.state == PlayState::Playing {
                self.pump();
            }
            if self.dirty {
                self.publish();
            }
        }
    }

    fn emit(&mut self, event: Event) {
        self.dirty = true;
        let _ = self.events.send(event);
    }

    fn rate(&self) -> u64 {
        self.source.as_ref().map_or(1, |s| u64::from(s.format.rate))
    }

    fn frames_to_ms(&self, frames: u64) -> u64 {
        frames * 1000 / self.rate()
    }

    /// Frames of the current track actually heard.
    fn heard(&self) -> u64 {
        self.written.saturating_sub(self.output.delay())
    }

    /// Record that `ms` is heard at `at`, and tell clients.
    fn anchor(&mut self, ms: u64, at: Instant) {
        self.anchor_ms = ms;
        self.anchor_at = at;
        self.emit(Event::State {
            state: self.state,
            position_ms: ms,
        });
    }

    fn set_state(&mut self, state: PlayState) {
        if self.state != state {
            self.state = state;
            let at = Instant::now();
            self.anchor(self.anchor_ms, at);
        }
    }

    fn publish(&mut self) {
        self.dirty = false;
        let duration_ms = self
            .source
            .as_ref()
            .and_then(|s| s.total_frames.map(|f| f * 1000 / u64::from(s.format.rate)));
        let mut shared = lock(&self.shared);
        shared.status = Status {
            state: self.state,
            index: self.index,
            position_ms: self.anchor_ms,
            duration_ms,
            output: self.output.format(),
            track: None,
        };
        shared.anchor = (self.state == PlayState::Playing).then_some(self.anchor_at);
        if self.queue_dirty {
            self.queue_dirty = false;
            shared.queue = Arc::from(self.queue.as_slice());
        }
    }

    /// Decode and write one chunk; move on at the end of a track.
    fn pump(&mut self) {
        let Some(source) = self.source.as_mut() else {
            return self.advance();
        };
        let channels = u64::from(source.format.channels);
        match source.next_chunk() {
            Ok(Some(chunk)) => {
                let frames = chunk.len() as u64 / channels;
                if let Err(e) = self.output.write(chunk) {
                    eprintln!("output: {e:#}");
                    self.halt();
                    return;
                }
                self.written += frames;
            }
            Ok(None) => self.advance(),
            Err(e) => {
                eprintln!("decode: {e:#}");
                self.advance();
            }
        }
    }

    /// Continue with the next queue entry without discarding buffered audio,
    /// so same-format tracks join gaplessly.
    fn advance(&mut self) {
        let next = self.index.map_or(0, |i| i + 1);
        if next < self.queue.len() {
            // The new track starts once the old tail has played out.
            let tail = Duration::from_millis(self.frames_to_ms(self.output.delay()));
            self.load(next, Instant::now() + tail);
            if self.state == PlayState::Playing && self.source.is_some() {
                self.anchor(0, self.anchor_at);
            }
        } else {
            self.output.close();
            self.source = None;
            self.index = None;
            self.written = 0;
            self.state = PlayState::Stopped;
            self.anchor(0, Instant::now());
            self.emit(Event::TrackChanged { index: None });
        }
    }

    /// Open queue entry `index` (heard from `starts_at`) and make the output
    /// ready for it, skipping entries that can't be opened.
    fn load(&mut self, mut index: usize, starts_at: Instant) {
        while index < self.queue.len() {
            match Source::open(&self.queue[index]) {
                Ok(source) => match self.output.ensure(source.format) {
                    Ok(_) => {
                        self.source = Some(source);
                        self.index = Some(index);
                        self.written = 0;
                        self.anchor_ms = 0;
                        self.anchor_at = starts_at;
                        self.emit(Event::TrackChanged { index: Some(index) });
                        return;
                    }
                    Err(e) => {
                        eprintln!("output: {e:#}");
                        return self.halt();
                    }
                },
                Err(e) => {
                    eprintln!("skipping {}: {e:#}", self.queue[index].display());
                    index += 1;
                }
            }
        }
        self.index = self.queue.len().checked_sub(1);
        self.advance();
    }

    /// Stop after an output failure; the device is released.
    fn halt(&mut self) {
        self.output.stop();
        self.source = None;
        self.written = 0;
        self.state = PlayState::Stopped;
        self.anchor(0, Instant::now());
    }

    /// Jump to `index` right away, dropping whatever is buffered.
    fn jump(&mut self, index: usize, play: bool) {
        let _ = self.output.discard();
        self.load(index, Instant::now());
        if self.source.is_some() {
            self.state = if play {
                PlayState::Playing
            } else {
                PlayState::Paused
            };
            self.anchor(0, Instant::now());
        }
    }

    /// Re-seek the source to what was actually heard, then drop the buffer:
    /// the device stays open, so there is no clock switch.
    fn rewind_to_heard(&mut self) {
        let heard = self.heard();
        if let Some(source) = self.source.as_mut() {
            match source.seek(heard) {
                Ok(at) => self.written = at,
                Err(e) => eprintln!("seek: {e:#}"),
            }
        }
        let _ = self.output.discard();
        self.anchor_ms = self.frames_to_ms(self.written);
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Add(paths) => {
                self.queue.extend(paths);
                self.queue_dirty = true;
                self.emit(Event::QueueChanged);
            }
            Command::Clear => {
                self.output.stop();
                self.queue.clear();
                self.queue_dirty = true;
                self.source = None;
                self.index = None;
                self.written = 0;
                self.state = PlayState::Stopped;
                self.anchor(0, Instant::now());
                self.emit(Event::QueueChanged);
                self.emit(Event::TrackChanged { index: None });
            }
            Command::Play(Some(i)) if i < self.queue.len() => self.jump(i, true),
            Command::Play(Some(_)) => {}
            Command::Play(None) => match self.state {
                PlayState::Playing => {}
                PlayState::Paused => self.set_state(PlayState::Playing),
                PlayState::Stopped if !self.queue.is_empty() => {
                    self.jump(self.index.unwrap_or(0), true)
                }
                PlayState::Stopped => {}
            },
            Command::Pause => {
                if self.state == PlayState::Playing {
                    self.rewind_to_heard();
                    self.set_state(PlayState::Paused);
                }
            }
            Command::Toggle => {
                let next = if self.state == PlayState::Playing {
                    Command::Pause
                } else {
                    Command::Play(None)
                };
                self.handle(next);
            }
            Command::Stop => {
                self.output.stop();
                self.source = None;
                self.written = 0;
                self.state = PlayState::Stopped;
                self.anchor(0, Instant::now());
            }
            Command::Next | Command::Previous => {
                let Some(i) = self.index else { return };
                let target = if cmd == Command::Next {
                    i + 1
                } else {
                    i.saturating_sub(1)
                };
                if target >= self.queue.len() {
                    return;
                }
                if self.state == PlayState::Stopped {
                    self.index = Some(target);
                    self.emit(Event::TrackChanged {
                        index: Some(target),
                    });
                } else {
                    let play = self.state == PlayState::Playing;
                    self.jump(target, play);
                }
            }
            Command::Seek { ms } => {
                let Some(source) = self.source.as_mut() else {
                    return;
                };
                let frame = ms.saturating_mul(u64::from(source.format.rate)) / 1000;
                if source.total_frames.is_some_and(|total| frame >= total) {
                    let _ = self.output.discard();
                    return self.advance();
                }
                match source.seek(frame) {
                    Ok(at) => {
                        let _ = self.output.discard();
                        self.written = at;
                        let ms = self.frames_to_ms(at);
                        self.anchor(ms, Instant::now());
                    }
                    Err(e) => eprintln!("seek: {e:#}"),
                }
            }
            Command::Shutdown => {}
        }
    }
}
