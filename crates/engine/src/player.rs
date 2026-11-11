use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lossless_proto::{Event, PlayState, Status, WireError};

use crate::output::{Output, Sink};
use crate::source::Source;
use crate::{Error, Result};

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

    /// Start the playback thread on any sink.
    pub fn spawn_with<S: Sink + Send + 'static>(sink: S) -> (Self, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let (worker, events_rx) = Worker::new(sink, rx);
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
    rx: Receiver<Envelope>,
    events: Sender<Event>,
    /// Events of the current step, sent only after the snapshot is
    /// published, so a client reacting to one never reads stale status.
    pending: Vec<Event>,
    /// The last recovery failure, until a track loads cleanly.
    last_error: Option<WireError>,
    shared: Arc<Mutex<Shared>>,
    dirty: bool,
    queue_dirty: bool,
}

impl<S: Sink> Worker<S> {
    fn new(output: S, rx: Receiver<Envelope>) -> (Self, Receiver<Event>) {
        let (events, events_rx) = mpsc::channel();
        let worker = Self {
            output,
            queue: Vec::new(),
            index: None,
            state: PlayState::Stopped,
            source: None,
            written: 0,
            anchor_ms: 0,
            anchor_at: Instant::now(),
            rx,
            events,
            pending: Vec::new(),
            last_error: None,
            shared: Arc::new(Mutex::new(Shared::new())),
            dirty: false,
            queue_dirty: false,
        };
        (worker, events_rx)
    }

    fn run(mut self) {
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

    fn emit(&mut self, event: Event) {
        self.dirty = true;
        self.pending.push(event);
    }

    /// Tell clients about a failure the player recovered from on its own.
    fn fail(&mut self, err: &Error) {
        eprintln!("{err}");
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
            index: self.index,
            position_ms: self.anchor_ms,
            duration_ms,
            output: self.output.format(),
            track: None,
            last_error: self.last_error.clone(),
        };
        shared.anchor = (self.state == PlayState::Playing).then_some(self.anchor_at);
        if self.queue_dirty {
            self.queue_dirty = false;
            shared.queue = Arc::from(self.queue.as_slice());
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
                if let Err(e) = self.output.write(chunk) {
                    self.fail(&e);
                    self.halt();
                    return;
                }
                self.written += frames;
            }
            Ok(None) => self.advance(),
            Err(e) => {
                self.advance();
                self.fail(&e);
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
        let mut skipped = false;
        while index < self.queue.len() {
            match Source::open(&self.queue[index]) {
                Ok(source) => match self.output.ensure(source.format) {
                    Ok(_) => {
                        if !skipped {
                            self.last_error = None;
                            self.dirty = true;
                        }
                        self.source = Some(source);
                        self.index = Some(index);
                        self.written = 0;
                        self.anchor_ms = 0;
                        self.anchor_at = starts_at;
                        self.emit(Event::TrackChanged { index: Some(index) });
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
                Err(e) => eprintln!("{e}"),
            }
        }
        let _ = self.output.discard();
        self.anchor_ms = self.frames_to_ms(self.written);
    }

    fn handle(&mut self, cmd: Command) -> Result<()> {
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
            Command::Play(Some(index)) => {
                return Err(Error::OutOfRange {
                    index,
                    len: self.queue.len(),
                });
            }
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
                return self.handle(next);
            }
            Command::Stop => {
                self.output.stop();
                self.source = None;
                self.written = 0;
                self.state = PlayState::Stopped;
                self.anchor(0, Instant::now());
            }
            Command::Next | Command::Previous => {
                let Some(i) = self.index else {
                    return Ok(());
                };
                let target = if cmd == Command::Next {
                    i + 1
                } else {
                    i.saturating_sub(1)
                };
                if target >= self.queue.len() {
                    return Ok(());
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
                    Err(e) => eprintln!("{e}"),
                }
            }
            Command::Shutdown => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use lossless_proto::{ErrorCode, Event, PlayState, StreamFormat};

    use super::{Command, Player, Shared, Worker, lock};
    use crate::fixtures::{BLOCK, Dir, Fixture, flac, write_bytes};
    use crate::output::Sink;
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
        let (_tx, rx) = mpsc::channel();
        Worker::new(sink, rx)
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
            if w.index == Some(index) {
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

    #[test]
    fn queue_plays_gapless_and_bit_exact() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 2 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let c = flac(&d, "c.flac", 96_000, 24, 2, BLOCK);
        let e = flac(&d, "e.flac", 44_100, 16, 2, BLOCK);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add(vec![
            a.path.clone(),
            b.path.clone(),
            c.path.clone(),
            e.path.clone(),
        ]))
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
        assert_eq!((w.state, w.index), (PlayState::Stopped, None));
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
        w.handle(Command::Add(vec![a.path.clone()])).unwrap();
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
        w.handle(Command::Add(vec![a.path.clone()])).unwrap();
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
        w.handle(Command::Add(vec![a.path.clone()])).unwrap();
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
        w.handle(Command::Add(vec![a.path.clone(), b.path.clone()]))
            .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Seek { ms: u64::MAX }).unwrap();
        assert_eq!(w.index, Some(1));
        drive(&mut w);
        assert!(w.output.heard == b.expected);
    }

    #[test]
    fn seek_without_a_track_is_ignored() {
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Seek { ms: 1_000 }).unwrap();
        assert_eq!((w.state, w.index), (PlayState::Stopped, None));
    }

    #[test]
    fn next_previous_and_jumps_respect_bounds() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Next).unwrap();
        assert_eq!(w.index, None, "next with nothing loaded");
        w.handle(Command::Add(vec![a.path.clone(), b.path.clone()]))
            .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Previous).unwrap();
        assert_eq!(w.index, Some(0), "previous at the start restarts");
        w.handle(Command::Next).unwrap();
        assert_eq!(w.index, Some(1));
        w.handle(Command::Next).unwrap();
        assert_eq!(w.index, Some(1), "next at the end does nothing");
        let err = w.handle(Command::Play(Some(7))).unwrap_err();
        assert!(matches!(err, Error::OutOfRange { index: 7, len: 2 }));
        assert_eq!(w.index, Some(1), "out-of-range jump is ignored");
        w.handle(Command::Play(Some(0))).unwrap();
        assert_eq!((w.index, w.state), (Some(0), PlayState::Playing));
        w.handle(Command::Pause).unwrap();
        w.handle(Command::Next).unwrap();
        assert_eq!((w.index, w.state), (Some(1), PlayState::Paused));
    }

    #[test]
    fn next_while_stopped_only_moves_the_cursor() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add(vec![a.path.clone(), b.path.clone()]))
            .unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Stop).unwrap();
        assert!(w.output.open.is_none(), "stop releases the device");
        w.handle(Command::Next).unwrap();
        assert_eq!((w.index, w.state), (Some(1), PlayState::Stopped));
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
        w.handle(Command::Add(vec![a.path.clone()])).unwrap();
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
        w.handle(Command::Add(vec![
            junk.clone(),
            a.path.clone(),
            junk.clone(),
        ]))
        .unwrap();
        w.handle(Command::Play(None)).unwrap();
        assert_eq!(w.index, Some(1));
        w.publish();
        let errs = errors(&events);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::Decode);
        assert!(errs[0].message.contains("junk.flac"));
        assert!(last_error(&w).is_some(), "kept after the entry that loaded");
        drive(&mut w);
        assert!(w.output.heard == a.expected);
        assert_eq!((w.state, w.index), (PlayState::Stopped, None));

        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add(vec![junk])).unwrap();
        w.handle(Command::Play(None)).unwrap();
        assert_eq!((w.state, w.index), (PlayState::Stopped, None));
    }

    #[test]
    fn play_out_of_range_is_an_error() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (player, _events) = Player::spawn_with(FakeSink::default());
        player
            .send(Command::Add(vec![a.path.clone(), a.path.clone()]))
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
    fn decode_error_mid_track_moves_on() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, 4 * BLOCK);
        let b = flac(&d, "b.flac", 44_100, 16, 2, BLOCK);
        let c = flac(&d, "c.flac", 44_100, 16, 2, BLOCK);
        let bytes = std::fs::read(&a.path).unwrap();
        let cut = write_bytes(&d, "cut.flac", &bytes[..bytes.len() * 2 / 3]);
        let (mut w, events) = worker(FakeSink::default());
        w.handle(Command::Add(vec![cut, b.path.clone(), c.path.clone()]))
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
        w.handle(Command::Add(vec![a.path.clone()])).unwrap();
        w.handle(Command::Play(None)).unwrap();
        assert_eq!(w.state, PlayState::Stopped);
        assert_eq!(w.output.stops, 1);
        assert_eq!(w.queue.len(), 1);
        w.publish();
        let errs = errors(&events);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::Device);
        assert_eq!(last_error(&w), Some(errs[0].clone()));

        let (mut w, events) = worker(FakeSink {
            fail_write: true,
            ..FakeSink::default()
        });
        w.handle(Command::Add(vec![a.path.clone()])).unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.step();
        assert_eq!(w.state, PlayState::Stopped);
        assert!(w.source.is_none());
        assert_eq!(w.queue.len(), 1);
        let errs = errors(&events);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::Device);
    }

    #[test]
    fn clear_stops_and_empties() {
        let d = Dir::new();
        let a = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let (mut w, _events) = worker(FakeSink::default());
        w.handle(Command::Add(vec![a.path.clone()])).unwrap();
        w.handle(Command::Play(None)).unwrap();
        w.handle(Command::Clear).unwrap();
        assert!(w.queue.is_empty());
        assert_eq!((w.state, w.index), (PlayState::Stopped, None));
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
        w.handle(Command::Add(vec![a.path.clone(), b.path.clone()]))
            .unwrap();
        w.handle(Command::Play(None)).unwrap();
        let before = Instant::now();
        drive_to(&mut w, 1);
        assert_eq!(w.anchor_ms, 0);
        // a's whole 4096 frames are still buffered when b starts decoding.
        let tail = Duration::from_millis(4096 * 1000 / 44_100);
        assert!(w.anchor_at >= before + tail);
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
        player.send(Command::Add(vec![a.path.clone()])).unwrap();
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
        player.send(Command::Add(vec![a.path.clone()])).unwrap();
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
