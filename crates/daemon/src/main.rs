#![forbid(unsafe_code)]

use std::process::ExitCode;

use lossless_daemon::LogLevel;
use lossless_proto::profile::{Dirs, Profile};

const USAGE: &str = "usage: losslessd [--log-level off|warn|info|debug]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let level = match args.next().as_deref() {
        None => None,
        Some("--log-level") => match args.next().map(|v| v.parse::<LogLevel>()) {
            Some(Ok(level)) => Some(level),
            _ => return usage(),
        },
        Some(_) => return usage(),
    };
    if args.next().is_some() {
        return usage();
    }
    let dirs = Dirs::from_env();
    let profile = Profile::default();
    lossless_daemon::run(
        &dirs.config_path(&profile),
        dirs.socket(&profile),
        dirs.state_path(&profile),
        level,
    )
    .map_or_else(fatal, |()| ExitCode::SUCCESS)
}

fn fatal(err: anyhow::Error) -> ExitCode {
    eprintln!("losslessd: {err:#}");
    ExitCode::FAILURE
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
