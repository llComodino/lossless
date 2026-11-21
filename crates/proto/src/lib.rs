#![forbid(unsafe_code)]

//! Wire protocol: one JSON object per line over a unix socket.
//! Clients send `Request`s; the server sends `ServerMessage`s (replies, and
//! events once the client has sent `Subscribe`).

mod display;
pub mod profile;

pub use display::{clock, describe, format_str, rate_str};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    #[default]
    Off,
    Queue,
    One,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Status,
    Queue,
    /// Start or resume; with an index, jump to that queue entry.
    Play {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek {
        ms: u64,
    },
    /// Append, or insert at a 0-based queue index.
    Add {
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<usize>,
    },
    /// Swap the whole queue and play from its first entry.
    Replace {
        paths: Vec<String>,
    },
    Remove {
        indices: Vec<usize>,
    },
    Move {
        from: usize,
        to: usize,
    },
    Clear,
    /// Omitted fields are left as they are.
    SetMode {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repeat: Option<Repeat>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        random: Option<bool>,
    },
    Subscribe,
    /// Tracks whose artist, album, title or path contain every word.
    Search {
        query: String,
    },
    /// Rescan the music directory.
    Update,
    /// Folders and tracks directly under a path relative to the music
    /// directory (empty for the top).
    Browse {
        #[serde(default)]
        path: String,
    },
    Artists,
    /// Album names of one artist, or of all.
    Albums {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artist: Option<String>,
    },
    /// Tracks matching an exact artist and/or album, case-insensitively.
    Find {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artist: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        album: Option<String>,
    },
    /// 0 to 100; `no_mixer` when the device has none.
    SetVolume {
        percent: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Reply {
        id: u64,
        #[serde(flatten)]
        result: Outcome,
    },
    Event {
        #[serde(flatten)]
        event: Event,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(Body),
    Err(WireError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    OutOfRange,
    NoMixer,
    NoMusicDir,
    ScanRunning,
    Device,
    Decode,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Body {
    Done,
    Status(Box<Status>),
    Queue(Vec<Track>),
    Tracks(Vec<Track>),
    Entries(Vec<Entry>),
    Names(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Entry {
    Dir { name: String, path: String },
    Track(Track),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Sent on every state change and seek. While playing, clients advance
    /// `position_ms` with their own clock: the server sends no ticks.
    State {
        state: PlayState,
        position_ms: u64,
    },
    TrackChanged {
        index: Option<usize>,
    },
    QueueChanged,
    Mode {
        repeat: Repeat,
        random: bool,
    },
    LibraryChanged {
        tracks: usize,
    },
    /// Sent on every `set_volume` and on changes made outside the daemon.
    Volume {
        percent: u8,
    },
    Error(WireError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub state: PlayState,
    pub index: Option<usize>,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    /// What the output device was actually opened with.
    pub output: Option<StreamFormat>,
    /// The current queue entry, with tags when the library knows it.
    #[serde(default)]
    pub track: Option<Track>,
    /// The last failure the daemon recovered from on its own (a skipped
    /// file, a device that stopped playback), until a track loads cleanly.
    #[serde(default)]
    pub last_error: Option<WireError>,
    #[serde(default)]
    pub repeat: Repeat,
    #[serde(default)]
    pub random: bool,
    /// `None` without a mixer.
    #[serde(default)]
    pub volume: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFormat {
    pub rate: u32,
    pub bits: u16,
    pub channels: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    #[serde(default)]
    pub track_number: Option<u32>,
    #[serde(default)]
    pub disc_number: Option<u32>,
    pub duration_ms: Option<u64>,
    pub format: Option<StreamFormat>,
}

/// `$XDG_RUNTIME_DIR/lossless.sock`, else `/tmp/lossless-$USER.sock`.
pub fn default_socket() -> std::path::PathBuf {
    profile::Dirs::from_env().socket(&profile::Profile::default())
}

/// Serialize as one protocol line, newline included.
pub fn encode<T: Serialize>(msg: &T) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        let req = Request {
            id: 7,
            command: Command::Seek { ms: 1500 },
        };
        assert_eq!(
            encode(&req).unwrap(),
            "{\"id\":7,\"cmd\":\"seek\",\"ms\":1500}\n"
        );
        assert_eq!(
            decode::<Request>("{\"id\":1,\"cmd\":\"play\"}\n")
                .unwrap()
                .command,
            Command::Play { index: None }
        );
        let jump = Request {
            id: 2,
            command: Command::Play { index: Some(3) },
        };
        assert_eq!(
            encode(&jump).unwrap(),
            "{\"id\":2,\"cmd\":\"play\",\"index\":3}\n"
        );
        let add = |at| Request {
            id: 1,
            command: Command::Add {
                paths: vec!["a".into()],
                at,
            },
        };
        assert_eq!(
            encode(&add(Some(0))).unwrap(),
            "{\"id\":1,\"cmd\":\"add\",\"paths\":[\"a\"],\"at\":0}\n"
        );
        assert_eq!(
            encode(&add(None)).unwrap(),
            "{\"id\":1,\"cmd\":\"add\",\"paths\":[\"a\"]}\n"
        );
        assert_eq!(
            decode::<Request>("{\"id\":1,\"cmd\":\"add\",\"paths\":[\"a\"]}").unwrap(),
            add(None)
        );
        assert_eq!(
            encode(&Request {
                id: 3,
                command: Command::Move { from: 1, to: 0 }
            })
            .unwrap(),
            "{\"id\":3,\"cmd\":\"move\",\"from\":1,\"to\":0}\n"
        );
        assert_eq!(
            encode(&Request {
                id: 4,
                command: Command::SetMode {
                    repeat: Some(Repeat::One),
                    random: None
                }
            })
            .unwrap(),
            "{\"id\":4,\"cmd\":\"set_mode\",\"repeat\":\"one\"}\n"
        );
        assert_eq!(
            decode::<Request>("{\"id\":5,\"cmd\":\"set_mode\",\"random\":true}")
                .unwrap()
                .command,
            Command::SetMode {
                repeat: None,
                random: Some(true)
            }
        );
    }

    #[test]
    fn listings_on_the_wire() {
        let line = |command| encode(&Request { id: 1, command }).unwrap();
        assert_eq!(
            line(Command::Browse { path: "a/b".into() }),
            "{\"id\":1,\"cmd\":\"browse\",\"path\":\"a/b\"}\n"
        );
        assert_eq!(line(Command::Artists), "{\"id\":1,\"cmd\":\"artists\"}\n");
        assert_eq!(
            line(Command::Albums { artist: None }),
            "{\"id\":1,\"cmd\":\"albums\"}\n"
        );
        assert_eq!(
            line(Command::Find {
                artist: Some("A".into()),
                album: None
            }),
            "{\"id\":1,\"cmd\":\"find\",\"artist\":\"A\"}\n"
        );
        assert_eq!(
            decode::<Request>("{\"id\":2,\"cmd\":\"browse\"}")
                .unwrap()
                .command,
            Command::Browse {
                path: String::new()
            }
        );
        let reply = |body| {
            encode(&ServerMessage::Reply {
                id: 4,
                result: Outcome::Ok(body),
            })
            .unwrap()
        };
        assert_eq!(
            reply(Body::Entries(vec![Entry::Dir {
                name: "x".into(),
                path: "x".into()
            }])),
            "{\"type\":\"reply\",\"id\":4,\"ok\":{\"kind\":\"entries\",\"data\":[{\"kind\":\"dir\",\"data\":{\"name\":\"x\",\"path\":\"x\"}}]}}\n"
        );
        assert_eq!(
            reply(Body::Names(vec!["n".into()])),
            "{\"type\":\"reply\",\"id\":4,\"ok\":{\"kind\":\"names\",\"data\":[\"n\"]}}\n"
        );
    }

    #[test]
    fn modes_on_the_wire() {
        let ev = ServerMessage::Event {
            event: Event::Mode {
                repeat: Repeat::Queue,
                random: true,
            },
        };
        assert_eq!(
            encode(&ev).unwrap(),
            "{\"type\":\"event\",\"event\":\"mode\",\"repeat\":\"queue\",\"random\":true}\n"
        );
        let old =
            r#"{"state":"stopped","index":null,"position_ms":0,"duration_ms":null,"output":null}"#;
        let st: Status = decode(old).unwrap();
        assert_eq!((st.repeat, st.random), (Repeat::Off, false));
    }

    #[test]
    fn volume_on_the_wire() {
        let req = Request {
            id: 6,
            command: Command::SetVolume { percent: 40 },
        };
        assert_eq!(
            encode(&req).unwrap(),
            "{\"id\":6,\"cmd\":\"set_volume\",\"percent\":40}\n"
        );
        let ev = ServerMessage::Event {
            event: Event::Volume { percent: 40 },
        };
        assert_eq!(
            encode(&ev).unwrap(),
            "{\"type\":\"event\",\"event\":\"volume\",\"percent\":40}\n"
        );
        let old =
            r#"{"state":"stopped","index":null,"position_ms":0,"duration_ms":null,"output":null}"#;
        assert_eq!(decode::<Status>(old).unwrap().volume, None);
    }

    #[test]
    fn reply_and_event_wire_format() {
        let ok = ServerMessage::Reply {
            id: 7,
            result: Outcome::Ok(Body::Done),
        };
        assert_eq!(
            encode(&ok).unwrap(),
            "{\"type\":\"reply\",\"id\":7,\"ok\":{\"kind\":\"done\"}}\n"
        );
        let ev = ServerMessage::Event {
            event: Event::State {
                state: PlayState::Playing,
                position_ms: 1500,
            },
        };
        assert_eq!(
            encode(&ev).unwrap(),
            "{\"type\":\"event\",\"event\":\"state\",\"state\":\"playing\",\"position_ms\":1500}\n"
        );
        assert_eq!(decode::<ServerMessage>(&encode(&ev).unwrap()).unwrap(), ev);
    }

    #[test]
    fn errors_on_the_wire() {
        let m = ServerMessage::Reply {
            id: 3,
            result: Outcome::Err(WireError {
                code: ErrorCode::OutOfRange,
                message: "index 14 out of range: queue has 12 entries".into(),
            }),
        };
        assert_eq!(
            encode(&m).unwrap(),
            r#"{"type":"reply","id":3,"err":{"code":"out_of_range","message":"index 14 out of range: queue has 12 entries"}}"#.to_owned() + "\n"
        );
        let e = ServerMessage::Event {
            event: Event::Error(WireError {
                code: ErrorCode::Decode,
                message: "x.flac: bad frame".into(),
            }),
        };
        assert_eq!(
            encode(&e).unwrap(),
            r#"{"type":"event","event":"error","code":"decode","message":"x.flac: bad frame"}"#
                .to_owned()
                + "\n"
        );
        assert_eq!(decode::<ServerMessage>(&encode(&e).unwrap()).unwrap(), e);
        assert_eq!(
            WireError {
                code: ErrorCode::Io,
                message: "boom".into()
            }
            .to_string(),
            "boom"
        );
    }
}

#[cfg(test)]
mod props {
    use proptest::option::of;
    use proptest::prelude::*;

    use super::*;

    fn format() -> impl Strategy<Value = StreamFormat> {
        (any::<u32>(), any::<u16>(), any::<u16>()).prop_map(|(rate, bits, channels)| StreamFormat {
            rate,
            bits,
            channels,
        })
    }

    fn track() -> impl Strategy<Value = Track> {
        (
            any::<String>(),
            of(any::<String>()),
            of(any::<String>()),
            of(any::<String>()),
            of(any::<u32>()),
            of(any::<u32>()),
            of(any::<u64>()),
            of(format()),
        )
            .prop_map(
                |(path, title, artist, album, track_number, disc_number, duration_ms, format)| {
                    Track {
                        path,
                        title,
                        artist,
                        album,
                        track_number,
                        disc_number,
                        duration_ms,
                        format,
                    }
                },
            )
    }

    fn entry() -> impl Strategy<Value = Entry> {
        prop_oneof![
            (any::<String>(), any::<String>()).prop_map(|(name, path)| Entry::Dir { name, path }),
            track().prop_map(Entry::Track),
        ]
    }

    fn state() -> impl Strategy<Value = PlayState> {
        prop_oneof![
            Just(PlayState::Stopped),
            Just(PlayState::Playing),
            Just(PlayState::Paused)
        ]
    }

    fn repeat() -> impl Strategy<Value = Repeat> {
        prop_oneof![Just(Repeat::Off), Just(Repeat::Queue), Just(Repeat::One)]
    }

    fn command() -> impl Strategy<Value = Command> {
        prop_oneof![
            Just(Command::Status),
            Just(Command::Queue),
            of(any::<usize>()).prop_map(|index| Command::Play { index }),
            Just(Command::Pause),
            Just(Command::Toggle),
            Just(Command::Stop),
            Just(Command::Next),
            Just(Command::Previous),
            any::<u64>().prop_map(|ms| Command::Seek { ms }),
            (
                proptest::collection::vec(any::<String>(), 0..4),
                of(any::<usize>())
            )
                .prop_map(|(paths, at)| Command::Add { paths, at }),
            proptest::collection::vec(any::<String>(), 0..4)
                .prop_map(|paths| Command::Replace { paths }),
            proptest::collection::vec(any::<usize>(), 0..4)
                .prop_map(|indices| Command::Remove { indices }),
            (any::<usize>(), any::<usize>()).prop_map(|(from, to)| Command::Move { from, to }),
            Just(Command::Clear),
            (of(repeat()), of(any::<bool>()))
                .prop_map(|(repeat, random)| Command::SetMode { repeat, random }),
            Just(Command::Subscribe),
            any::<String>().prop_map(|query| Command::Search { query }),
            Just(Command::Update),
            any::<String>().prop_map(|path| Command::Browse { path }),
            Just(Command::Artists),
            of(any::<String>()).prop_map(|artist| Command::Albums { artist }),
            (of(any::<String>()), of(any::<String>()))
                .prop_map(|(artist, album)| Command::Find { artist, album }),
            any::<u8>().prop_map(|percent| Command::SetVolume { percent }),
        ]
    }

    fn status() -> impl Strategy<Value = Status> {
        (
            state(),
            of(any::<usize>()),
            any::<u64>(),
            of(any::<u64>()),
            of(format()),
            of(track()),
            of(wire_error()),
            repeat(),
            any::<bool>(),
            of(any::<u8>()),
        )
            .prop_map(
                |(
                    state,
                    index,
                    position_ms,
                    duration_ms,
                    output,
                    track,
                    last_error,
                    repeat,
                    random,
                    volume,
                )| {
                    Status {
                        state,
                        index,
                        position_ms,
                        duration_ms,
                        output,
                        track,
                        last_error,
                        repeat,
                        random,
                        volume,
                    }
                },
            )
    }

    fn wire_error() -> impl Strategy<Value = WireError> {
        let code = prop_oneof![
            Just(ErrorCode::BadRequest),
            Just(ErrorCode::NotFound),
            Just(ErrorCode::OutOfRange),
            Just(ErrorCode::NoMixer),
            Just(ErrorCode::NoMusicDir),
            Just(ErrorCode::ScanRunning),
            Just(ErrorCode::Device),
            Just(ErrorCode::Decode),
            Just(ErrorCode::Io),
        ];
        (code, any::<String>()).prop_map(|(code, message)| WireError { code, message })
    }

    fn event() -> impl Strategy<Value = Event> {
        prop_oneof![
            (state(), any::<u64>())
                .prop_map(|(state, position_ms)| Event::State { state, position_ms }),
            of(any::<usize>()).prop_map(|index| Event::TrackChanged { index }),
            Just(Event::QueueChanged),
            (repeat(), any::<bool>()).prop_map(|(repeat, random)| Event::Mode { repeat, random }),
            any::<usize>().prop_map(|tracks| Event::LibraryChanged { tracks }),
            any::<u8>().prop_map(|percent| Event::Volume { percent }),
            wire_error().prop_map(Event::Error),
        ]
    }

    fn message() -> impl Strategy<Value = ServerMessage> {
        let body = prop_oneof![
            Just(Body::Done),
            status().prop_map(|s| Body::Status(Box::new(s))),
            proptest::collection::vec(track(), 0..3).prop_map(Body::Queue),
            proptest::collection::vec(track(), 0..3).prop_map(Body::Tracks),
            proptest::collection::vec(entry(), 0..3).prop_map(Body::Entries),
            proptest::collection::vec(any::<String>(), 0..3).prop_map(Body::Names),
        ];
        let outcome = prop_oneof![
            body.prop_map(Outcome::Ok),
            wire_error().prop_map(Outcome::Err)
        ];
        prop_oneof![
            (any::<u64>(), outcome).prop_map(|(id, result)| ServerMessage::Reply { id, result }),
            event().prop_map(|event| ServerMessage::Event { event }),
        ]
    }

    /// A line-delimited protocol breaks if a message spans lines: strings
    /// with newlines must come out escaped.
    fn one_line(line: &str) -> bool {
        line.ends_with('\n') && line.matches('\n').count() == 1
    }

    proptest! {
        #[test]
        fn requests_round_trip(id in any::<u64>(), command in command()) {
            let req = Request { id, command };
            let line = encode(&req).unwrap();
            prop_assert!(one_line(&line));
            prop_assert_eq!(decode::<Request>(&line).unwrap(), req);
        }

        #[test]
        fn server_messages_round_trip(msg in message()) {
            let line = encode(&msg).unwrap();
            prop_assert!(one_line(&line));
            prop_assert_eq!(decode::<ServerMessage>(&line).unwrap(), msg);
        }

        #[test]
        fn decoding_garbage_never_panics(line in any::<String>()) {
            let _ = decode::<Request>(&line);
            let _ = decode::<ServerMessage>(&line);
        }

        #[test]
        fn decoding_json_shaped_garbage_never_panics(
            cmd in "[a-z_]{0,12}",
            extra in "[ -~]{0,40}",
        ) {
            let _ = decode::<Request>(&format!("{{\"id\":1,\"cmd\":\"{cmd}\"{extra}}}"));
        }
    }
}

/// Pad or cut `s` to exactly `width` terminal columns; a wide character
/// that does not fit is replaced by a space.
pub fn fit(s: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let total: usize = s.graphemes(true).map(|g| g.width()).sum();
    let ellipsis = total > width && width >= 3;
    let limit = if ellipsis { width - 3 } else { width };
    let mut out = String::new();
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if used + w > limit || (limit == 0 && w == 0) {
            break;
        }
        used += w;
        out.push_str(g);
    }
    if ellipsis {
        out.push_str("...");
        used += 3;
    }
    out.push_str(&" ".repeat(width - used));
    out
}

#[cfg(test)]
mod fit_tests {
    use super::fit;
    use unicode_width::UnicodeWidthStr;

    proptest::proptest! {
        #[test]
        fn fit_is_always_exactly_the_width(s in ".{0,80}", width in 0usize..100) {
            proptest::prop_assert_eq!(fit(&s, width).width(), width);
        }
    }

    #[test]
    fn fit_pads_and_cuts_by_columns() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("Earl\u{2019}s Court 2004", 10), "Earl\u{2019}s ...");
        assert_eq!(fit("abcdef", 2), "ab", "too narrow for an ellipsis");
        assert_eq!(fit("abcdef", 0), "");
        assert_eq!(fit("abcdef", 6), "abcdef");
    }

    #[test]
    fn fit_counts_wide_and_combining() {
        assert_eq!(fit("米津玄師", 8), "米津玄師");
        assert_eq!(fit("米津玄師", 10), "米津玄師  ");
        assert_eq!(fit("米津玄師", 7), "米津...", "cut on a column boundary");
        assert_eq!(
            fit("米津玄師", 6),
            "米... ",
            "wide char does not fit: space"
        );
        assert_eq!(fit("米津玄師", 1), " ");
        assert_eq!(fit("a米", 2), "a ");
        assert_eq!(fit("🎵🎵🎵", 6), "🎵🎵🎵");
        assert_eq!(fit("🎵🎵🎵🎵", 7), "🎵🎵...");
        assert_eq!(fit("e\u{301}x", 2), "e\u{301}x");
        assert_eq!(fit("e\u{301}", 3), "e\u{301}  ");
        assert_eq!(fit("\u{2764}\u{fe0f}", 2), "\u{2764}\u{fe0f}");
        assert_eq!(fit("\u{2764}\u{fe0f}", 4), "\u{2764}\u{fe0f}  ");
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(fit(family, 2), family);
        assert_eq!(fit(family, 5), format!("{family}   "));
        let flag = "\u{1f1ef}\u{1f1f5}";
        assert_eq!(fit(flag, 2), flag);
        assert_eq!(fit(&format!("{flag}{flag}"), 3), "...");
        for w in 0..12 {
            for s in [
                "",
                "abc",
                "米津玄師 - IRIS OUT",
                "🎵 e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
                "a米b米c米d米",
            ] {
                assert_eq!(fit(s, w).width(), w, "{s:?} at {w}");
            }
        }
    }
}
