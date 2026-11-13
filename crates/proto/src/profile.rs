//! Profile names and the paths they map to. `default` keeps the original
//! paths; any other name gets its own config, state and socket.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

const DEFAULT: &str = "default";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Profile(String);

impl Default for Profile {
    fn default() -> Self {
        Profile(DEFAULT.into())
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Profile {
    pub fn parse(name: &str) -> Result<Profile, String> {
        let ok = (1..=32).contains(&name.len())
            && name
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
        if ok {
            Ok(Profile(name.into()))
        } else {
            Err(format!("{name:?}: profile names are 1-32 of a-z 0-9 _ -"))
        }
    }

    pub fn name(&self) -> &str {
        &self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == DEFAULT
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    pub config: PathBuf,
    pub state: PathBuf,
    pub runtime: Option<PathBuf>,
    pub user: String,
}

impl Dirs {
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Dirs {
        let home = var("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        Dirs {
            config: var("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from),
            state: var("XDG_STATE_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(".local/state")),
            runtime: var("XDG_RUNTIME_DIR").map(PathBuf::from),
            user: var("USER")
                .and_then(|u| u.into_string().ok())
                .unwrap_or_else(|| "user".into()),
        }
    }

    pub fn from_env() -> Dirs {
        Dirs::from_vars(|k| std::env::var_os(k))
    }

    pub fn config_path(&self, p: &Profile) -> PathBuf {
        let file = if p.is_default() { "config" } else { p.name() };
        self.config.join("lossless").join(format!("{file}.toml"))
    }

    pub fn state_path(&self, p: &Profile) -> PathBuf {
        let dir = self.state.join("lossless");
        let dir = if p.is_default() {
            dir
        } else {
            dir.join(p.name())
        };
        dir.join("state.json")
    }

    /// Without a runtime dir: `$TMPDIR/lossless-USER[-NAME].sock`.
    pub fn socket(&self, p: &Profile) -> PathBuf {
        let suffix = if p.is_default() {
            String::new()
        } else {
            format!("-{}", p.name())
        };
        match &self.runtime {
            Some(dir) => dir.join(format!("lossless{suffix}.sock")),
            None => std::env::temp_dir().join(format!("lossless-{}{suffix}.sock", self.user)),
        }
    }

    /// `default` first, then every regular `NAME.toml` (symlinks followed) with a valid name
    /// other than `config`, sorted.
    pub fn profiles(&self) -> Vec<Profile> {
        let mut named: Vec<Profile> = std::fs::read_dir(self.config.join("lossless"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| std::fs::metadata(e.path()).is_ok_and(|m| m.is_file()))
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let stem = name.strip_suffix(".toml")?;
                (stem != "config")
                    .then(|| Profile::parse(stem).ok())
                    .flatten()
            })
            .filter(|p| !p.is_default())
            .collect();
        named.sort();
        std::iter::once(Profile::default()).chain(named).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    fn p(name: &str) -> Profile {
        Profile::parse(name).unwrap()
    }

    fn dirs() -> Dirs {
        Dirs {
            config: "C".into(),
            state: "S".into(),
            runtime: Some("R".into()),
            user: "u".into(),
        }
    }

    #[test]
    fn names_are_checked() {
        for ok in ["pw", "a", "dac-2", "x_y", &"a".repeat(32), "default"] {
            assert_eq!(p(ok).name(), ok);
        }
        assert!(p("default").is_default());
        assert!(Profile::default().is_default());
        assert!(!p("pw").is_default());
        for bad in ["", &"a".repeat(33), "Pw", "p w", "../x", "x.toml", "é"] {
            assert!(Profile::parse(bad).is_err(), "{bad:?} accepted");
        }
        assert_eq!(
            Profile::parse("Bad").unwrap_err(),
            "\"Bad\": profile names are 1-32 of a-z 0-9 _ -"
        );
        assert_eq!(p("pw").to_string(), "pw");
    }

    #[test]
    fn default_keeps_todays_paths() {
        let d = dirs();
        let p = Profile::default();
        assert_eq!(d.config_path(&p), Path::new("C/lossless/config.toml"));
        assert_eq!(d.state_path(&p), Path::new("S/lossless/state.json"));
        assert_eq!(d.socket(&p), Path::new("R/lossless.sock"));
    }

    #[test]
    fn named_profile_paths() {
        let d = dirs();
        let pw = p("pw");
        assert_eq!(d.config_path(&pw), Path::new("C/lossless/pw.toml"));
        assert_eq!(d.state_path(&pw), Path::new("S/lossless/pw/state.json"));
        assert_eq!(d.socket(&pw), Path::new("R/lossless-pw.sock"));
    }

    #[test]
    fn sockets_without_a_runtime_dir() {
        let d = Dirs {
            runtime: None,
            ..dirs()
        };
        let tmp = std::env::temp_dir();
        assert_eq!(d.socket(&Profile::default()), tmp.join("lossless-u.sock"));
        assert_eq!(d.socket(&p("pw")), tmp.join("lossless-u-pw.sock"));
    }

    #[test]
    fn listing_skips_what_is_not_a_profile() {
        let root = std::env::temp_dir().join(format!("lossless-profiles-{}", std::process::id()));
        let dir = root.join("lossless");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(dir.join("x.toml")).unwrap();
        for f in [
            "config.toml",
            "pw.toml",
            "alsa.toml",
            "foo.toml.bak",
            "Bad Name.toml",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::fs::write(root.join("stowed.toml"), "").unwrap();
        std::os::unix::fs::symlink(root.join("stowed.toml"), dir.join("dac.toml")).unwrap();
        std::os::unix::fs::symlink(root.join("gone.toml"), dir.join("gone.toml")).unwrap();
        std::os::unix::fs::symlink(dir.join("x.toml"), dir.join("y.toml")).unwrap();
        let d = Dirs {
            config: root.clone(),
            ..dirs()
        };
        let names: Vec<String> = d.profiles().iter().map(|p| p.name().to_owned()).collect();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(names, ["default", "alsa", "dac", "pw"]);
        let missing = Dirs {
            config: root.join("nope"),
            ..dirs()
        };
        assert_eq!(missing.profiles(), [Profile::default()]);
    }

    #[test]
    fn state_paths_follow_the_existing_fallbacks() {
        let from = |vars: &[(&str, &str)]| {
            let m: HashMap<String, OsString> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), OsString::from(v)))
                .collect();
            Dirs::from_vars(|k| m.get(k).cloned())
        };
        let d = from(&[("HOME", "/h"), ("XDG_STATE_HOME", "rel")]);
        assert_eq!(d.state, Path::new("/h/.local/state"));
        assert_eq!(d.config, Path::new("/h/.config"));
        let d = from(&[]);
        assert_eq!(d.config, Path::new("/.config"));
        assert_eq!(d.state, Path::new("/.local/state"));
        assert_eq!(d.runtime, None);
        assert_eq!(d.user, "user");
        let d = from(&[
            ("HOME", "/h"),
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_STATE_HOME", "/s"),
            ("XDG_RUNTIME_DIR", "/r"),
            ("USER", "me"),
        ]);
        assert_eq!(d.config, Path::new("/c"));
        assert_eq!(d.state, Path::new("/s"));
        assert_eq!(d.runtime.as_deref(), Some(Path::new("/r")));
        assert_eq!(d.user, "me");
    }
}
