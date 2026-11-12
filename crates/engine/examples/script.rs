use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use anyhow::Context;
use lossless_engine::{Command, Player};

/// Drive the player through a fixed scenario, printing status and events.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let device = args.next().context("usage: script <device> <file>...")?;
    let files: Vec<PathBuf> = args.map(PathBuf::from).collect();
    let (player, events) = Player::spawn(device);
    let printer = std::thread::spawn(move || {
        for ev in events {
            println!("  event {ev:?}");
        }
    });
    let show = |label: &str| {
        let s = player.snapshot().status;
        println!(
            "{label:<18} {:?} idx={:?} pos={}ms dur={:?} out={:?}",
            s.state, s.index, s.position_ms, s.duration_ms, s.output
        );
    };
    let step = |cmd: Command, wait: u64, label: &str| {
        player.send(cmd).expect("command failed");
        sleep(Duration::from_millis(wait));
        show(label);
    };
    player.send(Command::Add(files))?;
    step(Command::Play(None), 3000, "play 3s");
    step(Command::Pause, 1500, "pause 1.5s");
    step(Command::Play(None), 2000, "resume 2s");
    step(Command::Seek { ms: 60_000 }, 1500, "seek 60s +1.5s");
    step(Command::Next, 3000, "next 3s");
    step(Command::Previous, 2000, "previous 2s");
    step(Command::Play(Some(2)), 3000, "play idx 2, 3s");
    step(
        Command::Seek {
            ms: u64::MAX / 2000,
        },
        3000,
        "seek past end",
    );
    step(Command::Stop, 500, "stop");
    drop(player);
    printer.join().ok();
    Ok(())
}
