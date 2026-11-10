#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use lossless_proto::{
    Body, Command, Event, Outcome, PlayState, Request, ServerMessage, Status, StreamFormat, Track,
    decode, default_socket, encode,
};

/// `println!` that exits quietly when stdout is closed (e.g. piped to `head`)
/// instead of panicking.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout().lock(), $($arg)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            panic!("writing to stdout: {e}");
        }
    }};
}

const USAGE: &str = "usage: lossless [--json] [--socket PATH] <command>

  status                 state, position, output format
  queue                  list the queue
  add <path>...          add files or folders (relative to music_dir)
  play [N]               start/resume, or jump to queue entry N (1-based)
  pause | toggle | stop
  next | prev
  seek <[[h:]m:]s>       seek within the current track
  clear                  stop and empty the queue
  search <words>...      find tracks (artist, album, title or path)
  update                 rescan the music directory
  watch                  print events until interrupted";

fn parse_time(s: &str) -> Result<u64> {
    let mut secs: f64 = 0.0;
    for part in s.split(':') {
        let v: f64 = part.parse().with_context(|| format!("bad time {s:?}"))?;
        if !v.is_finite() || v.is_sign_negative() {
            bail!("bad time {s:?}");
        }
        secs = secs * 60.0 + v;
    }
    if !secs.is_finite() {
        bail!("bad time {s:?}");
    }
    Ok((secs * 1000.0) as u64)
}

fn parse(args: &[String]) -> Result<Command> {
    let (name, rest) = args.split_first().ok_or_else(|| anyhow!(USAGE))?;
    let none = |cmd: Command| {
        if rest.is_empty() {
            Ok(cmd)
        } else {
            Err(anyhow!("{name} takes no arguments"))
        }
    };
    match name.as_str() {
        "status" => none(Command::Status),
        "queue" => none(Command::Queue),
        "pause" => none(Command::Pause),
        "toggle" => none(Command::Toggle),
        "stop" => none(Command::Stop),
        "next" => none(Command::Next),
        "prev" | "previous" => none(Command::Previous),
        "clear" => none(Command::Clear),
        "watch" => none(Command::Subscribe),
        "update" => none(Command::Update),
        "search" if !rest.is_empty() => Ok(Command::Search {
            query: rest.join(" "),
        }),
        "add" if !rest.is_empty() => Ok(Command::Add {
            paths: rest.to_vec(),
        }),
        "play" => match rest {
            [] => Ok(Command::Play { index: None }),
            [n] => {
                let n: usize = n.parse().with_context(|| format!("bad index {n:?}"))?;
                let index = n
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("indexes start at 1"))?;
                Ok(Command::Play { index: Some(index) })
            }
            _ => bail!("play takes at most one index"),
        },
        "seek" => match rest {
            [t] => Ok(Command::Seek { ms: parse_time(t)? }),
            _ => bail!("seek takes one time"),
        },
        _ => Err(anyhow!(USAGE)),
    }
}

fn clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn format_str(f: &StreamFormat) -> String {
    format!(
        "{}-bit/{} kHz/{}ch",
        f.bits,
        f64::from(f.rate) / 1000.0,
        f.channels
    )
}

fn print_status(s: &Status) {
    let state = match s.state {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        PlayState::Stopped => "stopped",
    };
    let index = s.index.map_or_else(|| "-".into(), |i| (i + 1).to_string());
    let duration = s.duration_ms.map_or_else(|| "?".into(), clock);
    let output = s
        .output
        .as_ref()
        .map_or_else(|| "closed".into(), format_str);
    out!(
        "{state}  #{index}  {}/{duration}  output {output}",
        clock(s.position_ms)
    );
    if let Some(t) = &s.track {
        out!("{}", describe(t));
    }
}

/// "Artist - Title (Album)", falling back to the file name.
fn describe(t: &Track) -> String {
    let name = || {
        std::path::Path::new(&t.path)
            .file_name()
            .map_or_else(|| t.path.clone(), |n| n.to_string_lossy().into_owned())
    };
    let mut s = match (&t.artist, &t.title) {
        (Some(a), Some(ti)) => format!("{a} - {ti}"),
        (None, Some(ti)) => ti.clone(),
        _ => name(),
    };
    if let Some(album) = &t.album {
        s.push_str(&format!(" ({album})"));
    }
    s
}

/// Pad or cut `s` to exactly `width` characters.
fn fit(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len <= width {
        format!("{s}{}", " ".repeat(width - len))
    } else if width < 3 {
        s.chars().take(width).collect()
    } else {
        let cut: String = s.chars().take(width - 3).collect();
        format!("{cut}...")
    }
}

fn track_line(n: usize, t: &Track) -> String {
    let duration = t.duration_ms.map_or_else(|| "?".into(), clock);
    let format = t.format.as_ref().map_or_else(String::new, format_str);
    format!("{n:>4}  {}  {duration:>6}  {format}", fit(&describe(t), 60))
}

fn print_event(e: &Event) {
    match e {
        Event::State { state, position_ms } => {
            out!("state {state:?} at {}", clock(*position_ms))
        }
        Event::TrackChanged { index } => match index {
            Some(i) => out!("track #{}", i + 1),
            None => out!("track none"),
        },
        Event::QueueChanged => out!("queue changed"),
        Event::LibraryChanged { tracks } => out!("library {tracks} tracks"),
    }
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut json = false;
    let mut socket: Option<PathBuf> = None;
    while let Some(flag) = args.first().filter(|a| a.starts_with("--")).cloned() {
        args.remove(0);
        match flag.as_str() {
            "--json" => json = true,
            "--socket" if !args.is_empty() => socket = Some(PathBuf::from(args.remove(0))),
            "--help" => {
                out!("{USAGE}");
                return Ok(());
            }
            _ => bail!("unknown flag {flag}\n\n{USAGE}"),
        }
    }
    let command = parse(&args)?;
    let watch = command == Command::Subscribe;
    let socket = socket.unwrap_or_else(default_socket);
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to {} (is losslessd running?)", socket.display()))?;
    stream.write_all(encode(&Request { id: 1, command })?.as_bytes())?;

    for line in BufReader::new(stream).lines() {
        let line = line?;
        if json {
            out!("{line}");
        }
        match decode::<ServerMessage>(&line)? {
            ServerMessage::Reply { id: 1, result } => match result {
                Outcome::Err(e) => bail!("{e}"),
                Outcome::Ok(body) => {
                    if !json {
                        match body {
                            Body::Done => {}
                            Body::Status(s) => print_status(&s),
                            Body::Tracks(tracks) => {
                                for (i, t) in tracks.iter().enumerate() {
                                    out!("{}", track_line(i + 1, t));
                                    out!("      {}", t.path);
                                }
                            }
                            Body::Queue(tracks) => {
                                for (i, t) in tracks.iter().enumerate() {
                                    out!("{}", track_line(i + 1, t));
                                }
                            }
                        }
                    }
                    if !watch {
                        return Ok(());
                    }
                }
            },
            ServerMessage::Reply { .. } => {}
            ServerMessage::Event { event } => {
                if !json {
                    print_event(&event);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("90").unwrap(), 90_000);
        assert_eq!(parse_time("1:30").unwrap(), 90_000);
        assert_eq!(parse_time("1:00:05.5").unwrap(), 3_605_500);
        assert!(parse_time("x").is_err());
        for bad in ["inf", "-inf", "NaN", "1e400", "1:-5", "-0", "1::2", ""] {
            assert!(parse_time(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn fit_pads_and_cuts_by_chars() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("Earl\u{2019}s Court 2004", 10), "Earl\u{2019}s ...");
        assert_eq!(fit("Earl\u{2019}s Court 2004", 10).chars().count(), 10);
        assert_eq!(fit("abcdef", 2), "ab", "too narrow for an ellipsis");
        assert_eq!(fit("abcdef", 0), "");
    }

    #[test]
    fn play_is_one_based() {
        let args = |s: &[&str]| s.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse(&args(&["play", "1"])).unwrap(),
            Command::Play { index: Some(0) }
        );
        assert!(parse(&args(&["play", "0"])).is_err());
        assert_eq!(
            parse(&args(&["play"])).unwrap(),
            Command::Play { index: None }
        );
    }
}

#[cfg(test)]
mod props {
    use proptest::prelude::*;

    use super::{fit, parse, parse_time};

    proptest! {
        #[test]
        fn fit_is_always_exactly_the_width(s in ".{0,80}", width in 0usize..100) {
            prop_assert_eq!(fit(&s, width).chars().count(), width);
        }

        #[test]
        fn parse_time_reads_clock_times(h in 0u64..100, m in 0u64..60, s in 0u64..60) {
            let ms = ((h * 60 + m) * 60 + s) * 1000;
            prop_assert_eq!(parse_time(&format!("{h}:{m:02}:{s:02}")).unwrap(), ms);
            prop_assert_eq!(parse_time(&format!("{}", h * 3600 + m * 60 + s)).unwrap(), ms);
        }

        #[test]
        fn parse_time_never_panics(s in ".{0,30}") {
            let _ = parse_time(&s);
        }

        #[test]
        fn parsing_arguments_never_panics(args in proptest::collection::vec(".{0,12}", 0..5)) {
            let _ = parse(&args);
        }
    }
}
