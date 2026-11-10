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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    State { state: PlayState },
    TrackChanged { index: Option<usize> },
    QueueChanged,
    Position { ms: u64 },
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
            },
        };
        assert_eq!(
            encode(&ev).unwrap(),
            "{\"type\":\"event\",\"event\":\"state\",\"state\":\"playing\"}\n"
        );
        assert_eq!(decode::<ServerMessage>(&encode(&ev).unwrap()).unwrap(), ev);
    }
}
