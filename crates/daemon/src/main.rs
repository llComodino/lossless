#![forbid(unsafe_code)]

use anyhow::Result;
use lossless_daemon::{Daemon, bind, config_path, load_config};
use lossless_engine::Player;
use lossless_proto::default_socket;

fn main() -> Result<()> {
    let cfg = load_config(&config_path())?;
    let socket = cfg.socket.clone().unwrap_or_else(default_socket);
    let listener = bind(&socket)?;
    eprintln!(
        "losslessd: device {} socket {}",
        cfg.device,
        socket.display()
    );
    let (player, events) = Player::spawn(cfg.device);
    let daemon = Daemon::start(player, events, cfg.music_dir)?;
    if let Err(e) = daemon.rescan() {
        eprintln!("library: {e}");
    }
    daemon.serve(listener);
    Ok(())
}
