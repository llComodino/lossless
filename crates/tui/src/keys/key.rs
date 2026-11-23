use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde::{Deserialize, Deserializer};

/// A key as bindings name it: Shift is never part of it, a shifted
/// character is that character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
}

const NAMES: [(&str, KeyCode); 13] = [
    ("space", KeyCode::Char(' ')),
    ("enter", KeyCode::Enter),
    ("esc", KeyCode::Esc),
    ("tab", KeyCode::Tab),
    ("backtab", KeyCode::BackTab),
    ("backspace", KeyCode::Backspace),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("pgup", KeyCode::PageUp),
];

fn named(s: &str) -> Option<KeyCode> {
    if s == "pgdown" {
        return Some(KeyCode::PageDown);
    }
    if let Some(n) = s.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=12).contains(&n)
        && !s[1..].starts_with('0')
    {
        return Some(KeyCode::F(n));
    }
    NAMES.iter().find(|(name, _)| *name == s).map(|&(_, c)| c)
}

impl Key {
    pub fn parse(s: &str) -> Result<Key, String> {
        let bad = || {
            format!(
                "key {s:?}: expected a character or one of space, enter, esc, tab, \
                 backtab, backspace, up, down, left, right, home, end, pgup, pgdown, \
                 f1-f12, optionally after ctrl- or alt-"
            )
        };
        let (mut ctrl, mut alt, mut rest) = (false, false, s);
        loop {
            if let Some(r) = rest
                .strip_prefix("ctrl-")
                .filter(|r| !r.is_empty() && !ctrl)
            {
                (ctrl, rest) = (true, r);
            } else if let Some(r) = rest.strip_prefix("alt-").filter(|r| !r.is_empty() && !alt) {
                (alt, rest) = (true, r);
            } else {
                break;
            }
        }
        let mut chars = rest.chars();
        let code = match (chars.next(), chars.next()) {
            (Some(c), None) => KeyCode::Char(c),
            _ => named(rest).ok_or_else(bad)?,
        };
        Ok(Key { code, ctrl, alt })
    }

    pub fn from_event(e: &KeyEvent) -> Option<Key> {
        if e.kind == KeyEventKind::Release {
            return None;
        }
        Some(Key {
            code: e.code,
            ctrl: e.modifiers.contains(KeyModifiers::CONTROL),
            alt: e.modifiers.contains(KeyModifiers::ALT),
        })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("ctrl-")?;
        }
        if self.alt {
            f.write_str("alt-")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::PageDown => f.write_str("pgdown"),
            KeyCode::F(n) => write!(f, "f{n}"),
            code => match NAMES.iter().find(|(_, c)| *c == code) {
                Some((name, _)) => f.write_str(name),
                None => write!(f, "{code:?}"),
            },
        }
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Key::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode::{self, Char};
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    use super::*;

    #[test]
    fn parses_chars_and_names() {
        assert_eq!(
            Key::parse("j"),
            Ok(Key {
                code: Char('j'),
                ctrl: false,
                alt: false
            })
        );
        assert_eq!(Key::parse("space").unwrap().code, Char(' '));
        assert_eq!(
            Key::parse("ctrl-d"),
            Ok(Key {
                code: Char('d'),
                ctrl: true,
                alt: false
            })
        );
        assert_eq!(Key::parse("alt-ctrl-x"), Key::parse("ctrl-alt-x"));
        assert_eq!(Key::parse("f12").unwrap().code, KeyCode::F(12));
        assert_eq!(Key::parse("backtab").unwrap().code, KeyCode::BackTab);
        assert_eq!(Key::parse("-").unwrap().code, Char('-'));
    }

    #[test]
    fn rejects_bad_keys() {
        for s in [
            "",
            "jj",
            "ctrl-",
            "f13",
            "f0",
            "meta-x",
            "Space",
            "ctrl-ctrl-x",
        ] {
            assert!(Key::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn ctrl_keeps_case() {
        assert_ne!(Key::parse("ctrl-D"), Key::parse("ctrl-d"));
    }

    #[test]
    fn shift_is_ignored_for_chars() {
        let ev = |c, m| KeyEvent::new(Char(c), m);
        assert_eq!(
            Key::from_event(&ev('G', KeyModifiers::SHIFT)),
            Key::parse("G").ok()
        );
        assert_eq!(
            Key::from_event(&ev('+', KeyModifiers::SHIFT)),
            Key::parse("+").ok()
        );
        assert_eq!(
            Key::from_event(&ev('d', KeyModifiers::CONTROL)),
            Key::parse("ctrl-d").ok()
        );
        assert_eq!(
            Key::from_event(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Key::parse("backtab").ok()
        );
    }

    #[test]
    fn release_is_none() {
        let k = KeyEvent::new_with_kind_and_state(
            Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        );
        assert_eq!(Key::from_event(&k), None);
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "j",
            "G",
            "ctrl-d",
            "alt-x",
            "ctrl-alt-x",
            "space",
            "enter",
            "backtab",
            "f5",
            "/",
            "-",
        ] {
            assert_eq!(Key::parse(s).unwrap().to_string(), s);
        }
    }
}
