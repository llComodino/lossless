use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use lossless_proto::{Command, Request, ServerMessage, decode, encode};

pub enum Msg {
    Server(u32, ServerMessage),
    Disconnected(u32),
    Key(crossterm::event::KeyEvent),
    Resize,
    Tick,
}

/// Request ids of the replies that follow a (re)connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resync {
    pub subscribe: u64,
    pub status: u64,
    pub queue: u64,
}

pub struct Conn {
    stream: UnixStream,
    path: PathBuf,
    tx: Sender<Msg>,
    generation: u32,
    /// Never reset across reconnects, so ids stay unique per `Conn`.
    next_id: u64,
}

fn open(path: &Path, tx: &Sender<Msg>, generation: u32) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    let reader = BufReader::new(stream.try_clone()?);
    let tx = tx.clone();
    thread::spawn(move || read_loop(reader, tx, generation));
    Ok(stream)
}

fn read_loop(mut reader: BufReader<UnixStream>, tx: Sender<Msg>, generation: u32) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(Msg::Disconnected(generation));
                return;
            }
            Ok(_) => {
                if let Ok(msg) = decode::<ServerMessage>(&line)
                    && tx.send(Msg::Server(generation, msg)).is_err()
                {
                    return;
                }
            }
        }
    }
}

impl Conn {
    /// Connects and starts the reader; sends nothing.
    pub fn connect(path: &Path, tx: Sender<Msg>) -> io::Result<Conn> {
        let generation = 1;
        let stream = open(path, &tx, generation)?;
        Ok(Conn {
            stream,
            path: path.to_path_buf(),
            tx,
            generation,
            next_id: 1,
        })
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn send(&mut self, command: Command) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let line = encode(&Request { id, command }).map_err(io::Error::other)?;
        self.stream.write_all(line.as_bytes())?;
        Ok(id)
    }

    pub fn resync(&mut self) -> io::Result<Resync> {
        Ok(Resync {
            subscribe: self.send(Command::Subscribe)?,
            status: self.send(Command::Status)?,
            queue: self.send(Command::Queue)?,
        })
    }

    pub fn reconnect(&mut self) -> io::Result<Resync> {
        let _ = self.stream.shutdown(Shutdown::Both);
        self.generation += 1;
        self.stream = open(&self.path, &self.tx, self.generation)?;
        self.resync()
    }

    /// The path becomes current even when connecting fails.
    pub fn switch(&mut self, path: &Path) -> io::Result<Resync> {
        self.path = path.to_path_buf();
        self.reconnect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use lossless_daemon::{Daemon, bind};
    use lossless_engine::Player;
    use lossless_engine::fixtures::{Dir, PacedSink};
    use lossless_proto::{Body, Event, Outcome};

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    struct Server {
        dir: Dir,
        daemon: Arc<Daemon>,
    }

    impl Server {
        fn launch(dir: &Dir) -> Arc<Daemon> {
            std::fs::create_dir_all(dir.path("music")).unwrap();
            let listener = bind(&dir.path("sock")).unwrap();
            let (player, events) = Player::spawn_with_mixer(PacedSink::default(), None);
            let daemon = Arc::new(
                Daemon::start(
                    player,
                    events,
                    Some(dir.path("music")),
                    dir.path("state.json"),
                    Box::new(|| {}),
                )
                .unwrap(),
            );
            let d = daemon.clone();
            thread::spawn(move || d.serve(listener));
            daemon
        }

        fn new() -> Self {
            let dir = Dir::new();
            let daemon = Self::launch(&dir);
            Self { dir, daemon }
        }

        fn sock(&self) -> PathBuf {
            self.dir.path("sock")
        }
    }

    fn next(rx: &Receiver<Msg>) -> Msg {
        rx.recv_timeout(WAIT).expect("no message")
    }

    fn reply(rx: &Receiver<Msg>, generation: u32, want: u64) -> Outcome {
        loop {
            match next(rx) {
                Msg::Server(g, ServerMessage::Reply { id, result }) if id == want => {
                    assert_eq!(g, generation);
                    return result;
                }
                Msg::Disconnected(g) => assert_ne!(g, generation, "disconnected"),
                _ => {}
            }
        }
    }

    fn queue_changed(rx: &Receiver<Msg>, generation: u32) {
        loop {
            match next(rx) {
                Msg::Server(g, ServerMessage::Event { event }) => {
                    assert_eq!(g, generation);
                    if event == Event::QueueChanged {
                        return;
                    }
                }
                Msg::Disconnected(g) => assert_ne!(g, generation, "disconnected"),
                _ => {}
            }
        }
    }

    fn is_done(o: &Outcome) -> bool {
        matches!(o, Outcome::Ok(Body::Done))
    }

    #[test]
    fn requests_get_matching_replies() {
        let s = Server::new();
        let (tx, rx) = mpsc::channel();
        let mut c = Conn::connect(&s.sock(), tx).unwrap();
        let a = c.send(Command::Status).unwrap();
        let b = c.send(Command::Queue).unwrap();
        assert_ne!(a, b);
        assert!(matches!(
            reply(&rx, c.generation(), a),
            Outcome::Ok(Body::Status(_))
        ));
        assert!(matches!(
            reply(&rx, c.generation(), b),
            Outcome::Ok(Body::Queue(_))
        ));
    }

    #[test]
    fn events_arrive_after_subscribe() {
        let s = Server::new();
        let (tx, rx) = mpsc::channel();
        let mut c = Conn::connect(&s.sock(), tx).unwrap();
        let r = c.resync().unwrap();
        assert!(is_done(&reply(&rx, c.generation(), r.subscribe)));
        reply(&rx, c.generation(), r.status);
        reply(&rx, c.generation(), r.queue);
        let id = c.send(Command::Clear).unwrap();
        assert!(is_done(&reply(&rx, c.generation(), id)));
        queue_changed(&rx, c.generation());
    }

    #[test]
    fn switch_moves_to_another_daemon() {
        let a = Server::new();
        let b = Server::new();
        let (tx, rx) = mpsc::channel();
        let mut c = Conn::connect(&a.sock(), tx).unwrap();
        assert_eq!(c.generation(), 1);
        let r = c.switch(&b.sock()).unwrap();
        assert_eq!(c.generation(), 2);
        assert!(is_done(&reply(&rx, 2, r.subscribe)));
        reply(&rx, 2, r.status);
        reply(&rx, 2, r.queue);
        let id = c.send(Command::Clear).unwrap();
        assert!(is_done(&reply(&rx, 2, id)));
        queue_changed(&rx, 2);
    }

    #[test]
    fn reconnect_resubscribes_and_refetches() {
        let s = Server::new();
        let (tx, rx) = mpsc::channel();
        let mut c = Conn::connect(&s.sock(), tx).unwrap();
        let first = c.generation();
        s.daemon.shutdown();
        loop {
            if let Msg::Disconnected(g) = next(&rx) {
                assert_eq!(g, first);
                break;
            }
        }
        let _second = Server::launch(&s.dir);
        let r = c.reconnect().unwrap();
        assert_eq!(c.generation(), first + 1);
        let g = c.generation();
        assert!(is_done(&reply(&rx, g, r.subscribe)));
        assert!(matches!(
            reply(&rx, g, r.status),
            Outcome::Ok(Body::Status(_))
        ));
        assert!(matches!(
            reply(&rx, g, r.queue),
            Outcome::Ok(Body::Queue(_))
        ));
        let id = c.send(Command::Clear).unwrap();
        assert!(is_done(&reply(&rx, g, id)));
        queue_changed(&rx, g);
    }
}
