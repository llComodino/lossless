use std::io::{ErrorKind, Read};
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

/// Largest accepted config file. Real configs are a few lines.
pub const MAX_CONFIG: u64 = 64 * 1024;

/// Read the config at `path`; a missing file means all defaults. Only a
/// regular file (or a symlink to one) of at most `MAX_CONFIG` bytes is read:
/// a FIFO or device would block or stream forever.
pub fn load_config(path: &Path) -> Result<Config> {
    let ctx = || format!("reading {}", path.display());
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return parse_config(""),
        Err(e) => return Err(e).with_context(ctx),
    };
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_CONFIG + 1).read_to_string(&mut text))
        .with_context(ctx)?;
    if text.len() as u64 > MAX_CONFIG {
        bail!("{} is larger than {MAX_CONFIG} bytes", path.display());
    }
    parse_config(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse_config(text: &str) -> Result<Config> {
    let mut cfg: Config = toml::from_str(text)?;
    check_device(&cfg.device)?;
    cfg.music_dir = cfg.music_dir.map(|d| expand_tilde(&d));
    cfg.socket = cfg.socket.map(|s| expand_tilde(&s));
    for (what, p) in [("music_dir", &cfg.music_dir), ("socket", &cfg.socket)] {
        if let Some(p) = p {
            check_path(what, p)?;
        }
    }
    Ok(cfg)
}

/// Longest accepted config path, in bytes (Linux PATH_MAX).
const MAX_PATH: usize = 4096;

/// A config path, after `~` expansion, must be absolute and spelled plainly:
/// no `.` or `..` segments, no control or bidi characters (they would reach
/// logs and terminals), valid UTF-8, at most `MAX_PATH` bytes.
pub fn check_path(what: &str, p: &Path) -> Result<()> {
    let Some(s) = p.to_str() else {
        bail!("{what} is not valid UTF-8");
    };
    if s.chars().any(|c| c.is_control() || is_bidi(c)) {
        bail!("{what} {s:?} contains control characters");
    }
    if s.len() > MAX_PATH {
        bail!("{what} is longer than {MAX_PATH} bytes");
    }
    if !p.is_absolute() {
        bail!("{what} {s:?} must be absolute or start with ~/");
    }
    if s.split('/').any(|seg| seg == "." || seg == "..") {
        bail!("{what} {s:?} must not contain . or .. segments");
    }
    Ok(())
}

fn is_bidi(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
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
    fn plain_absolute_paths_are_accepted() {
        for ok in [
            "/",
            "/home/me/Music",
            "/run/user/1000/lossless.sock",
            "/a//b/",
            "/a/.hidden/b..c",
        ] {
            check_path("p", Path::new(ok)).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
    }

    #[test]
    fn odd_paths_are_refused() {
        let long = format!("/{}", "a".repeat(MAX_PATH));
        for (bad, why) in [
            ("music", "absolute"),
            ("", "absolute"),
            ("./music", "absolute"),
            ("~user/music", "absolute"),
            ("/a/../etc", ".."),
            ("/a/./b", ".."),
            ("/a/..", ".."),
            ("/a\u{1b}[2J", "control"),
            ("/a\nb", "control"),
            ("/a\u{0}b", "control"),
            ("/a\u{7f}", "control"),
            ("/a\u{202e}gpj.flac", "control"),
            (long.as_str(), "longer"),
        ] {
            let err = check_path("p", Path::new(bad)).expect_err(bad).to_string();
            assert!(err.contains(why), "{bad:?}: {err}");
        }
    }

    #[test]
    fn non_utf8_paths_are_refused() {
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(std::ffi::OsStr::from_bytes(b"/a\xffb"));
        assert!(
            check_path("p", p)
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
    }

    #[test]
    fn config_paths_are_checked_after_tilde_expansion() {
        let home = home();
        let cfg = parse_config("music_dir = \"~\"\nsocket = \"~/x.sock\"").unwrap();
        assert_eq!(cfg.music_dir, Some(home.clone()));
        assert_eq!(cfg.socket, Some(home.join("x.sock")));
        for bad in [
            "music_dir = \"Music\"",
            "music_dir = \"~/../../etc\"",
            "socket = \"/tmp/\\u001b]0;pwned\\u0007.sock\"",
            "socket = \"sock\"",
        ] {
            assert!(parse_config(bad).is_err(), "{bad}");
        }
    }

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Self {
            let p =
                std::env::temp_dir().join(format!("lossless-cfg-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loading_reads_only_small_regular_files() {
        let t = Tmp::new("load");
        let p = t.0.join("config.toml");
        assert_eq!(
            load_config(&p).unwrap().device,
            "default",
            "missing means defaults"
        );

        std::fs::write(&p, "device = \"null\"\n").unwrap();
        assert_eq!(load_config(&p).unwrap().device, "null");
        let link = t.0.join("link.toml");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert_eq!(load_config(&link).unwrap().device, "null");

        let err = |p: &Path| format!("{:#}", load_config(p).unwrap_err());
        assert!(err(&t.0).contains("not a regular file"));
        assert!(err(Path::new("/dev/zero")).contains("not a regular file"));

        let big = format!("# {}\n", "x".repeat(MAX_CONFIG as usize));
        std::fs::write(&p, big).unwrap();
        assert!(err(&p).contains("larger than"));
        let exact = format!("#{}", "x".repeat(MAX_CONFIG as usize - 1));
        std::fs::write(&p, exact).unwrap();
        assert!(load_config(&p).is_ok(), "exactly the cap is fine");

        std::fs::write(&p, b"device = \"\xff\"").unwrap();
        assert!(err(&p).contains("reading"));
        std::fs::write(&p, "device = \"file:'|id'\"").unwrap();
        let e = err(&p);
        assert!(e.contains("parsing") && e.contains("not allowed"), "{e}");

        let blocked = t.0.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("config.toml"), "").unwrap();
        std::fs::set_permissions(
            &blocked,
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();
        let denied = load_config(&blocked.join("config.toml"));
        std::fs::set_permissions(
            &blocked,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        assert!(format!("{:#}", denied.unwrap_err()).contains("reading"));
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
