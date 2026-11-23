use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::Duration;

use super::Run;
use super::sanitize::{MAX_CHARS, sanitize};
use crate::conn::Msg;

/// How long, after `sh` exits, its output may still drain before the exit
/// is reported; a job it left in the background may hold the pipes open.
const DRAIN: Duration = Duration::from_millis(100);

/// A modal script: running in its own process group, its output arriving
/// as `Msg::ScriptOut`, then one `Msg::ScriptExit`.
pub struct Job {
    pub id: u64,
    pub pgid: u32,
    stdin: Option<ChildStdin>,
    selection: PathBuf,
}

fn selection_file(run: &Run) -> io::Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("lossless-tui-{}-{n}.sel", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    for p in &run.selection {
        writeln!(f, "{p}")?;
    }
    Ok(path)
}

pub fn start(run: &Run, tx: Sender<Msg>) -> io::Result<Job> {
    let selection = selection_file(run)?;
    let spawned = Command::new("sh")
        .arg("-c")
        .arg(&run.command)
        .envs(run.env.iter().map(|(k, v)| (k, v)))
        .env("LOSSLESS_SELECTION", &selection)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&selection);
            return Err(e);
        }
    };
    let id = run.id;
    let (done_tx, done_rx) = channel();
    let readers: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|o| Box::new(o) as _),
        child.stderr.take().map(|e| Box::new(e) as _),
    ];
    for r in readers.into_iter().flatten() {
        let (tx, done) = (tx.clone(), done_tx.clone());
        thread::spawn(move || {
            read_lines(r, id, &tx);
            let _ = done.send(());
        });
    }
    drop(done_tx);
    let stdin = child.stdin.take();
    let pgid = child.id();
    thread::spawn(move || {
        let status = child.wait();
        for _ in 0..2 {
            if done_rx.recv_timeout(DRAIN).is_err() {
                break;
            }
        }
        if let Ok(st) = status {
            let _ = tx.send(Msg::ScriptExit(id, st));
        }
    });
    Ok(Job {
        id,
        pgid,
        stdin,
        selection,
    })
}

/// Sends each line as it ends; a line longer than a modal shows is cut.
fn read_lines(mut r: impl Read, id: u64, tx: &Sender<Msg>) {
    let cap = MAX_CHARS * 4;
    let mut line = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &b in &buf[..n] {
            if b == b'\n' {
                if tx.send(Msg::ScriptOut(id, sanitize(&line))).is_err() {
                    return;
                }
                line.clear();
            } else if line.len() < cap {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        let _ = tx.send(Msg::ScriptOut(id, sanitize(&line)));
    }
}

impl Job {
    /// A line of input; a script that stopped reading just misses it.
    pub fn send_line(&mut self, line: &str) {
        if let Some(stdin) = &mut self.stdin
            && writeln!(stdin, "{line}").is_err()
        {
            self.stdin = None;
        }
    }

    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Kills the whole process group, so pipelines and background jobs go
    /// with `sh`. std has no killpg, hence the `kill` command.
    pub fn kill(&self) {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.pgid)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.selection);
    }
}

#[cfg(test)]
mod tests {
    use std::process::ExitStatus;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::config::Mode;
    use crate::conn::Msg;

    fn run(command: &str) -> Run {
        Run {
            id: 7,
            name: "t".into(),
            command: command.into(),
            mode: Mode::Modal,
            env: vec![("LOSSLESS_SOCKET".into(), "/run/x.sock".into())],
            selection: vec!["/m/a.flac".into(), "/m/b c.flac".into()],
        }
    }

    /// Output lines until the exit, at most 5 s.
    fn collect(rx: &Receiver<Msg>) -> (Vec<String>, Option<ExitStatus>) {
        let end = Instant::now() + Duration::from_secs(5);
        let mut lines = Vec::new();
        while let Ok(m) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            match m {
                Msg::ScriptOut(7, l) => lines.push(l),
                Msg::ScriptExit(7, st) => return (lines, Some(st)),
                _ => {}
            }
        }
        (lines, None)
    }

    #[test]
    fn echo_reaches_the_channel() {
        let (tx, rx) = channel();
        let _job = start(&run("echo hi; sleep 0.1; echo err >&2"), tx).unwrap();
        let (lines, st) = collect(&rx);
        assert!(st.unwrap().success());
        assert_eq!(lines, ["hi", "err"]);
    }

    #[test]
    fn stdin_line_round_trips() {
        let (tx, rx) = channel();
        let mut job = start(&run("read x; echo got:$x"), tx).unwrap();
        job.send_line("y");
        let (lines, _) = collect(&rx);
        assert_eq!(lines, ["got:y"]);
    }

    #[test]
    fn close_stdin_ends_a_reader() {
        let (tx, rx) = channel();
        let mut job = start(&run("cat; echo eof"), tx).unwrap();
        job.close_stdin();
        let (lines, st) = collect(&rx);
        assert!(st.unwrap().success());
        assert_eq!(lines, ["eof"]);
    }

    #[test]
    fn env_and_selection_file() {
        let (tx, rx) = channel();
        let job = start(
            &run("cat \"$LOSSLESS_SELECTION\"; echo $LOSSLESS_SOCKET; echo $LOSSLESS_SELECTION"),
            tx,
        )
        .unwrap();
        let (lines, _) = collect(&rx);
        assert_eq!(&lines[..3], ["/m/a.flac", "/m/b c.flac", "/run/x.sock"]);
        let file = std::path::PathBuf::from(&lines[3]);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(job);
        assert!(!file.exists());
    }

    #[test]
    fn esc_kills_the_whole_pipeline() {
        let (tx, rx) = channel();
        let job = start(&run("sleep 30 | sleep 30"), tx).unwrap();
        let pgid = job.pgid;
        job.kill();
        let (_, st) = collect(&rx);
        assert!(!st.expect("no exit after kill").success());
        let left = std::process::Command::new("pgrep")
            .args(["-g", &pgid.to_string()])
            .output();
        if let Ok(out) = left {
            assert!(out.stdout.is_empty(), "left behind: {:?}", out.stdout);
        }
    }

    #[test]
    fn spawn_failure_is_an_error() {
        let (tx, _rx) = channel();
        let mut r = run("true");
        r.env.push(("BAD\0KEY".into(), "x".into()));
        assert!(start(&r, tx).is_err());
    }
}
