#![forbid(unsafe_code)]

mod term;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use lossless_proto::profile::{Dirs, Profile};

const USAGE: &str = "usage: losslessd-tui [-p NAME | --socket PATH]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut socket: Option<PathBuf> = None;
    let mut profile = Profile::default();
    while let Some(flag) = args.next() {
        match (flag.as_str(), args.next()) {
            ("--help", _) => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            ("--socket", Some(path)) => socket = Some(PathBuf::from(path)),
            ("-p" | "--profile", Some(name)) => match Profile::parse(&name) {
                Ok(p) => profile = p,
                Err(e) => {
                    eprintln!("losslessd-tui: {e}");
                    return ExitCode::from(2);
                }
            },
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let path = socket.unwrap_or_else(|| Dirs::from_env().socket(&profile));
    let _stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "losslessd-tui: connecting to {} (is losslessd running?)",
                path.display()
            );
            return ExitCode::from(1);
        }
    };
    term::install_panic_hook();
    match term::Guard::new() {
        Ok(_guard) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("losslessd-tui: terminal: {e}");
            ExitCode::from(1)
        }
    }
}
