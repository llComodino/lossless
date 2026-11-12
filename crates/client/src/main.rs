#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use lossless_proto::{
    Body, Command, Event, Outcome, PlayState, Repeat, Request, ServerMessage, Status, StreamFormat,
    Track, decode, default_socket, encode,
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
  add [--at N] <path>... add files or folders (relative to music_dir)
  replace <path>...      replace the queue and stop
  remove <N>...          remove queue entries (1-based)
  move <from> <to>       move a queue entry
  play [N]               start/resume, or jump to queue entry N (1-based)
  pause | toggle | stop
  next | prev
  seek <[[h:]m:]s>       seek within the current track
  clear                  stop and empty the queue
  repeat off|queue|one   repeat nothing, the queue or the current track
  random on|off          play the queue in random order
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

/// A 1-based queue position as a wire index.
fn position(s: &str) -> Result<usize> {
    let n: usize = s.parse().with_context(|| format!("bad position {s:?}"))?;
    n.checked_sub(1)
        .ok_or_else(|| anyhow!("positions start at 1"))
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
        "add" => {
            let (at, paths) = match rest {
                [flag, n, paths @ ..] if flag == "--at" => (Some(position(n)?), paths),
                _ => (None, rest),
            };
            if paths.is_empty() {
                bail!(USAGE);
            }
            Ok(Command::Add {
                paths: paths.to_vec(),
                at,
            })
        }
        "replace" if !rest.is_empty() => Ok(Command::Replace {
            paths: rest.to_vec(),
        }),
        "remove" if !rest.is_empty() => Ok(Command::Remove {
            indices: rest.iter().map(|n| position(n)).collect::<Result<_>>()?,
        }),
        "move" => match rest {
            [from, to] => Ok(Command::Move {
                from: position(from)?,
                to: position(to)?,
            }),
            _ => bail!("move takes two positions"),
        },
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
        "repeat" => {
            let repeat = match rest {
                [m] if m == "off" => Repeat::Off,
                [m] if m == "queue" => Repeat::Queue,
                [m] if m == "one" => Repeat::One,
                _ => bail!("repeat takes off, queue or one"),
            };
            Ok(Command::SetMode {
                repeat: Some(repeat),
                random: None,
            })
        }
        "random" => {
            let random = match rest {
                [m] if m == "on" => true,
                [m] if m == "off" => false,
                _ => bail!("random takes on or off"),
            };
            Ok(Command::SetMode {
                repeat: None,
                random: Some(random),
            })
        }
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

fn modes(repeat: Repeat, random: bool) -> String {
    let repeat = match repeat {
        Repeat::Off => "off",
        Repeat::Queue => "queue",
        Repeat::One => "one",
    };
    let random = if random { "on" } else { "off" };
    format!("repeat:{repeat} random:{random}")
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
        "{state}  #{index}  {}/{duration}  output {output} {}",
        clock(s.position_ms),
        modes(s.repeat, s.random)
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
        Event::Mode { repeat, random } => out!("mode {}", modes(*repeat, *random)),
        Event::LibraryChanged { tracks } => out!("library {tracks} tracks"),
        Event::Error(e) => out!("error: {}", e.message),
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
                Outcome::Err(e) => bail!("{}", e.message),
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

    fn track(path: &str, artist: Option<&str>, title: Option<&str>, album: Option<&str>) -> Track {
        Track {
            path: path.into(),
            title: title.map(Into::into),
            artist: artist.map(Into::into),
            album: album.map(Into::into),
            track_number: None,
            disc_number: None,
            duration_ms: None,
            format: None,
        }
    }

    #[test]
    fn clock_shows_hours_only_when_needed() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(59_999), "0:59");
        assert_eq!(clock(3_599_999), "59:59");
        assert_eq!(clock(3_600_000), "1:00:00");
        assert_eq!(clock(36_061_000), "10:01:01");
    }

    #[test]
    fn formats_read_as_bits_rate_channels() {
        let f = |rate, bits| {
            format_str(&StreamFormat {
                rate,
                bits,
                channels: 2,
            })
        };
        assert_eq!(f(44_100, 16), "16-bit/44.1 kHz/2ch");
        assert_eq!(f(192_000, 24), "24-bit/192 kHz/2ch");
    }

    #[test]
    fn describe_falls_back_to_the_file_name() {
        let p = "/m/a/01 x.flac";
        assert_eq!(
            describe(&track(p, Some("A"), Some("T"), Some("L"))),
            "A - T (L)"
        );
        assert_eq!(describe(&track(p, None, Some("T"), None)), "T");
        assert_eq!(describe(&track(p, Some("A"), None, None)), "01 x.flac");
        assert_eq!(describe(&track(p, None, None, Some("L"))), "01 x.flac (L)");
        assert_eq!(describe(&track("/", None, None, None)), "/");
    }

    #[test]
    fn track_lines_have_fixed_columns() {
        let mut t = track("/m/a.flac", Some("A"), Some("T"), None);
        assert_eq!(
            track_line(3, &t),
            format!("   3  {}       ?  ", fit("A - T", 60))
        );
        t.duration_ms = Some(61_000);
        t.format = Some(StreamFormat {
            rate: 96_000,
            bits: 24,
            channels: 2,
        });
        assert_eq!(
            track_line(12, &t),
            format!("  12  {}    1:01  24-bit/96 kHz/2ch", fit("A - T", 60))
        );
    }

    #[test]
    fn commands_and_their_arguments() {
        let args = |s: &[&str]| s.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let ok = |s: &[&str]| parse(&args(s)).unwrap();
        let err = |s: &[&str]| parse(&args(s)).unwrap_err().to_string();
        assert_eq!(ok(&["previous"]), Command::Previous);
        assert_eq!(ok(&["prev"]), Command::Previous);
        assert_eq!(ok(&["watch"]), Command::Subscribe);
        assert_eq!(ok(&["seek", "1:30"]), Command::Seek { ms: 90_000 });
        assert_eq!(
            ok(&["search", "a", "b"]),
            Command::Search {
                query: "a b".into()
            }
        );
        assert_eq!(
            ok(&["add", "x", "y"]),
            Command::Add {
                paths: vec!["x".into(), "y".into()],
                at: None
            }
        );
        assert_eq!(
            ok(&["add", "--at", "1", "x"]),
            Command::Add {
                paths: vec!["x".into()],
                at: Some(0)
            }
        );
        assert_eq!(
            ok(&["replace", "x"]),
            Command::Replace {
                paths: vec!["x".into()]
            }
        );
        assert_eq!(
            ok(&["remove", "2", "1"]),
            Command::Remove {
                indices: vec![1, 0]
            }
        );
        assert_eq!(ok(&["move", "1", "3"]), Command::Move { from: 0, to: 2 });
        assert_eq!(err(&["move", "1"]), "move takes two positions");
        assert!(err(&["remove", "x"]).contains("bad position"));
        assert_eq!(err(&["status", "x"]), "status takes no arguments");
        assert_eq!(err(&["seek"]), "seek takes one time");
        assert_eq!(err(&["play", "1", "2"]), "play takes at most one index");
        assert!(err(&["play", "x"]).contains("bad index"));
        for usage in [
            &[][..],
            &["add"],
            &["add", "--at", "1"],
            &["replace"],
            &["remove"],
            &["search"],
            &["bogus"],
        ] {
            assert_eq!(err(usage), USAGE, "{usage:?}");
        }
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
