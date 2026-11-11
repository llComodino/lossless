use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// ALSA device, e.g. "hw:CARD=PRO2,DEV=0" for bit-perfect, "default" for
    /// the PipeWire path.
    #[serde(default = "default_device")]
    pub device: String,
    /// Relative paths in `add` resolve against this.
    pub music_dir: Option<PathBuf>,
    pub socket: Option<PathBuf>,
}

fn default_device() -> String {
    "default".into()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

pub(crate) fn expand_tilde(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

pub fn config_path() -> PathBuf {
    let base =
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home().join(".config"), PathBuf::from);
    base.join("lossless/config.toml")
}

/// Read the config at `path`; a missing file means all defaults.
pub fn load_config(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse_config(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse_config(text: &str) -> Result<Config> {
    let mut cfg: Config = toml::from_str(text)?;
    cfg.music_dir = cfg.music_dir.map(|d| expand_tilde(&d));
    cfg.socket = cfg.socket.map(|s| expand_tilde(&s));
    Ok(cfg)
}
