use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lossless_proto::{Event, PlayState, Repeat, Status, WireError};

use crate::mixer::{self, Mixer};
use crate::output::{Output, Sink};
use crate::queue::{Queue, Removal};
use crate::source::Source;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Insert before `at`; `None` appends.
    Add {
        paths: Vec<PathBuf>,
        at: Option<usize>,
    },
    /// Swap the queue and play its first entry.
    Replace(Vec<PathBuf>),
    Remove(Vec<usize>),
    Move {
        from: usize,
        to: usize,
    },
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
    /// `None` leaves that mode as it is.
    SetMode {
        repeat: Option<Repeat>,
        random: Option<bool>,
    },
    /// Percent, 0 to 100.
    SetVolume(u8),
    /// The level the mixer watcher read after a change on the card.
    VolumeChanged(u8),
    /// Put back a saved session: paused at `position_ms` of entry `index`,
    /// with the device left closed until play.
    Restore {
        queue: Vec<PathBuf>,
        index: Option<usize>,
        position_ms: u64,
        repeat: Repeat,
        random: bool,
    },
    Shutdown,
}

/// A command and, for `Player::send`, who waits for it to take effect.
struct Envelope {
    cmd: Command,
    done: Option<mpsc::SyncSender<Result<()>>>,
}

impl Envelope {
    fn shutdown() -> Self {
        Self {
            cmd: Command::Shutdown,
            done: None,
        }
    }
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
    tx: Sender<Envelope>,
    shared: Arc<Mutex<Shared>>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Start the playback thread on ALSA `device`. Events arrive on the
    /// returned receiver.
    pub fn spawn(device: impl Into<String>) -> (Self, Receiver<Event>) {
        Self::spawn_with(Output::new(device))
    }

    /// Start the playback thread on any sink, without a mixer.
    pub fn spawn_with<S: Sink + Send + 'static>(sink: S) -> (Self, Receiver<Event>) {
        Self::spawn_with_mixer(sink, None)
    }

    /// Start the playback thread on any sink. A mixer exposing a software
    /// `gain` has it applied to every chunk; others never touch samples.
    pub fn spawn_with_mixer<S: Sink + Send + 'static>(
        sink: S,
        mixer: Option<Box<dyn Mixer>>,
    ) -> (Self, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let (worker, events_rx) = Worker::new(sink, tx.clone(), rx, mixer);
        let shared = Arc::clone(&worker.shared);
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

    /// Run `cmd` and return once it has taken effect and the resulting
    /// status is published, so a `snapshot` right after sees it. While
    /// playing, commands are taken between decoded chunks: the wait is at
    /// most about one chunk. Fails with what the command itself got wrong
    /// (e.g. an index past the queue); playback problems arrive as events.
    pub fn send(&self, cmd: Command) -> Result<()> {
        let (done, wait) = mpsc::sync_channel(1);
        let env = Envelope {
            cmd,
            done: Some(done),
        };
        match self.tx.send(env) {
            Ok(()) => wait.recv().unwrap_or(Ok(())),
            Err(_) => Ok(()),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared).at(Instant::now())
    }
}

impl Shared {
    fn new() -> Self {
        Self {
            status: Status {
                state: PlayState::Stopped,
                index: None,
                position_ms: 0,
                duration_ms: None,
                output: None,
                track: None,
                last_error: None,
                repeat: Repeat::Off,
                random: false,
                volume: None,
            },
            queue: Arc::from(Vec::new()),
            anchor: None,
        }
    }

    /// The snapshot as seen at `now`: while playing, the position advances
    /// from the anchor (never before it, never past the duration).
    fn at(&self, now: Instant) -> Snapshot {
        let mut status = self.status.clone();
        if let Some(at) = self.anchor {
            let elapsed = now.saturating_duration_since(at).as_millis() as u64;
            status.position_ms = status.position_ms.saturating_add(elapsed);
            if let Some(d) = status.duration_ms {
                status.position_ms = status.position_ms.min(d);
            }
        }
        Snapshot {
            status,
            queue: Arc::clone(&self.queue),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Envelope::shutdown());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Worker<S: Sink> {
    output: S,
    queue: Queue,
    state: PlayState,
    source: Option<Source>,
    /// Frames of the current track handed to the output.
    written: u64,
    /// Position heard at `anchor_at` (in the future right after a gapless
    /// track change, while the previous track's tail is still buffered).
    anchor_ms: u64,
    anchor_at: Instant,
    /// For the mixer watcher's reports.
    tx: Sender<Envelope>,
    rx: Receiver<Envelope>,
    events: Sender<Event>,
    /// Events of the current step, sent only after the snapshot is
    /// published, so a client reacting to one never reads stale status.
    pending: Vec<Event>,
    /// The last recovery failure, until a track loads cleanly.
    last_error: Option<WireError>,
    /// Set while recovering from a failure, so the load that follows keeps it.
    keep_error: bool,
    shared: Arc<Mutex<Shared>>,
    dirty: bool,
    queue_dirty: bool,
    mixer: Option<Box<dyn Mixer>>,
    gain: Option<Arc<AtomicU8>>,
    /// The last level published.
    volume: Option<u8>,
    watcher: Option<JoinHandle<()>>,
    /// The card went away (a watcher ended) or was missing at start: reopen
    /// the mixer before use.
    stale: bool,
    scratch: Vec<i32>,
}

impl<S: Sink> Worker<S> {
    fn new(
        output: S,
        tx: Sender<Envelope>,
        rx: Receiver<Envelope>,
        mixer: Option<Box<dyn Mixer>>,
    ) -> (Self, Receiver<Event>) {
        let (events, events_rx) = mpsc::channel();
        let volume = mixer.as_ref().and_then(|m| m.get().ok());
        let mixer_closed = mixer.is_some() && volume.is_none();
        let mut shared = Shared::new();
        shared.status.volume = volume;
        let worker = Self {
            output,
            queue: Queue::default(),
            state: PlayState::Stopped,
            source: None,
            written: 0,
            anchor_ms: 0,
            anchor_at: Instant::now(),
            tx,
            rx,
            events,
            pending: Vec::new(),
            last_error: None,
            keep_error: false,
            shared: Arc::new(Mutex::new(shared)),
            dirty: false,
            queue_dirty: false,
            gain: mixer.as_ref().and_then(|m| m.gain()),
            mixer,
            volume,
            watcher: None,
            stale: mixer_closed,
            scratch: Vec::new(),
        };
        (worker, events_rx)
    }

    fn run(mut self) {
        self.arm();
        loop {
            // Idle: block until a command arrives. Playing: take whatever is
            // queued without waiting; the device paces the loop because
            // writes block until it has room.
            let msg = if self.state == PlayState::Playing {
                match self.rx.try_recv() {
                    Ok(msg) => Some(msg),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Envelope::shutdown()),
                }
            } else {
                Some(self.rx.recv().unwrap_or_else(|_| Envelope::shutdown()))
            };
            if let Some(Envelope { cmd, done }) = msg {
                if cmd == Command::Shutdown {
                    self.output.close();
                    return;
                }
                let result = self.handle(cmd);
                if self.dirty {
                    self.publish();
                }
                if let Some(done) = done {
                    let _ = done.send(result);
                }
            }
            self.step();
        }
    }

    /// One unit of playback work after a command: decode and write a chunk
    /// while playing, and publish if anything changed.
    fn step(&mut self) {
        if self.state == PlayState::Playing {
            self.pump();
        }
        if self.dirty {
            self.publish();
        }
    }

    /// Start the mixer watcher unless one is running. A watcher ends when
    /// its card goes away; the next successful `ensure` comes back here.
    fn arm(&mut self) {
        if self.watcher.as_ref().is_some_and(JoinHandle::is_finished) {
            self.watcher = None;
            self.stale = true;
        }
        if self.watcher.is_some() {
            return;
        }
        let Some(m) = self.mixer.as_mut() else { return };
        if self.stale {
            if let Err(e) = m.reopen() {
                return log::warn!("mixer: {e}");
            }
            self.stale = false;
            if let Ok(p) = m.get() {
                self.volume_seen(p);
            }
        }
        let Some(m) = &self.mixer else { return };
        let tx = self.tx.clone();
        let report = Box::new(move |p| {
            tx.send(Envelope {
                cmd: Command::VolumeChanged(p),
                done: None,
            })
            .is_ok()
        });
        match m.watch(report) {
            Ok(w) => self.watcher = w,
            Err(e) => log::warn!("mixer: {e}"),
        }
    }

    /// A level read from the mixer: published only if it changed, so the
    /// watcher's echo of our own `SetVolume` is not reported twice.
    fn volume_seen(&mut self, percent: u8) {
        if self.volume != Some(percent) {
            self.volume = Some(percent);
            self.emit(Event::Volume { percent });
        }
    }

    fn emit(&mut self, event: Event) {
        log::debug!("player: {event:?}");
        self.dirty = true;
        self.pending.push(event);
    }

    /// Tell clients about a failure the player recovered from on its own.
    fn fail(&mut self, err: &Error) {
        log::warn!("{err}");
        let wire = err.to_wire();
        self.last_error = Some(wire.clone());
        self.emit(Event::Error(wire));
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
            index: self.queue.current(),
            position_ms: self.anchor_ms,
            duration_ms,
            output: self.output.format(),
            track: None,
            last_error: self.last_error.clone(),
            repeat: self.queue.repeat(),
            random: self.queue.random(),
            volume: self.volume,
        };
        shared.anchor = (self.state == PlayState::Playing).then_some(self.anchor_at);
        if self.queue_dirty {
            self.queue_dirty = false;
            shared.queue = Arc::from(self.queue.entries());
        }
        drop(shared);
        for event in self.pending.drain(..) {
            let _ = self.events.send(event);
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
                let gain = self
                    .gain
                    .as_ref()
                    .map_or(100, |g| g.load(Ordering::Relaxed));
                let written = if gain >= 100 {
                    self.output.write(chunk)
                } else {
                    self.scratch.clear();
                    self.scratch.extend_from_slice(chunk);
                    mixer::apply(&mut self.scratch, gain);
                    self.output.write(&self.scratch)
                };
                if let Err(e) = written {
                    self.fail(&e);
                    self.halt();
                    return;
                }
                self.written += frames;
            }
            Ok(None) => self.advance(),
            Err(e) => {
                self.fail(&e);
                self.keep_error = true;
                self.advance();
                self.keep_error = false;
            }
        }
    }

    /// Continue with the next queue entry for the mode without discarding
    /// buffered audio, so same-format tracks join gaplessly. A track that
    /// failed is skipped even under repeat one.
    fn advance(&mut self) {
        let next = if self.keep_error {
            self.queue.skip()
        } else {
            self.queue.next()
        };
        match next {
            Some(next) => {
                // The new track starts once the old tail has played out.
                let tail = Duration::from_millis(self.frames_to_ms(self.output.delay()));
                self.load(next, Instant::now() + tail);
                if self.state == PlayState::Playing && self.source.is_some() {
                    self.anchor(0, self.anchor_at);
                }
            }
            None => self.finish(),
        }
    }

    /// The queue ran out: release the device and stop.
    fn finish(&mut self) {
        self.output.close();
        self.source = None;
        self.queue.set_current(None);
        self.written = 0;
        self.state = PlayState::Stopped;
        self.anchor(0, Instant::now());
        self.emit(Event::TrackChanged { index: None });
    }

    /// Open queue entry `index` (heard from `starts_at`) and make the output
    /// ready for it, skipping entries that can't be opened. Each entry is
    /// tried once per load, so a queue of broken files ends even under repeat.
    fn load(&mut self, index: usize, starts_at: Instant) {
        let mut skipped = false;
        let mut tried = vec![false; self.queue.entries().len()];
        let mut left = tried.len();
        let mut next = Some(index);
        while let Some(index) = next.filter(|_| left > 0) {
            if !std::mem::replace(&mut tried[index], true) {
                left -= 1;
                match Source::open(&self.queue.entries()[index]) {
                    Ok(source) => match self.output.ensure(source.format) {
                        Ok(_) => {
                            if !skipped && !self.keep_error {
                                self.last_error = None;
                                self.dirty = true;
                            }
                            self.source = Some(source);
                            self.queue.set_current(Some(index));
                            self.written = 0;
                            self.anchor_ms = 0;
                            self.anchor_at = starts_at;
                            self.emit(Event::TrackChanged { index: Some(index) });
                            self.arm();
                            return;
                        }
                        Err(e) => {
                            self.fail(&e);
                            return self.halt();
                        }
                    },
                    Err(e) => {
                        self.fail(&e);
                        skipped = true;
                    }
                }
            }
            self.queue.set_current(Some(index));
            next = self.queue.skip();
        }
        self.finish();
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

    /// Open entry `index` at `ms` without touching the device. A failure
    /// leaves it stopped on that entry, like a failed load.
    fn restore(&mut self, index: Option<usize>, ms: u64) {
        let index = index.filter(|&i| i < self.queue.entries().len());
        self.queue.set_current(index);
        self.emit(Event::TrackChanged { index });
        let Some(i) = index else {
            return self.anchor(0, Instant::now());
        };
        let opened = Source::open(&self.queue.entries()[i]).and_then(|mut s| {
            match ms.saturating_mul(u64::from(s.format.rate)) / 1000 {
                0 => Ok((s, 0)),
                frame => s.seek(frame).map(|at| (s, at)),
            }
        });
        match opened {
            Ok((source, at)) => {
                self.source = Some(source);
                self.written = at;
                self.state = PlayState::Paused;
                self.anchor(self.frames_to_ms(at), Instant::now());
            }
            Err(e) => {
                self.fail(&e);
                self.anchor(0, Instant::now());
            }
        }
    }

    /// Paused to playing; the device may still be closed after a restore.
    fn resume(&mut self) {
        let format = self.source.as_ref().map(|s| s.format);
        if let Some(Err(e)) = format.map(|f| self.output.ensure(f)) {
            self.fail(&e);
            return self.halt();
        }
        self.arm();
        self.set_state(PlayState::Playing);
    }

    /// Re-seek the source to what was actually heard, then drop the buffer:
    /// the device stays open, so there is no clock switch.
    fn rewind_to_heard(&mut self) {
        let heard = self.heard();
        if let Some(source) = self.source.as_mut() {
            match source.seek(heard) {
                Ok(at) => self.written = at,
                Err(e) => self.fail(&e),
            }
        }
        let _ = self.output.discard();
        self.anchor_ms = self.frames_to_ms(self.written);
    }

    /// Publish the edited queue; tell clients if the playing entry's index
    /// differs from `was`.
    fn queue_edited(&mut self, was: Option<usize>) {
        self.queue_dirty = true;
        self.emit(Event::QueueChanged);
        let now = self.queue.current();
        if now != was {
            self.emit(Event::TrackChanged { index: now });
        }
    }

    /// After an edit that moved the cursor: if nothing reported where it
    /// ended up (a jump that failed to open the device), say so now.
    fn track_settled(&mut self, was: Option<usize>) {
        let told = self
            .pending
            .iter()
            .any(|e| matches!(e, Event::TrackChanged { .. }));
        let now = self.queue.current();
        if !told && now != was {
            self.emit(Event::TrackChanged { index: now });
        }
    }

    fn handle(&mut self, cmd: Command) -> Result<()> {
        log::debug!("player: {cmd:?}");
        match cmd {
            Command::Add { paths, at } => {
                let was = self.queue.current();
                self.queue.insert(at, paths)?;
                self.queue_edited(was);
            }
            Command::Replace(paths) if paths.is_empty() => return self.handle(Command::Clear),
            Command::Replace(paths) => {
                let was = self.queue.current();
                self.queue.replace(paths);
                self.queue_edited(None);
                self.jump(0, true);
                self.track_settled(was);
            }
            Command::Remove(indices) => {
                let was = self.queue.current();
                match self.queue.remove(&indices)? {
                    Removal::Kept => self.queue_edited(was),
                    Removal::CurrentRemoved { mut next } => {
                        self.queue_edited(None);
                        if self.queue.random() {
                            next = self.queue.next();
                        }
                        match next {
                            Some(n) if self.state != PlayState::Stopped => {
                                let play = self.state == PlayState::Playing;
                                self.jump(n, play);
                            }
                            next => {
                                if self.state != PlayState::Stopped {
                                    self.halt();
                                }
                                self.queue.set_current(next);
                                self.emit(Event::TrackChanged { index: next });
                            }
                        }
                        self.track_settled(was);
                    }
                }
            }
            Command::Move { from, to } => {
                let was = self.queue.current();
                self.queue.move_entry(from, to)?;
                self.queue_edited(was);
            }
            Command::Clear => {
                self.halt();
                self.queue.replace(Vec::new());
                self.queue_dirty = true;
                self.emit(Event::QueueChanged);
                self.emit(Event::TrackChanged { index: None });
            }
            Command::Play(Some(i)) if i < self.queue.entries().len() => {
                if self.queue.cycle_done() {
                    self.queue.restart_cycle();
                }
                self.jump(i, true)
            }
            Command::Play(Some(index)) => {
                return Err(Error::OutOfRange {
                    index,
                    len: self.queue.entries().len(),
                });
            }
            Command::Play(None) => match self.state {
                PlayState::Playing => {}
                PlayState::Paused => self.resume(),
                PlayState::Stopped if !self.queue.entries().is_empty() => {
                    let first = self.queue.current().or_else(|| {
                        self.queue.restart_cycle();
                        self.queue.next()
                    });
                    self.jump(first.unwrap_or(0), true)
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
                return self.handle(next);
            }
            Command::Stop => self.halt(),
            Command::Next | Command::Previous => {
                let current = self.queue.current();
                let target = match (&cmd, current) {
                    (Command::Next, None) => None,
                    (Command::Next, Some(_)) => self.queue.skip(),
                    _ => self.queue.previous().or(current),
                };
                let Some(target) = target else {
                    return Ok(());
                };
                if self.state == PlayState::Stopped {
                    self.queue.set_current(Some(target));
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
                    return Ok(());
                };
                let frame = ms.saturating_mul(u64::from(source.format.rate)) / 1000;
                if source.total_frames.is_some_and(|total| frame >= total) {
                    let _ = self.output.discard();
                    self.advance();
                    return Ok(());
                }
                match source.seek(frame) {
                    Ok(at) => {
                        let _ = self.output.discard();
                        self.written = at;
                        let ms = self.frames_to_ms(at);
                        self.anchor(ms, Instant::now());
                    }
                    Err(e) => return Err(e),
                }
            }
            Command::SetMode { repeat, random } => {
                self.queue.set_mode(repeat, random);
                self.emit(Event::Mode {
                    repeat: self.queue.repeat(),
                    random: self.queue.random(),
                });
            }
            Command::SetVolume(percent) if percent > 100 => {
                return Err(Error::BadRequest(format!(
                    "volume {percent} out of range 0 to 100"
                )));
            }
            Command::SetVolume(percent) => {
                self.arm();
                if self.stale {
                    return Err(Error::NoMixer);
                }
                let m = self.mixer.as_mut().ok_or(Error::NoMixer)?;
                m.set(percent)?;
                let now = m.get().unwrap_or(percent);
                self.volume = Some(now);
                self.emit(Event::Volume { percent: now });
            }
            Command::VolumeChanged(percent) => self.volume_seen(percent),
            Command::Restore {
                queue,
                index,
                position_ms,
                repeat,
                random,
            } => {
                self.output.stop();
                self.source = None;
                self.written = 0;
                self.state = PlayState::Stopped;
                self.last_error = None;
                self.queue.replace(queue);
                self.queue.set_mode(Some(repeat), Some(random));
                self.queue_dirty = true;
                self.emit(Event::QueueChanged);
                self.emit(Event::Mode { repeat, random });
                self.restore(index, position_ms);
            }
            Command::Shutdown => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use lossless_proto::{ErrorCode, Event, PlayState, Repeat, StreamFormat};

    use super::{Command, Player, Shared, Worker, lock};
    use crate::fixtures::{BLOCK, Dir, Fixture, flac, write_bytes};
    use crate::mixer::Mixer;
    use crate::mixer::SoftwareMixer;
    use crate::output::Sink;
    use crate::queue::Queue;
    use crate::{Error, Result};

    /// Records what reaches the "speaker". The last `delay` frames written
    /// since the previous discard count as buffered: a discard or stop drops
    /// them unheard, like a real device.
    #[derive(Default)]
    struct FakeSink {
        open: Option<StreamFormat>,
        opens: Vec<StreamFormat>,
        heard: Vec<i32>,
        delay: u64,
        since_discard: u64,
        fail_ensure: bool,
        fail_write: bool,
        stops: usize,
    }

    impl FakeSink {
        fn drop_buffered(&mut self) {
            if let Some(f) = self.open {
                let frames = self.delay.min(self.since_discard) as usize;
                let n = self.heard.len() - frames * f.channels as usize;
                self.heard.truncate(n);
            }
            self.since_discard = 0;
        }
    }

    impl Sink for FakeSink {
        fn ensure(&mut self, src: StreamFormat) -> Result<StreamFormat> {
            if self.fail_ensure {
                return Err(Error::Device {
                    device: "fake".into(),
                    reason: "device gone".into(),
                });
            }
            if self.open != Some(src) {
                self.open = Some(src);
                self.opens.push(src);
            }
            Ok(src)
        }
        fn format(&self) -> Option<StreamFormat> {
            self.open
        }
        fn write(&mut self, samples: &[i32]) -> Result<()> {
            if self.fail_write {
                return Err(Error::Device {
                    device: "fake".into(),
                    reason: "write failed".into(),
                });
            }
            let ch = self.open.expect("write while closed").channels as usize;
            self.heard.extend_from_slice(samples);
            self.since_discard += (samples.len() / ch) as u64;
            Ok(())
        }
        fn delay(&self) -> u64 {
            self.delay.min(self.since_discard)
        }
        fn discard(&mut self) -> Result<()> {
            self.drop_buffered();
            Ok(())
        }
        fn close(&mut self) {
            self.since_discard = 0;
            self.open = None;
        }
        fn stop(&mut self) {
            self.drop_buffered();
            self.open = None;
            self.stops += 1;
        }
    }

    fn worker(sink: FakeSink) -> (Worker<FakeSink>, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        Worker::new(sink, tx, rx, None)
    }

    fn mixed_worker(sink: FakeSink, p: u8) -> Worker<FakeSink> {
        let (tx, rx) = mpsc::channel();
        let mut m = SoftwareMixer::new();
        m.set(p).unwrap();
        Worker::new(sink, tx, rx, Some(Box::new(m))).0
    }

    /// Run until playback stops (bounded so a bug can't hang the test).
    fn drive(w: &mut Worker<FakeSink>) {
        for _ in 0..100_000 {
            if w.state != PlayState::Playing {
                return;
            }
            w.step();
        }
        panic!("playback never stopped");
    }

    /// Run until the current queue entry is `index`.
    fn drive_to(w: &mut Worker<FakeSink>, index: usize) {
        for _ in 0..100_000 {
            if w.queue.current() == Some(index) {
                return;
            }
            w.step();
        }
        panic!("never reached entry {index}");
    }

    fn concat(fs: &[&Fixture]) -> Vec<i32> {
        fs.iter().flat_map(|f| f.expected.iter().copied()).collect()
    }

    fn fmt(rate: u32, bits: u16) -> StreamFormat {
        StreamFormat {
            rate,
            bits,
            channels: 2,
        }
    }

    fn states(events: &mpsc::Receiver<Event>) -> Vec<(PlayState, u64)> {
        events
            .try_iter()
            .filter_map(|e| match e {
                Event::State { state, position_ms } => Some((state, position_ms)),
                _ => None,
            })
            .collect()
    }

    fn play_one(w: &mut Worker<FakeSink>, f: &Fixture) {
        w.handle(Command::Add {
            paths: vec![f.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        drive(w);
    }

    #[test]
    fn software_gain_scales_and_full_gain_is_exact() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let mut w = mixed_worker(FakeSink::default(), 100);
        play_one(&mut w, &a);
        assert!(w.output.heard == concat(&[&a]), "100% must be untouched");
        let mut w = mixed_worker(FakeSink::default(), 0);
        play_one(&mut w, &a);
        assert!(w.output.heard.iter().all(|&s| s == 0));
        assert_eq!(w.output.heard.len(), concat(&[&a]).len());
    }

    type Watch = (Box<dyn Fn(u8) -> bool + Send>, Option<mpsc::Sender<()>>);

    /// A mixer whose watchers run until the test ends them.
    struct FakeMixer {
        level: u8,
        fail_set: bool,
        /// Not opened yet: reads fail until a reopen succeeds.
        closed: bool,
        /// Reopens that fail before the card is back.
        absent: usize,
        watches: Arc<Mutex<Vec<Watch>>>,
    }

    impl FakeMixer {
        fn new(level: u8) -> Self {
            Self {
                level,
                fail_set: false,
                closed: false,
                absent: 0,
                watches: Arc::default(),
            }
        }
    }

    impl Mixer for FakeMixer {
        fn get(&self) -> Result<u8> {
            if self.closed {
                return Err(Error::Device {
                    device: "hw:Fake".into(),
                    reason: "not open".into(),
                });
            }
            Ok(self.level)
        }
        fn set(&mut self, percent: u8) -> Result<()> {
            if self.fail_set || self.closed {
                return Err(Error::Device {
                    device: "hw:Fake".into(),
                    reason: "No such device".into(),
                });
            }
            self.level = percent;
            Ok(())
        }
        fn watch(
            &self,
            on_change: Box<dyn Fn(u8) -> bool + Send>,
        ) -> Result<Option<JoinHandle<()>>> {
            let (end, ended) = mpsc::channel::<()>();
            self.watches.lock().unwrap().push((on_change, Some(end)));
            Ok(Some(thread::spawn(move || {
                let _ = ended.recv();
            })))
        }
        /// The card is back, at its power-on level.
        fn reopen(&mut self) -> Result<()> {
            if self.absent > 0 {
                self.absent -= 1;
                return Err(Error::Device {
                    device: "hw:Fake".into(),
                    reason: "No such device".into(),
                });
            }
            self.fail_set = false;
            self.closed = false;
            self.level = 25;
            Ok(())
        }
    }

    fn faked(sink: FakeSink, m: FakeMixer) -> (Worker<FakeSink>, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        Worker::new(sink, tx, rx, Some(Box::new(m)))
    }

    fn volumes(events: &mpsc::Receiver<Event>) -> Vec<u8> {
        events
            .try_iter()
            .filter_map(|e| match e {
                Event::Volume { percent } => Some(percent),
                _ => None,
            })
            .collect()
    }

    fn volume(w: &Worker<FakeSink>) -> Option<u8> {
        lock(&w.shared).at(Instant::now()).status.volume
    }

    #[test]
    fn set_volume_emits_one_event() {
        let (mut w, events) = faked(FakeSink::default(), FakeMixer::new(100));
        assert_eq!(volume(&w), Some(100), "read from the mixer at start");
        w.handle(Command::SetVolume(40)).unwrap();
        w.publish();
        assert_eq!(volumes(&events), [40]);
        assert_eq!(volume(&w), Some(40));
        w.handle(Command::SetVolume(40)).unwrap();
        w.publish();
        assert_eq!(volumes(&events), [40], "every set reports");
        w.handle(Command::VolumeChanged(40)).unwrap();
        w.publish();
        assert!(
            volumes(&events).is_empty(),
            "the watcher's echo of our own set"
        );
        w.handle(Command::VolumeChanged(70)).unwrap();
        w.publish();
        assert_eq!(volumes(&events), [70]);
        assert_eq!(volume(&w), Some(70));
        let err = w.handle(Command::SetVolume(101)).unwrap_err();
        assert_eq!(err.code(), ErrorCode::BadRequest);
        assert_eq!(w.mixer.as_ref().unwrap().get().unwrap(), 40);
    }

    #[test]
    fn no_mixer_is_an_error() {
        let (mut w, events) = worker(FakeSink::default());
        let err = w.handle(Command::SetVolume(50)).unwrap_err();
        assert_eq!(err.code(), ErrorCode::NoMixer);
        w.publish();
        assert_eq!(volume(&w), None);
        assert!(volumes(&events).is_empty());
        w.arm();
        assert!(w.watcher.is_none());
    }

    #[test]
    fn a_mixer_missing_at_start_is_retried() {
        let mut m = FakeMixer::new(100);
        m.closed = true;
        m.absent = 2;
        let watches = Arc::clone(&m.watches);
        let (mut w, events) = faked(FakeSink::default(), m);
        assert_eq!(volume(&w), None, "unknown until opened");
        w.arm();
        assert!(watches.lock().unwrap().is_empty(), "the card is not there");
        let err = w.handle(Command::SetVolume(50)).unwrap_err();
        assert_eq!(err.code(), ErrorCode::NoMixer);
        w.publish();
        assert_eq!(volume(&w), None);
        w.handle(Command::SetVolume(50)).unwrap();
        w.publish();
        assert_eq!(volumes(&events), [25, 50], "reopened, then set");
        assert_eq!(volume(&w), Some(50));
        assert_eq!(watches.lock().unwrap().len(), 1, "watched once opened");

        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let mut m = FakeMixer::new(100);
        m.closed = true;
        let watches2 = Arc::clone(&m.watches);
        let (mut w2, events2) = faked(FakeSink::default(), m);
        play_one(&mut w2, &a);
        w2.publish();
        assert_eq!(volumes(&events2), [25], "opened on the first ensure");
        assert_eq!(volume(&w2), Some(25));
        for ws in [watches, watches2] {
            for (_, end) in ws.lock().unwrap().iter_mut() {
                end.take();
            }
        }
    }

    /// Wait for a fake watcher thread to finish (bounded).
    fn finished(w: &Worker<FakeSink>) {
        for _ in 0..10_000 {
            if w.watcher.as_ref().is_none_or(JoinHandle::is_finished) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("watcher never finished");
    }

    #[test]
    fn mixer_survives_the_device_going_away() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let mut m = FakeMixer::new(100);
        m.fail_set = true;
        m.absent = 1;
        let watches = Arc::clone(&m.watches);
        let (mut w, events) = faked(FakeSink::default(), m);
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        assert_eq!(watches.lock().unwrap().len(), 1, "armed on ensure");
        w.step();
        let err = w.handle(Command::SetVolume(30)).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Device);
        w.publish();
        assert!(volumes(&events).is_empty());
        assert_eq!(volume(&w), Some(100));
        assert_eq!(w.state, PlayState::Playing);
        w.handle(Command::Play(Some(0))).unwrap();
        assert_eq!(watches.lock().unwrap().len(), 1, "a live watcher is kept");
        watches.lock().unwrap()[0].1.take();
        finished(&w);
        w.handle(Command::Play(Some(0))).unwrap();
        assert_eq!(watches.lock().unwrap().len(), 1, "the card is not back");
        w.handle(Command::Play(Some(0))).unwrap();
        assert_eq!(watches.lock().unwrap().len(), 2, "re-armed on ensure");
        w.publish();
        assert_eq!(volumes(&events), [25], "the reopened card's level");
        w.handle(Command::SetVolume(30)).unwrap();
        w.publish();
        assert_eq!(volumes(&events), [30]);
        drive(&mut w);
        assert!(w.output.heard.ends_with(&concat(&[&a])), "playback went on");
        for (_, end) in watches.lock().unwrap().iter_mut() {
            end.take();
        }
    }

    #[test]
    fn external_volume_changes_are_published_once() {
        let m = FakeMixer::new(100);
        let watches = Arc::clone(&m.watches);
        let (player, events) = Player::spawn_with_mixer(FakeSink::default(), Some(Box::new(m)));
        assert_eq!(player.snapshot().status.volume, Some(100));
        player.send(Command::Stop).unwrap();
        {
            let w = watches.lock().unwrap();
            assert_eq!(w.len(), 1, "armed when the player starts");
            for p in [100, 70, 70] {
                assert!((w[0].0)(p));
            }
        }
        player.send(Command::Stop).unwrap();
        assert_eq!(player.snapshot().status.volume, Some(70));
        assert_eq!(volumes(&events), [70]);
        drop(player);
        let mut w = watches.lock().unwrap();
        assert!(!(w[0].0)(50), "the watcher learns the player is gone");
        w[0].1.take();
    }

    #[test]
    fn queue_plays_gapless_and_bit_exact() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 2 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let c = flac(&d, "c.flac", 96_000, 24, 2, BLOCK);
        let e = flac(&d, "e.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![
                a.path.clone(),
                b.path.clone(),
                c.path.clone(),
                e.path.clone(),
            ],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        drive(&mut w);
        assert!(
            w.output.heard == concat(&[&a, &b, &c, &e]),
            "output differs"
        );
        // Same-format neighbours share one open; only real changes reopen.
        assert_eq!(
            w.output.opens,
            [fmt(44_100, 16), fmt(96_000, 24), fmt(44_100, 16)]
        );
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
        let tracks: Vec<_> = events
            .try_iter()
            .filter_map(|e| match e {
                Event::TrackChanged { index } => Some(index),
                _ => None,
            })
            .collect();
        assert_eq!(tracks, [Some(0), Some(1), Some(2), Some(3), None]);
    }

    #[test]
    fn play_from_stopped_emits_one_state_event() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.publish();
        let _ = states(&events);
        w.handle(Command::Play(None)).unwrap();
        assert!(states(&events).is_empty(), "nothing sent before publishing");
        w.publish();
        assert_eq!(states(&events), [(PlayState::Playing, 0)]);
    }

    #[test]
    fn pause_and_resume_lose_nothing() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let (mut w, events) = worker(FakeSink {
            delay: 1_000,
            ..FakeSink::default()
        });
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        for _ in 0..3 {
            w.step();
        }
        let heard_frames = w.written - 1_000;
        w.handle(Command::Pause).unwrap();
        w.publish();
        assert_eq!(w.state, PlayState::Paused);
        assert_eq!(w.written, heard_frames, "decoder rewound to what was heard");
        assert_eq!(w.anchor_ms, heard_frames * 1000 / 44_100);
        assert_eq!(
            states(&events).last(),
            Some(&(PlayState::Paused, w.anchor_ms))
        );
        w.step();
        assert_eq!(w.written, heard_frames, "paused: nothing written");
        w.handle(Command::Play(None)).unwrap();
        drive(&mut w);
        assert!(w.output.heard == a.expected, "pause/resume is not seamless");
    }

    #[test]
    fn seek_continues_from_the_exact_frame() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.step();
        let before = w.output.heard.len();
        w.handle(Command::Seek { ms: 250 }).unwrap();
        let frame = 250 * 44_100 / 1000;
        assert_eq!(w.anchor_ms, 250);
        drive(&mut w);
        assert!(w.output.heard[..before] == a.expected[..before]);
        assert!(w.output.heard[before..] == a.expected[frame * 2..]);
    }

    #[test]
    fn seek_past_end_moves_to_the_next_track() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Seek { ms: u64::MAX }).unwrap();
        assert_eq!(w.queue.current(), Some(1));
        drive(&mut w);
        assert!(w.output.heard == b.expected);
    }

    #[test]
    fn seek_without_a_track_is_ignored() {
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Seek { ms: 1_000 }).unwrap();
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
    }

    #[test]
    fn next_previous_and_jumps_respect_bounds() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Next).unwrap();
        assert_eq!(w.queue.current(), None, "next with nothing loaded");
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Previous).unwrap();
        assert_eq!(w.queue.current(), Some(0), "previous at the start restarts");
        w.handle(Command::Next).unwrap();
        assert_eq!(w.queue.current(), Some(1));
        w.handle(Command::Next).unwrap();
        assert_eq!(w.queue.current(), Some(1), "next at the end does nothing");
        let err = w.handle(Command::Play(Some(7))).unwrap_err();
        assert!(matches!(err, Error::OutOfRange { index: 7, len: 2 }));
        assert_eq!(
            w.queue.current(),
            Some(1),
            "out-of-range jump leaves the queue alone"
        );
        w.handle(Command::Play(Some(0))).unwrap();
        assert_eq!((w.queue.current(), w.state), (Some(0), PlayState::Playing));
        w.handle(Command::Pause).unwrap();
        w.handle(Command::Next).unwrap();
        assert_eq!((w.queue.current(), w.state), (Some(1), PlayState::Paused));
    }

    #[test]
    fn next_while_stopped_only_moves_the_cursor() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Stop).unwrap();
        assert!(w.output.open.is_none(), "stop releases the device");
        w.handle(Command::Next).unwrap();
        assert_eq!((w.queue.current(), w.state), (Some(1), PlayState::Stopped));
        assert!(w.source.is_none());
        w.handle(Command::Play(None)).unwrap();
        drive(&mut w);
        assert!(w.output.heard.ends_with(&b.expected));
    }

    #[test]
    fn toggle_flips_between_playing_and_paused() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Toggle).unwrap();
        assert_eq!(w.state, PlayState::Stopped, "toggle with an empty queue");
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Toggle).unwrap();
        assert_eq!(w.state, PlayState::Playing);
        w.handle(Command::Toggle).unwrap();
        assert_eq!(w.state, PlayState::Paused);
        w.handle(Command::Pause).unwrap();
        assert_eq!(w.state, PlayState::Paused, "pause while paused");
        w.handle(Command::Toggle).unwrap();
        w.handle(Command::Play(None)).unwrap();
        assert_eq!(w.state, PlayState::Playing, "play while playing");
    }

    #[test]
    fn unreadable_entries_are_skipped() {
        let d = Dir::new();
        let junk = write_bytes(&d, "junk.flac", &[0x5a; 512]);
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![junk.clone(), a.path.clone(), junk.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        assert_eq!(w.queue.current(), Some(1));
        w.publish();
        let errs = errors(&events);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::Decode);
        assert!(errs[0].message.contains("junk.flac"));
        assert!(last_error(&w).is_some(), "kept after the entry that loaded");
        drive(&mut w);
        assert!(w.output.heard == a.expected);
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));

        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![junk],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
    }

    #[test]
    fn play_out_of_range_is_an_error() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (player, _events) = Player::spawn_with(FakeSink::default());
        player
            .send(Command::Add {
                paths: vec![a.path.clone(), a.path.clone()],
                at: None,
            })
            .unwrap();
        let err = player.send(Command::Play(Some(5))).unwrap_err();
        assert!(matches!(err, Error::OutOfRange { index: 5, len: 2 }));
        assert_eq!(player.snapshot().status.state, PlayState::Stopped);
    }

    fn errors(events: &mpsc::Receiver<Event>) -> Vec<lossless_proto::WireError> {
        events
            .try_iter()
            .filter_map(|e| match e {
                Event::Error(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    fn last_error(w: &Worker<FakeSink>) -> Option<lossless_proto::WireError> {
        lock(&w.shared).at(Instant::now()).status.last_error
    }

    #[test]
    fn decode_error_then_device_failure_reports_the_latest() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let bytes = std::fs::read(&a.path).unwrap();
        let cut = write_bytes(&d, "cut.flac", &bytes[..bytes.len() * 2 / 3]);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![cut, b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.output.fail_ensure = true;
        drive(&mut w);
        let codes: Vec<_> = errors(&events).iter().map(|e| e.code).collect();
        assert_eq!(codes, [ErrorCode::Decode, ErrorCode::Device]);
        assert_eq!(last_error(&w).map(|e| e.code), Some(ErrorCode::Device));
    }

    #[test]
    fn a_clean_load_after_a_skipped_entry_clears_the_error() {
        let d = Dir::new();
        let junk = write_bytes(&d, "junk.flac", &[0x5a; 512]);
        let a = flac(&d, "a.flac", 44_100, 16, 2, 2 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![junk, a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.publish();
        assert!(last_error(&w).is_some());
        drive_to(&mut w, 2);
        assert_eq!(last_error(&w), None);
    }

    #[test]
    fn seek_failures_reach_the_caller() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let bytes = std::fs::read(&a.path).unwrap();
        let cut = write_bytes(&d, "cut.flac", &bytes[..bytes.len() / 2]);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![cut],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        let err = w.handle(Command::Seek { ms: 350 }).unwrap_err();
        assert!(matches!(err, Error::Decode { .. }), "{err}");
    }

    #[test]
    fn decode_error_mid_track_moves_on() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let c = flac(&d, "c.flac", 44_100, 16, 2, BLOCK);
        let bytes = std::fs::read(&a.path).unwrap();
        let cut = write_bytes(&d, "cut.flac", &bytes[..bytes.len() * 2 / 3]);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![cut, b.path.clone(), c.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        drive_to(&mut w, 1);
        let errs = errors(&events);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::Decode);
        assert!(errs[0].message.contains("cut.flac"), "{}", errs[0].message);
        assert_eq!(last_error(&w), Some(errs[0].clone()));
        drive_to(&mut w, 2);
        assert_eq!(last_error(&w), None, "cleared once a track loads");
        drive(&mut w);
        let heard = &w.output.heard;
        let tail = concat(&[&b, &c]);
        assert!(heard.ends_with(&tail), "next tracks played in full");
        let partial = &heard[..heard.len() - tail.len()];
        assert!(
            a.expected.starts_with(partial),
            "what played of the cut file is exact"
        );
    }

    #[test]
    fn output_failures_stop_playback() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink {
            fail_ensure: true,
            ..FakeSink::default()
        });
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        assert_eq!(w.state, PlayState::Stopped);
        assert_eq!(w.output.stops, 1);
        assert_eq!(w.queue.entries().len(), 1);
        w.publish();
        let errs = errors(&events);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::Device);
        assert_eq!(last_error(&w), Some(errs[0].clone()));

        let (mut w, events) = worker(FakeSink {
            fail_write: true,
            ..FakeSink::default()
        });
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.step();
        assert_eq!(w.state, PlayState::Stopped);
        assert!(w.source.is_none());
        assert_eq!(w.queue.entries().len(), 1);
        let errs = errors(&events);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::Device);
    }

    #[test]
    fn clear_stops_and_empties() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Clear).unwrap();
        assert!(w.queue.entries().is_empty());
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
        w.handle(Command::Play(None)).unwrap();
        assert_eq!(w.state, PlayState::Stopped, "play with an empty queue");
    }

    #[test]
    fn gapless_change_anchors_after_the_buffered_tail() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink {
            delay: 44_100,
            ..FakeSink::default()
        });
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        let before = Instant::now();
        drive_to(&mut w, 1);
        assert_eq!(w.anchor_ms, 0);
        // a's whole 4096 frames are still buffered when b starts decoding.
        let tail = Duration::from_millis(4096 * 1000 / 44_100);
        assert!(w.anchor_at >= before + tail);
    }

    fn tracks(events: &mpsc::Receiver<Event>) -> Vec<Option<usize>> {
        events
            .try_iter()
            .filter_map(|e| match e {
                Event::TrackChanged { index } => Some(index),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn edits_during_a_buffered_tail_keep_the_playing_file() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, 2 * BLOCK);
        let c = flac(&d, "c.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink {
            delay: 2 * BLOCK as u64,
            ..FakeSink::default()
        });
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        drive_to(&mut w, 1);
        assert!(w.anchor_at > Instant::now(), "a's tail is still buffered");
        w.publish();
        let _ = events.try_iter().count();
        w.handle(Command::Add {
            paths: vec![c.path.clone()],
            at: Some(0),
        })
        .unwrap();
        w.publish();
        let snap = lock(&w.shared).at(Instant::now());
        assert_eq!(snap.status.index, Some(2));
        assert_eq!(snap.queue[2], b.path);
        let evs: Vec<_> = events.try_iter().collect();
        assert!(evs.contains(&Event::QueueChanged));
        assert!(evs.contains(&Event::TrackChanged { index: Some(2) }));
        w.handle(Command::Move { from: 0, to: 2 }).unwrap();
        assert_eq!(w.queue.current(), Some(1));
        w.handle(Command::Remove(vec![0])).unwrap();
        assert_eq!(w.queue.current(), Some(0));
        w.publish();
        let _ = events.try_iter().count();
        let err = w.handle(Command::Remove(vec![9])).unwrap_err();
        assert!(matches!(err, Error::OutOfRange { index: 9, len: 2 }));
        w.publish();
        assert!(
            events.try_iter().next().is_none(),
            "failed edits are silent"
        );
        drive(&mut w);
        assert!(
            w.output.heard == concat(&[&a, &b, &c]),
            "edits must not disturb the buffered audio"
        );
        assert_eq!(w.output.opens.len(), 1);
    }

    #[test]
    fn a_device_failure_after_an_edit_still_reports_the_track() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, 4 * BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        let edits = [
            Command::Replace(vec![b.path.clone()]),
            Command::Remove(vec![0]),
        ];
        for edit in edits {
            w.output.fail_ensure = false;
            w.handle(Command::Replace(vec![a.path.clone(), b.path.clone()]))
                .unwrap();
            w.publish();
            let _ = events.try_iter().count();
            w.output.fail_ensure = true;
            w.handle(edit.clone()).unwrap();
            assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
            w.publish();
            assert_eq!(tracks(&events), [None], "{edit:?}");
        }
    }

    #[test]
    fn replace_keeps_the_device_open_on_same_format() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.step();
        let opens = w.output.opens.len();
        let _ = events.try_iter().count();
        w.handle(Command::Replace(vec![b.path.clone()])).unwrap();
        assert_eq!(w.output.opens.len(), opens, "device stayed open");
        assert_eq!((w.state, w.queue.current()), (PlayState::Playing, Some(0)));
        assert_eq!(w.queue.entries(), std::slice::from_ref(&b.path));
        w.publish();
        let evs: Vec<_> = events.try_iter().collect();
        assert!(evs.contains(&Event::QueueChanged));
        assert!(evs.contains(&Event::TrackChanged { index: Some(0) }));
        let before = w.output.heard.len();
        drive(&mut w);
        assert!(w.output.heard[before..] == b.expected);

        w.handle(Command::Replace(Vec::new())).unwrap();
        assert!(w.queue.entries().is_empty());
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
    }

    #[test]
    fn removing_the_playing_entry_plays_the_next() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let c = flac(&d, "c.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink {
            delay: BLOCK as u64,
            ..FakeSink::default()
        });
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone(), c.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.step();
        w.step();
        w.publish();
        let _ = events.try_iter().count();
        w.handle(Command::Remove(vec![0])).unwrap();
        let cut = w.output.heard.len();
        assert!(cut < 2 * BLOCK * 2, "buffered audio was discarded");
        assert!(w.output.heard == a.expected[..cut]);
        assert_eq!((w.state, w.queue.current()), (PlayState::Playing, Some(0)));
        w.publish();
        let evs: Vec<_> = events.try_iter().collect();
        assert!(evs.contains(&Event::QueueChanged));
        assert!(evs.contains(&Event::TrackChanged { index: Some(0) }));
        drive(&mut w);
        assert!(w.output.heard[cut..] == concat(&[&b, &c]));

        w.handle(Command::Play(Some(0))).unwrap();
        w.handle(Command::Pause).unwrap();
        w.handle(Command::Remove(vec![0])).unwrap();
        assert_eq!((w.state, w.queue.current()), (PlayState::Paused, Some(0)));
        w.handle(Command::Stop).unwrap();
        w.publish();
        let _ = events.try_iter().count();
        w.handle(Command::Remove(vec![0])).unwrap();
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
        w.publish();
        assert_eq!(tracks(&events), [None]);
    }

    #[test]
    fn removing_the_last_playing_entry_stops() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, 4 * BLOCK);
        let (mut w, events) = worker(FakeSink {
            delay: BLOCK as u64,
            ..FakeSink::default()
        });
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(Some(1))).unwrap();
        w.step();
        w.publish();
        let _ = events.try_iter().count();
        w.handle(Command::Remove(vec![1])).unwrap();
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
        assert!(w.output.open.is_none(), "device released");
        assert!(w.output.heard.is_empty(), "buffered audio was discarded");
        assert_eq!(w.queue.entries(), std::slice::from_ref(&a.path));
        w.publish();
        let evs: Vec<_> = events.try_iter().collect();
        assert!(evs.contains(&Event::QueueChanged));
        assert!(evs.contains(&Event::TrackChanged { index: None }));
    }

    #[test]
    fn repeat_one_is_gapless_and_bit_exact() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 2 * BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::SetMode {
            repeat: Some(Repeat::One),
            random: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        for _ in 0..100_000 {
            if w.output.heard.len() > 2 * a.expected.len() {
                break;
            }
            w.step();
        }
        w.handle(Command::SetMode {
            repeat: Some(Repeat::Off),
            random: None,
        })
        .unwrap();
        drive(&mut w);
        assert!(w.output.heard == concat(&[&a, &a, &a]), "output differs");
        assert_eq!(w.output.opens.len(), 1);
        assert_eq!(tracks(&events), [Some(0), Some(0), Some(0), None]);
    }

    #[test]
    fn random_plays_all() {
        let d = Dir::new();
        let fs: Vec<Fixture> = (0..5)
            .map(|i| flac(&d, &format!("{i}.flac"), 44_100, 16, 2, BLOCK))
            .collect();
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: fs.iter().map(|f| f.path.clone()).collect(),
            at: None,
        })
        .unwrap();
        w.handle(Command::SetMode {
            repeat: Some(Repeat::Off),
            random: Some(true),
        })
        .unwrap();
        for pass in 0..2 {
            w.output.heard.clear();
            w.handle(Command::Play(None)).unwrap();
            drive(&mut w);
            assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
            let played: Vec<usize> = tracks(&events).into_iter().flatten().collect();
            let mut sorted = played.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, [0, 1, 2, 3, 4], "pass {pass}: {played:?}");
            let order: Vec<&Fixture> = played.iter().map(|&i| &fs[i]).collect();
            assert!(
                w.output.heard == concat(&order),
                "pass {pass}: output differs"
            );
        }
    }

    #[test]
    fn a_jump_after_a_finished_shuffle_starts_a_new_cycle() {
        let d = Dir::new();
        let fs: Vec<Fixture> = (0..4)
            .map(|i| flac(&d, &format!("{i}.flac"), 44_100, 16, 2, BLOCK))
            .collect();
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: fs.iter().map(|f| f.path.clone()).collect(),
            at: None,
        })
        .unwrap();
        w.handle(Command::SetMode {
            repeat: Some(Repeat::Off),
            random: Some(true),
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        drive(&mut w);
        w.publish();
        let _ = events.try_iter().count();
        w.handle(Command::Play(Some(2))).unwrap();
        drive(&mut w);
        let played: Vec<usize> = tracks(&events).into_iter().flatten().collect();
        assert_eq!(played[0], 2);
        let mut sorted = played.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [0, 1, 2, 3], "{played:?}");
    }

    #[test]
    fn previous_after_a_finish_goes_back_to_the_last_track() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        drive(&mut w);
        w.publish();
        let _ = events.try_iter().count();
        w.handle(Command::Previous).unwrap();
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, Some(1)));
        w.publish();
        assert_eq!(tracks(&events), [Some(1)]);
    }

    #[test]
    fn broken_entries_never_stop_a_repeating_shuffle() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let junk = write_bytes(&d, "junk.flac", &[0x5a; 512]);
        for seed in 1..16 {
            let (mut w, _events) = worker(FakeSink::default());
            w.queue = Queue::with_seed(seed);
            w.handle(Command::Add {
                paths: vec![a.path.clone(), junk.clone(), junk.clone()],
                at: None,
            })
            .unwrap();
            w.handle(Command::SetMode {
                repeat: Some(Repeat::Queue),
                random: Some(true),
            })
            .unwrap();
            w.handle(Command::Play(None)).unwrap();
            for _ in 0..60 {
                assert_eq!(w.state, PlayState::Playing, "seed {seed}");
                w.step();
            }
        }
    }

    #[test]
    fn set_mode_emits_one_mode_event_and_publishes() {
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::SetMode {
            repeat: Some(Repeat::Queue),
            random: None,
        })
        .unwrap();
        w.publish();
        w.handle(Command::SetMode {
            repeat: None,
            random: Some(true),
        })
        .unwrap();
        w.publish();
        let modes: Vec<Event> = events
            .try_iter()
            .filter(|e| matches!(e, Event::Mode { .. }))
            .collect();
        let mode = |repeat, random| Event::Mode { repeat, random };
        assert_eq!(
            modes,
            [mode(Repeat::Queue, false), mode(Repeat::Queue, true)]
        );
        let st = lock(&w.shared).at(Instant::now()).status;
        assert_eq!((st.repeat, st.random), (Repeat::Queue, true));
    }

    fn restore(w: &mut Worker<FakeSink>, queue: Vec<std::path::PathBuf>, index: Option<usize>) {
        w.handle(Command::Restore {
            queue,
            index,
            position_ms: 250,
            repeat: Repeat::Queue,
            random: true,
        })
        .unwrap();
        w.publish();
    }

    #[test]
    fn restore_resumes_bit_exact_without_opening_the_device() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        restore(&mut w, vec![a.path.clone()], Some(0));
        assert!(w.output.opens.is_empty(), "device opened before play");
        let st = lock(&w.shared).at(Instant::now()).status;
        assert_eq!(
            (st.state, st.index, st.position_ms, st.repeat, st.random),
            (PlayState::Paused, Some(0), 250, Repeat::Queue, true)
        );
        let got: Vec<Event> = events.try_iter().collect();
        assert_eq!(
            got,
            [
                Event::QueueChanged,
                Event::Mode {
                    repeat: Repeat::Queue,
                    random: true
                },
                Event::TrackChanged { index: Some(0) },
                Event::State {
                    state: PlayState::Paused,
                    position_ms: 250
                },
            ]
        );
        w.handle(Command::SetMode {
            repeat: Some(Repeat::Off),
            random: None,
        })
        .unwrap();
        w.handle(Command::Toggle).unwrap();
        assert_eq!(w.output.opens, [fmt(44_100, 16)]);
        drive(&mut w);
        let frame = 250 * 44_100 / 1000;
        assert!(
            w.output.heard == a.expected[frame * 2..],
            "resume not exact"
        );
    }

    #[test]
    fn restore_keeps_the_queue_when_its_entry_is_broken() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let junk = write_bytes(&d, "junk.flac", &[0x5a; 512]);
        let (mut w, events) = worker(FakeSink::default());
        restore(&mut w, vec![junk.clone(), a.path.clone()], Some(0));
        assert_eq!(errors(&events).len(), 1);
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, Some(0)));
        assert_eq!(w.queue.entries().len(), 2);
        assert!(w.output.opens.is_empty());

        restore(&mut w, vec![a.path.clone()], Some(0));
        w.handle(Command::Restore {
            queue: vec![a.path.clone()],
            index: Some(0),
            position_ms: u64::MAX / 1000,
            repeat: Repeat::Off,
            random: false,
        })
        .unwrap();
        assert_eq!(w.state, PlayState::Stopped, "seek past the end");
        w.handle(Command::Restore {
            queue: vec![a.path.clone()],
            index: Some(0),
            position_ms: 0,
            repeat: Repeat::Off,
            random: false,
        })
        .unwrap();
        assert_eq!(w.state, PlayState::Paused, "no seek needed at 0");

        restore(&mut w, vec![a.path.clone()], Some(5));
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, None));
        assert_eq!(lock(&w.shared).queue.len(), 1);
    }

    #[test]
    fn resume_after_restore_reports_a_device_failure() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let (mut w, events) = worker(FakeSink {
            fail_ensure: true,
            ..FakeSink::default()
        });
        restore(&mut w, vec![a.path.clone()], Some(0));
        assert_eq!(w.state, PlayState::Paused);
        assert!(errors(&events).is_empty());
        w.handle(Command::Play(None)).unwrap();
        w.publish();
        assert_eq!(errors(&events)[0].code, ErrorCode::Device);
        assert_eq!((w.state, w.queue.current()), (PlayState::Stopped, Some(0)));
    }

    #[test]
    fn next_skips_past_repeat_one() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: vec![a.path.clone(), b.path.clone()],
            at: None,
        })
        .unwrap();
        w.handle(Command::SetMode {
            repeat: Some(Repeat::One),
            random: None,
        })
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Next).unwrap();
        assert_eq!(w.queue.current(), Some(1));
        w.handle(Command::Next).unwrap();
        assert_eq!(w.queue.current(), Some(1), "next at the end does nothing");
        w.handle(Command::Previous).unwrap();
        assert_eq!(w.queue.current(), Some(0));
    }

    #[test]
    fn removing_the_playing_entry_in_random_picks_a_random_next() {
        let d = Dir::new();
        let fs: Vec<Fixture> = (0..3)
            .map(|i| flac(&d, &format!("{i}.flac"), 44_100, 16, 2, BLOCK))
            .collect();
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add {
            paths: fs.iter().map(|f| f.path.clone()).collect(),
            at: None,
        })
        .unwrap();
        w.handle(Command::SetMode {
            repeat: None,
            random: Some(true),
        })
        .unwrap();
        w.handle(Command::Play(Some(2))).unwrap();
        w.publish();
        let _ = events.try_iter().count();
        w.handle(Command::Remove(vec![2])).unwrap();
        w.publish();
        let now = w.queue.current();
        assert_eq!(tracks(&events), [now], "one track event");
        assert_eq!(
            w.state,
            PlayState::Playing,
            "the last entry was not the end"
        );
        assert!(w.queue.current().is_some_and(|c| c < 2));
    }

    #[test]
    fn snapshot_advances_from_the_anchor() {
        let mut shared = Shared::new();
        let t0 = Instant::now();
        shared.status.state = PlayState::Playing;
        shared.status.position_ms = 10_000;
        shared.status.duration_ms = Some(12_000);
        shared.anchor = Some(t0);
        let at = |d: u64| shared.at(t0 + Duration::from_millis(d)).status.position_ms;
        assert_eq!(at(0), 10_000);
        assert_eq!(at(1_500), 11_500);
        assert_eq!(at(60_000), 12_000, "clamped to the duration");
        let early = shared.at(t0 - Duration::from_millis(5)).status.position_ms;
        assert_eq!(early, 10_000, "nothing heard before the anchor");
        shared.anchor = None;
        assert_eq!(
            shared.at(t0 + Duration::from_secs(9)).status.position_ms,
            10_000
        );
    }

    #[test]
    fn commands_take_effect_before_send_returns() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 20 * BLOCK);
        let (player, _events) = Player::spawn_with(crate::fixtures::PacedSink::default());
        let state = |p: &Player| {
            let s = p.snapshot().status;
            (s.state, s.index)
        };
        player
            .send(Command::Add {
                paths: vec![a.path.clone()],
                at: None,
            })
            .unwrap();
        assert_eq!(player.snapshot().queue.len(), 1);
        player.send(Command::Play(None)).unwrap();
        assert_eq!(state(&player), (PlayState::Playing, Some(0)));
        player.send(Command::Pause).unwrap();
        assert_eq!(state(&player), (PlayState::Paused, Some(0)));
        player.send(Command::Seek { ms: 1_000 }).unwrap();
        assert_eq!(player.snapshot().status.position_ms, 1_000);
        player.send(Command::Stop).unwrap();
        assert_eq!(state(&player).0, PlayState::Stopped);
    }

    #[test]
    fn thread_wiring_and_shutdown() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (player, events) = Player::spawn_with(FakeSink::default());
        player
            .send(Command::Add {
                paths: vec![a.path.clone()],
                at: None,
            })
            .unwrap();
        player.send(Command::Play(None)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match events.recv_timeout(Duration::from_secs(10)).expect("event") {
                Event::TrackChanged { index: None } => break,
                _ => assert!(Instant::now() < deadline, "playback never ended"),
            }
        }
        // The event was sent after publishing: status already agrees.
        let snap = player.snapshot();
        assert_eq!(snap.status.state, PlayState::Stopped);
        assert_eq!(snap.status.index, None);
        assert_eq!(snap.queue.len(), 1);
        drop(player);
    }
}

#[cfg(test)]
mod props {
    use std::time::{Duration, Instant};

    use lossless_proto::PlayState;
    use proptest::prelude::*;

    use super::Shared;

    proptest! {
        /// The derived position never goes backwards and never passes the
        /// duration, whatever the anchor and the clock say.
        #[test]
        fn position_is_monotonic_and_bounded(
            base in any::<u64>(),
            duration in proptest::option::of(any::<u64>()),
            mut a in 0u64..1 << 40,
            mut b in 0u64..1 << 40,
        ) {
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            let mut shared = Shared::new();
            let t0 = Instant::now();
            shared.status.state = PlayState::Playing;
            shared.status.position_ms = base;
            shared.status.duration_ms = duration;
            shared.anchor = Some(t0);
            let pa = shared.at(t0 + Duration::from_millis(a)).status.position_ms;
            let pb = shared.at(t0 + Duration::from_millis(b)).status.position_ms;
            prop_assert!(pa <= pb);
            if let Some(d) = duration {
                prop_assert!(pb <= d.max(base.min(d)));
            }
        }
    }
}
