use std::io::{self, Write};
use std::process::Command;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::enable_raw_mode;

use super::{Run, modal::selection_file, status_text};
use crate::term;

/// Runs `run` on the real terminal and waits for it. A failure is left on
/// screen until a key is pressed. The caller redraws afterwards.
pub fn run(run: &Run) -> io::Result<()> {
    let selection = selection_file(run)?;
    term::suspend();
    let status = Command::new("sh")
        .arg("-c")
        .arg(&run.command)
        .envs(run.env.iter().map(|(k, v)| (k, v)))
        .env("LOSSLESS_SELECTION", &selection)
        .status();
    let _ = std::fs::remove_file(&selection);
    let result = match status {
        Ok(st) if !st.success() => {
            print!("\r\n[{}, press any key]", status_text(st));
            let _ = io::stdout().flush();
            let _ = enable_raw_mode();
            while let Ok(e) = event::read() {
                if matches!(e, Event::Key(k) if k.kind != KeyEventKind::Release) {
                    break;
                }
            }
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    };
    term::resume()?;
    result
}
