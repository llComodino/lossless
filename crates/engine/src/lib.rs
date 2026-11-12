#![forbid(unsafe_code)]

mod error;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
mod mixer;
mod output;
mod player;
mod queue;
mod source;

pub use error::{Error, Result};
pub use mixer::{HardwareMixer, Mixer, MixerKind, MixerSetting, SoftwareMixer, choose, watch};
pub use output::{Output, Sink};
pub use player::{Command, Player, Snapshot};
pub use source::Source;
