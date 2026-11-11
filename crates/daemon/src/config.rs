use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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
    check_device(&cfg.device)?;
    cfg.music_dir = cfg.music_dir.map(|d| expand_tilde(&d));
    cfg.socket = cfg.socket.map(|s| expand_tilde(&s));
    Ok(cfg)
}

/// Device names usable without a PCM definition of their own.
const BARE_DEVICES: &[&str] = &["default", "null", "pipewire", "pulse", "sysdefault"];
/// Device families that take `CARD[,DEV]` and nothing else.
const CARD_DEVICES: &[&str] = &["hw", "plughw", "front", "sysdefault"];

/// Accept only plain device names. ALSA's name syntax reaches every PCM
/// plugin, including `file`, which runs `file:'|cmd'` through a shell, so
/// anything beyond a bare name or `family:CARD=id,DEV=n` (or `family:id,n`)
/// is refused.
pub fn check_device(name: &str) -> Result<()> {
    if BARE_DEVICES.contains(&name) {
        return Ok(());
    }
    let ok = name
        .split_once(':')
        .is_some_and(|(family, args)| CARD_DEVICES.contains(&family) && card_args(args));
    if !ok {
        bail!(
            "device {name:?} is not allowed: use one of {} or {}:CARD=<id>[,DEV=<n>]",
            BARE_DEVICES.join(", "),
            CARD_DEVICES.join("/")
        );
    }
    Ok(())
}

fn card_args(args: &str) -> bool {
    let (card, dev) = match args.split_once(',') {
        Some((c, d)) => (c, Some(d)),
        None => (args, None),
    };
    let keyed = card.starts_with("CARD=");
    let card = card.strip_prefix("CARD=").unwrap_or(card);
    let dev_ok = match dev {
        None => true,
        Some(d) if keyed => d.strip_prefix("DEV=").is_some_and(is_index),
        Some(d) => is_index(d),
    };
    is_card_id(card) && dev_ok
}

/// Kernel card ids: up to 15 of `[A-Za-z0-9_-]`, or a card number.
fn is_card_id(s: &str) -> bool {
    (1..=15).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_index(s: &str) -> bool {
    (1..=2).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_devices_are_accepted() {
        for ok in [
            "default",
            "null",
            "pipewire",
            "hw:CARD=PRO2,DEV=0",
            "hw:CARD=PRO2",
            "front:CARD=Capsule-X_1,DEV=3",
            "plughw:0,0",
            "hw:PRO2",
            "sysdefault:CARD=PRO2",
            "sysdefault",
        ] {
            check_device(ok).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
    }

    #[test]
    fn plugin_syntax_is_refused() {
        for bad in [
            "file:'|sh -c id',raw",
            "file:FILE=/tmp/x",
            "file",
            "plug:file",
            "{ type file file \"|id\" slave.pcm null }",
            "hw:CARD=PRO2,DEV=0,SUBDEV=0",
            "hw:CARD=PRO2,0",
            "hw:PRO2,DEV=0",
            "hw:CARD=\"PRO2\"",
            "hw:CARD=PRO2;DEV=0",
            "hw:CARD=PRO2,DEV=100",
            "hw:CARD=ThisNameIsTooLong",
            "hw:",
            "hw:,0",
            "hw:CARD=",
            "hw:CARD=PRO2,DEV=",
            "hw:PRO2,",
            "hw:PRO2 ",
            " default",
            "Default",
            "dmix:CARD=PRO2",
            "hw:CARD=PR\u{0}O2",
            "",
        ] {
            let err = check_device(bad).expect_err(bad).to_string();
            assert!(err.contains("not allowed"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn config_rejects_a_plugin_device() {
        let err = parse_config("device = \"file:'|touch /tmp/x',raw\"").unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }
}

#[cfg(test)]
mod props {
    use proptest::prelude::*;

    use super::check_device;

    /// Independent of the grammar: what an accepted name may contain.
    fn harmless(name: &str) -> bool {
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-:,=".contains(&b))
            && name.matches(':').count() <= 1
            && name.len() <= 40
    }

    proptest! {
        #[test]
        fn accepted_names_are_harmless(name in "\\PC{0,40}") {
            if check_device(&name).is_ok() {
                prop_assert!(harmless(&name), "{name:?}");
            }
        }

        #[test]
        fn accepted_names_from_near_misses_are_harmless(
            name in "(hw|plughw|front|file|plug|dmix|sysdefault)(:([A-Za-z0-9_=,'|\" -]{0,24}))?"
        ) {
            if check_device(&name).is_ok() {
                prop_assert!(harmless(&name), "{name:?}");
            }
        }

        #[test]
        fn every_real_card_name_is_accepted(
            family in "(hw|plughw|front|sysdefault)",
            card in "[A-Za-z0-9_-]{1,15}",
            dev in 0u8..32,
        ) {
            let name = format!("{family}:CARD={card},DEV={dev}");
            prop_assert!(check_device(&name).is_ok(), "{}", name);
            let name = format!("{family}:{card},{dev}");
            prop_assert!(check_device(&name).is_ok(), "{}", name);
            let name = format!("{family}:CARD={card}");
            prop_assert!(check_device(&name).is_ok(), "{}", name);
        }
    }
}
