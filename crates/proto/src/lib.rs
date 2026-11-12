#![forbid(unsafe_code)]

//! Wire protocol: one JSON object per line over a unix socket.
//! Clients send `Request`s; the server sends `ServerMessage`s (replies, and
//! events once the client has sent `Subscribe`).

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
    /// Swap the whole queue; stops playback.
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
    #[serde(default)]
    pub last_error: Option<WireError>,
    #[serde(default)]
    pub repeat: Repeat,
    #[serde(default)]
    pub random: bool,
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
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => std::path::Path::new(&dir).join("lossless.sock"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("lossless-{user}.sock"))
        }
    }
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
            wire_error().prop_map(Event::Error),
        ]
    }

    fn message() -> impl Strategy<Value = ServerMessage> {
        let body = prop_oneof![
            Just(Body::Done),
            status().prop_map(|s| Body::Status(Box::new(s))),
            proptest::collection::vec(track(), 0..3).prop_map(Body::Queue),
            proptest::collection::vec(track(), 0..3).prop_map(Body::Tracks),
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
