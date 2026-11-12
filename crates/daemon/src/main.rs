#![forbid(unsafe_code)]

use std::process::ExitCode;

use lossless_daemon::LogLevel;

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
    lossless_daemon::run(
        &lossless_daemon::config_path(),
        lossless_proto::default_socket(),
        lossless_daemon::state::state_path(),
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
