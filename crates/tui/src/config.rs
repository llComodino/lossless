use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use lossless_proto::profile::Dirs;
use serde::Deserialize;

use crate::keys::{Key, Keymap, Name};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, deserialize_with = "unique_keys")]
    keys: HashMap<Name, Vec<Key>>,
    #[serde(default)]
    scripts: Vec<Script>,
    #[serde(default = "script_lines")]
    script_lines: usize,
}

impl Default for File {
    fn default() -> File {
        File {
            keys: HashMap::new(),
            scripts: Vec::new(),
            script_lines: script_lines(),
        }
    }
}

/// The `keys` map, refusing an action listed twice (RON keeps the last).
fn unique_keys<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<HashMap<Name, Vec<Key>>, D::Error> {
    struct Unique;
    impl<'de> serde::de::Visitor<'de> for Unique {
        type Value = HashMap<Name, Vec<Key>>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of action names to key lists")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut m: A,
        ) -> Result<Self::Value, A::Error> {
            let mut keys = HashMap::new();
            while let Some((name, list)) = m.next_entry::<Name, Vec<Key>>()? {
                if keys.insert(name, list).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "action {:?} twice",
                        name.as_str()
                    )));
                }
            }
            Ok(keys)
        }
    }
    d.deserialize_map(Unique)
}

fn script_lines() -> usize {
    200
}

const MAX_SCRIPT_LINES: usize = 100_000;

/// A user script; read in every build, run only with the `scripts`
/// feature.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub name: String,
    pub command: String,
    #[serde(default, deserialize_with = "some_key")]
    pub key: Option<Key>,
    pub mode: Mode,
}

/// `key: "r"`, not RON's `key: Some("r")`.
fn some_key<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Key>, D::Error> {
    Key::deserialize(d).map(Some)
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Modal,
    Terminal,
}

#[derive(Debug)]
pub struct Config {
    pub keymap: Keymap,
    pub scripts: Vec<Script>,
    /// Output lines a script's modal keeps.
    pub script_lines: usize,
}

fn check_scripts(scripts: &[Script], lines: usize) -> Result<(), String> {
    if !(1..=MAX_SCRIPT_LINES).contains(&lines) {
        return Err(format!(
            "script_lines {lines} must be 1 to {MAX_SCRIPT_LINES}"
        ));
    }
    for (i, s) in scripts.iter().enumerate() {
        if s.name.trim().is_empty() {
            return Err("script name must not be empty".into());
        }
        if s.command.trim().is_empty() {
            return Err(format!("script {:?}: command must not be empty", s.name));
        }
        if let Some(k) = s.key
            && let Some(o) = scripts[..i].iter().find(|o| o.key == Some(k))
        {
            return Err(format!(
                "key \"{k}\" is used by both scripts {:?} and {:?}",
                o.name, s.name
            ));
        }
    }
    Ok(())
}

pub fn path(dirs: &Dirs) -> PathBuf {
    dirs.config.join("lossless/tui.ron")
}

/// Nothing but whitespace and comments.
fn blank(mut s: &str) -> bool {
    loop {
        s = s.trim_start();
        if let Some(r) = s.strip_prefix("//") {
            s = r.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(r) = s.strip_prefix("/*") {
            match r.split_once("*/") {
                Some((_, r)) => s = r,
                None => return false,
            }
        } else {
            return s.is_empty();
        }
    }
}

/// `path` read once; a missing file, or one with only comments, gives the
/// defaults.
pub fn load(path: &Path) -> Result<Config, String> {
    let shown = path.display();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    let file: File = if blank(&text) {
        File::default()
    } else {
        ron::from_str(&text).map_err(|e| {
            let at = e.span.start;
            format!("{shown}:{}:{}: {}", at.line, at.col, e.code)
        })?
    };
    let keymap = Keymap::with_overrides(file.keys.into_iter().collect())
        .map_err(|e| format!("{shown}: {e}"))?;
    check_scripts(&file.scripts, file.script_lines).map_err(|e| format!("{shown}: {e}"))?;
    Ok(Config {
        keymap,
        scripts: file.scripts,
        script_lines: file.script_lines,
    })
}

#[cfg(test)]
mod tests {
    use lossless_engine::fixtures::Dir;

    use super::*;
    use crate::keys::{Key, Name};

    fn file(dir: &Dir, text: &str) -> PathBuf {
        let p = dir.path("tui.ron");
        std::fs::write(&p, text).unwrap();
        p
    }

    fn quit_keys(c: &Config) -> Vec<Key> {
        c.keymap.keys(Name::Quit).to_vec()
    }

    #[test]
    fn missing_file_is_defaults() {
        let dir = Dir::new();
        let c = load(&dir.path("nope.ron")).unwrap();
        assert_eq!(quit_keys(&c), vec![Key::parse("q").unwrap()]);
    }

    #[test]
    fn empty_file_is_defaults() {
        let dir = Dir::new();
        for t in ["", "  \n", "()"] {
            let c = load(&file(&dir, t)).unwrap();
            assert_eq!(quit_keys(&c), vec![Key::parse("q").unwrap()], "{t:?}");
        }
    }

    #[test]
    fn comment_only_file_is_defaults() {
        let dir = Dir::new();
        for t in [
            "// all off\n// (keys: {})\n",
            "/* (keys: {\"quit\": [\"Q\"]}) */",
            " // x\n /* y */ \n",
        ] {
            let c = load(&file(&dir, t)).unwrap();
            assert_eq!(quit_keys(&c), vec![Key::parse("q").unwrap()], "{t:?}");
        }
    }

    #[test]
    fn scripts_parse() {
        let dir = Dir::new();
        let c = load(&file(
            &dir,
            r#"(scripts: [
                (name: "r", command: "echo", key: "r", mode: Modal),
                (name: "t", command: "x", mode: Terminal),
            ])"#,
        ))
        .unwrap();
        assert_eq!(c.scripts.len(), 2);
        assert_eq!(c.scripts[0].key, Some(Key::parse("r").unwrap()));
        assert_eq!(c.scripts[0].mode, Mode::Modal);
        assert_eq!(c.scripts[1].key, None);
        assert_eq!(c.scripts[1].mode, Mode::Terminal);
        assert_eq!(c.script_lines, 200);
    }

    #[test]
    fn script_lines_bounds() {
        let dir = Dir::new();
        for (n, ok) in [(0, false), (1, true), (100_000, true), (100_001, false)] {
            let r = load(&file(&dir, &format!("(script_lines: {n})")));
            assert_eq!(r.is_ok(), ok, "{n}: {r:?}");
        }
        let c = load(&file(&dir, "(script_lines: 50)")).unwrap();
        assert_eq!(c.script_lines, 50);
    }

    #[test]
    fn duplicate_script_key_is_an_error() {
        let dir = Dir::new();
        let e = load(&file(
            &dir,
            r#"(scripts: [
                (name: "a", command: "x", key: "r", mode: Modal),
                (name: "b", command: "y", key: "r", mode: Terminal),
            ])"#,
        ))
        .unwrap_err();
        assert!(
            e.contains("\"r\"") && e.contains("\"a\"") && e.contains("\"b\""),
            "{e}"
        );
    }

    #[test]
    fn empty_name_or_command_is_an_error() {
        let dir = Dir::new();
        for t in [
            r#"(scripts: [(name: "", command: "x", mode: Modal)])"#,
            r#"(scripts: [(name: "a", command: " ", mode: Modal)])"#,
        ] {
            assert!(load(&file(&dir, t)).is_err(), "{t}");
        }
    }

    #[test]
    fn unknown_mode_has_position() {
        let dir = Dir::new();
        let e = load(&file(
            &dir,
            r#"(scripts: [(name: "a", command: "x", mode: Popup)])"#,
        ))
        .unwrap_err();
        assert!(e.contains("tui.ron:1:"), "{e}");
    }

    #[test]
    fn duplicate_action_is_an_error() {
        let dir = Dir::new();
        let e = load(&file(&dir, r#"(keys: {"quit": ["Q"], "quit": ["w"]})"#)).unwrap_err();
        assert!(
            e.contains("tui.ron:1:") && e.contains("\"quit\" twice"),
            "{e}"
        );
    }

    #[test]
    fn overrides_apply() {
        let dir = Dir::new();
        let c = load(&file(&dir, r#"(keys: {"quit": ["Q"]})"#)).unwrap();
        assert_eq!(quit_keys(&c), vec![Key::parse("Q").unwrap()]);
    }

    #[test]
    fn unknown_action_has_position() {
        let dir = Dir::new();
        let e = load(&file(&dir, "(\n  keys: {\"quitt\": [\"q\"]},\n)")).unwrap_err();
        assert!(e.contains("tui.ron:2:") && e.contains("quitt"), "{e}");
    }

    #[test]
    fn bad_key_has_position() {
        let dir = Dir::new();
        let e = load(&file(&dir, r#"(keys: {"quit": ["ctrl-"]})"#)).unwrap_err();
        assert!(e.contains("tui.ron:1:") && e.contains("ctrl-"), "{e}");
    }

    #[test]
    fn unknown_field_is_an_error() {
        let dir = Dir::new();
        assert!(load(&file(&dir, "(colors: 1)")).is_err());
    }

    #[test]
    fn clash_names_the_file() {
        let dir = Dir::new();
        let p = file(&dir, r#"(keys: {"stop": ["d"]})"#);
        let e = load(&p).unwrap_err();
        assert!(
            e.starts_with(&format!("{}: ", p.display())) && e.contains("remove"),
            "{e}"
        );
    }
}
