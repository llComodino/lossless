#![forbid(unsafe_code)]

use std::process::ExitCode;

use lossless_daemon::LogLevel;
use lossless_proto::profile::{Dirs, Profile};

const USAGE: &str = "usage: losslessd [--profile NAME] [--log-level off|warn|info|debug]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut level = None;
    let mut profile = None;
    while let Some(flag) = args.next() {
        let value = args.next();
        match (flag.as_str(), value) {
            ("--log-level", Some(v)) if level.is_none() => match v.parse::<LogLevel>() {
                Ok(l) => level = Some(l),
                Err(_) => return usage(),
            },
            ("--profile", Some(v)) if profile.is_none() => match Profile::parse(&v) {
                Ok(p) => profile = Some(p),
                Err(e) => {
                    eprintln!("losslessd: {e}");
                    return ExitCode::from(2);
                }
            },
            _ => return usage(),
        }
    }
    let profile = profile.unwrap_or_default();
    let dirs = Dirs::from_env();
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
