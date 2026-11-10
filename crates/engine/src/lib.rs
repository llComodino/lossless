#![forbid(unsafe_code)]

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
mod output;
mod player;
mod source;

pub use output::{Output, Sink};
pub use player::{Command, Player, Snapshot};
pub use source::Source;
