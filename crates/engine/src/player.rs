use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
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

/// What the player looks like right now; refreshed by the player thread.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: Status,
    pub queue: Vec<PathBuf>,
}

/// Handle to the playback thread. Dropping it shuts the thread down.
pub struct Player {
    tx: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Start the playback thread on `device`. Events arrive on the returned
    /// receiver.
    pub fn spawn(device: impl Into<String>) -> (Self, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot {
            status: Status {
                state: PlayState::Stopped,
                index: None,
                position_ms: 0,
                duration_ms: None,
                output: None,
                track: None,
            },
            queue: Vec::new(),
        }));
        let worker = Worker {
            output: Output::new(device),
            queue: Vec::new(),
            index: None,
            state: PlayState::Stopped,
            source: None,
            written: 0,
            rx,
            events: events_tx,
            snapshot: Arc::clone(&snapshot),
            last_position: Instant::now(),
        };
        let thread = thread::Builder::new()
            .name("player".into())
            .spawn(move || worker.run())
            .expect("spawning player thread");
        (
            Self {
                tx,
                snapshot,
                thread: Some(thread),
            },
            events_rx,
        )
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    pub fn snapshot(&self) -> Snapshot {
        lock(&self.snapshot).clone()
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

fn lock(m: &Mutex<Snapshot>) -> MutexGuard<'_, Snapshot> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

const POSITION_EVERY: Duration = Duration::from_secs(1);

struct Worker {
    output: Output,
    queue: Vec<PathBuf>,
    index: Option<usize>,
    state: PlayState,
    source: Option<Source>,
    /// Frames of the current track handed to the output.
    written: u64,
    rx: Receiver<Command>,
    events: Sender<Event>,
    snapshot: Arc<Mutex<Snapshot>>,
    last_position: Instant,
}

impl Worker {
    fn run(mut self) {
        loop {
            let cmd = if self.state == PlayState::Playing {
                match self.rx.try_recv() {
                    Ok(cmd) => Some(cmd),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Command::Shutdown),
                }
            } else {
                match self.rx.recv_timeout(POSITION_EVERY) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => Some(Command::Shutdown),
                }
            };
            if let Some(cmd) = cmd {
                if cmd == Command::Shutdown {
                    self.output.close();
                    return;
                }
                self.handle(cmd);
                self.publish();
            }
            if self.state == PlayState::Playing {
                self.pump();
                if self.last_position.elapsed() >= POSITION_EVERY {
                    self.last_position = Instant::now();
                    self.publish();
                    self.emit(Event::Position {
                        ms: self.position_ms(),
                    });
                }
            }
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn set_state(&mut self, state: PlayState) {
        if self.state != state {
            self.state = state;
            self.emit(Event::State { state });
        }
    }

    fn rate(&self) -> u64 {
        self.source.as_ref().map_or(1, |s| u64::from(s.format.rate))
    }

    /// Frames of the current track actually heard. Around a track change the
    /// output delay still holds the previous track's tail, so this is
    /// approximate for up to one buffer (~0.5 s).
    fn heard(&self) -> u64 {
        self.written.saturating_sub(self.output.delay())
    }

    fn position_ms(&self) -> u64 {
        self.heard() * 1000 / self.rate()
    }

    fn publish(&self) {
        let duration_ms = self
            .source
            .as_ref()
            .and_then(|s| s.total_frames.map(|f| f * 1000 / u64::from(s.format.rate)));
        let mut snap = lock(&self.snapshot);
        snap.status = Status {
            state: self.state,
            index: self.index,
            position_ms: self.position_ms(),
            duration_ms,
            output: self.output.format(),
            track: None,
        };
        snap.queue.clone_from(&self.queue);
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
            self.load(next);
        } else {
            self.output.close();
            self.source = None;
            self.index = None;
            self.written = 0;
            self.set_state(PlayState::Stopped);
            self.emit(Event::TrackChanged { index: None });
        }
    }

    /// Open queue entry `index` and make the output ready for it, skipping
    /// entries that can't be opened.
    fn load(&mut self, mut index: usize) {
        while index < self.queue.len() {
            match Source::open(&self.queue[index]) {
                Ok(source) => match self.output.ensure(source.format) {
                    Ok(_) => {
                        self.source = Some(source);
                        self.index = Some(index);
                        self.written = 0;
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
        self.advance_past_end();
    }

    fn advance_past_end(&mut self) {
        self.index = self.queue.len().checked_sub(1);
        self.advance();
    }

    /// Stop after an output failure; the device is released.
    fn halt(&mut self) {
        self.output.stop();
        self.source = None;
        self.written = 0;
        self.set_state(PlayState::Stopped);
    }

    /// Jump to `index` right away, dropping whatever is buffered.
    fn jump(&mut self, index: usize, play: bool) {
        let _ = self.output.discard();
        self.load(index);
        if self.source.is_some() {
            self.set_state(if play {
                PlayState::Playing
            } else {
                PlayState::Paused
            });
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
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Add(paths) => {
                self.queue.extend(paths);
                self.emit(Event::QueueChanged);
            }
            Command::Clear => {
                self.output.stop();
                self.queue.clear();
                self.source = None;
                self.index = None;
                self.written = 0;
                self.set_state(PlayState::Stopped);
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
                self.set_state(PlayState::Stopped);
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
                let play = self.state == PlayState::Playing;
                if self.state == PlayState::Stopped {
                    self.index = Some(target);
                    self.emit(Event::TrackChanged {
                        index: Some(target),
                    });
                } else {
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
                    }
                    Err(e) => eprintln!("seek: {e:#}"),
                }
            }
            Command::Shutdown => {}
        }
    }
}
