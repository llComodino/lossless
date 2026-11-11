#![forbid(unsafe_code)]

//! Wire protocol: one JSON object per line over a unix socket.
//! Clients send `Request`s; the server sends `ServerMessage`s (replies, and
//! events once the client has sent `Subscribe`).

use serde::{Deserialize, Serialize};

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
    Add {
        paths: Vec<String>,
    },
    Clear,
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
    Err(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Body {
    Done,
    Status(Status),
    Queue(Vec<Track>),
    Tracks(Vec<Track>),
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
    LibraryChanged {
        tracks: usize,
    },
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
        let err = ServerMessage::Reply {
            id: 8,
            result: Outcome::Err("no such file".into()),
        };
        assert_eq!(
            encode(&err).unwrap(),
            "{\"type\":\"reply\",\"id\":8,\"err\":\"no such file\"}\n"
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
            of(any::<u64>()),
            of(format()),
        )
            .prop_map(
                |(path, title, artist, album, track_number, duration_ms, format)| Track {
                    path,
                    title,
                    artist,
                    album,
                    track_number,
                    duration_ms,
                    format,
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
            proptest::collection::vec(any::<String>(), 0..4)
                .prop_map(|paths| Command::Add { paths }),
            Just(Command::Clear),
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
        )
            .prop_map(
                |(state, index, position_ms, duration_ms, output, track)| Status {
                    state,
                    index,
                    position_ms,
                    duration_ms,
                    output,
                    track,
                },
            )
    }

    fn event() -> impl Strategy<Value = Event> {
        prop_oneof![
            (state(), any::<u64>())
                .prop_map(|(state, position_ms)| Event::State { state, position_ms }),
            of(any::<usize>()).prop_map(|index| Event::TrackChanged { index }),
            Just(Event::QueueChanged),
            any::<usize>().prop_map(|tracks| Event::LibraryChanged { tracks }),
        ]
    }

    fn message() -> impl Strategy<Value = ServerMessage> {
        let body = prop_oneof![
            Just(Body::Done),
            status().prop_map(Body::Status),
            proptest::collection::vec(track(), 0..3).prop_map(Body::Queue),
            proptest::collection::vec(track(), 0..3).prop_map(Body::Tracks),
        ];
        let outcome = prop_oneof![
            body.prop_map(Outcome::Ok),
            any::<String>().prop_map(Outcome::Err)
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
