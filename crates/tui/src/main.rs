#![forbid(unsafe_code)]

mod app;
mod clock;
mod config;
mod conn;
mod filter;
mod input;
mod keys;
mod local;
#[cfg(feature = "scripts")]
mod scripts;
mod term;
mod theme;
mod ui;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Instant;

use lossless_proto::profile::{Dirs, Profile};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::{App, Out, update};
use clock::Clock;
use conn::{Conn, Msg, Resync};

const USAGE: &str = "usage: losslessd-tui [-p NAME | --socket PATH]";
const CHROME_ROWS: u16 = 6;

fn next_msg(rx: &Receiver<Msg>, clock: &Clock, now: Instant) -> Option<Msg> {
    match clock.next_wake(now) {
        Some(d) => match rx.recv_timeout(d) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => Some(Msg::Tick),
            Err(RecvTimeoutError::Disconnected) => None,
        },
        None => rx.recv().ok(),
    }
}

fn no_daemon(path: &std::path::Path) -> ExitCode {
    eprintln!(
        "losslessd-tui: connecting to {} (is losslessd running?)",
        path.display()
    );
    ExitCode::from(1)
}

fn set_page(app: &mut App, terminal: &Terminal<CrosstermBackend<Stdout>>) {
    if let Ok(size) = terminal.size() {
        app.page = usize::from(size.height.saturating_sub(CHROME_ROWS)).max(1);
        app.follow();
    }
}

fn apply(app: &mut App, conn: &Conn, r: std::io::Result<Resync>) {
    match r {
        Ok(r) => app.connected(conn.generation(), &r),
        Err(_) => app.connect_failed(conn.generation()),
    }
}

fn run(
    app: &mut App,
    conn: &mut Conn,
    dirs: &Dirs,
    rx: &Receiver<Msg>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> io::Result<()> {
    set_page(app, terminal);
    terminal.draw(|f| ui::draw(app, f))?;
    while let Some(msg) = next_msg(rx, &app.clock, Instant::now()) {
        let resize = matches!(msg, Msg::Resize);
        for out in update(app, msg, Instant::now()) {
            match out {
                Out::Send(cmd, pending) => {
                    if let Ok(id) = conn.send(cmd) {
                        app.expect(id, pending);
                    }
                }
                Out::Reconnect => {
                    let r = conn.reconnect();
                    apply(app, conn, r);
                }
                Out::Switch => {
                    app.socket = dirs.socket(&app.profiles[app.profile]);
                    let r = conn.switch(&app.socket);
                    apply(app, conn, r);
                }
                #[cfg(feature = "scripts")]
                Out::Run(run) => app.footer = Some(format!("running {}", run.name)),
            }
        }
        if app.quit {
            break;
        }
        if resize {
            set_page(app, terminal);
        }
        terminal.draw(|f| ui::draw(app, f))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut socket: Option<PathBuf> = None;
    let mut profile = Profile::default();
    while let Some(flag) = args.next() {
        match (flag.as_str(), args.next()) {
            ("--help", _) => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            ("--socket", Some(path)) => socket = Some(PathBuf::from(path)),
            ("-p" | "--profile", Some(name)) => match Profile::parse(&name) {
                Ok(p) => profile = p,
                Err(e) => {
                    eprintln!("losslessd-tui: {e}");
                    return ExitCode::from(2);
                }
            },
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let dirs = Dirs::from_env();
    let config = match config::load(&config::path(&dirs)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("losslessd-tui: {e}");
            return ExitCode::from(2);
        }
    };
    let switchable = socket.is_none();
    let path = socket.unwrap_or_else(|| dirs.socket(&profile));
    let (tx, rx) = mpsc::channel();
    let Ok(mut conn) = Conn::connect(&path, tx.clone()) else {
        return no_daemon(&path);
    };
    let Ok(resync) = conn.resync() else {
        return no_daemon(&path);
    };
    let mut profiles = dirs.profiles();
    let start = match profiles.iter().position(|p| *p == profile) {
        Some(i) => i,
        None => {
            profiles.push(profile.clone());
            profiles[1..].sort();
            profiles
                .iter()
                .position(|p| *p == profile)
                .unwrap_or_default()
        }
    };
    let mut app = App::new(profiles, start, switchable, config.keymap);
    app.scripts.items = config.scripts;
    app.script_lines = config.script_lines;
    app.socket = path.clone();
    app.connected(conn.generation(), &resync);
    term::install_panic_hook();
    let guard = match term::Guard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("losslessd-tui: terminal: {e}");
            return ExitCode::from(1);
        }
    };
    let mut terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
        Ok(t) => t,
        Err(e) => {
            drop(guard);
            eprintln!("losslessd-tui: terminal: {e}");
            return ExitCode::from(1);
        }
    };
    input::spawn(tx);
    let result = run(&mut app, &mut conn, &dirs, &rx, &mut terminal);
    drop(guard);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("losslessd-tui: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use lossless_proto::PlayState;

    use super::*;

    #[test]
    fn idle_loop_does_not_wake() {
        let t = Instant::now();
        let paused = Clock::from_event(PlayState::Paused, 0, None, t);
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = done_tx.send(next_msg(&rx, &paused, Instant::now()));
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(300)).is_err());
        tx.send(Msg::Resize).unwrap();
        let got = done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(got, Some(Msg::Resize)));

        let t = Instant::now();
        let playing = Clock::from_event(PlayState::Playing, 0, None, t);
        let (tx, rx) = mpsc::channel::<Msg>();
        let (ticks_tx, ticks_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Some(m) = next_msg(&rx, &playing, Instant::now()) {
                if ticks_tx.send(m).is_err() {
                    break;
                }
            }
        });
        let deadline = t + Duration::from_millis(2500);
        let mut ticks = 0;
        while let Ok(m) = ticks_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            assert!(matches!(m, Msg::Tick));
            ticks += 1;
        }
        assert_eq!(ticks, 2);
        drop(tx);
    }
}
