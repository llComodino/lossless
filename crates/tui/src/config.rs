use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use lossless_proto::profile::Dirs;
use serde::Deserialize;

use crate::keys::{Key, Keymap, Name};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    keys: HashMap<Name, Vec<Key>>,
}

#[derive(Debug)]
pub struct Config {
    pub keymap: Keymap,
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
    Ok(Config { keymap })
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
