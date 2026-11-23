use std::collections::HashMap;

use serde::{Deserialize, Deserializer};

use super::{Action, Key, View};

macro_rules! names {
    ($($name:ident $text:literal [$($key:literal)*],)*) => {
        /// An action a key can be bound to.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Name {
            $($name,)*
        }

        impl Name {
            pub const ALL: &[Name] = &[$(Name::$name,)*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Name::$name => $text,)*
                }
            }

            fn defaults(self) -> &'static [&'static str] {
                match self {
                    $(Name::$name => &[$($key),*],)*
                }
            }
        }
    };
}

names! {
    Down "down" ["j"],
    Up "up" ["k"],
    First "first" ["g"],
    Last "last" ["G"],
    HalfDown "half_down" ["ctrl-d"],
    HalfUp "half_up" ["ctrl-u"],
    Left "left" ["h"],
    Right "right" ["l"],
    ViewQueue "view_queue" ["1"],
    ViewDirectories "view_directories" ["2"],
    ViewArtists "view_artists" ["3"],
    ViewSearch "view_search" ["4"],
    ViewScripts "view_scripts" [],
    NextView "next_view" ["tab"],
    PrevView "prev_view" ["backtab"],
    Enter "enter" ["enter"],
    Add "add" ["a"],
    AddNext "add_next" ["A"],
    Remove "remove" ["d"],
    Clear "clear" ["D"],
    MoveDown "move_down" ["J"],
    MoveUp "move_up" ["K"],
    Toggle "toggle" ["p" "space"],
    Stop "stop" ["s"],
    Next "next" [">"],
    Previous "previous" ["<"],
    SeekForward "seek_forward" ["f"],
    SeekBack "seek_back" ["b"],
    Repeat "repeat" ["r"],
    Random "random" ["z"],
    VolumeUp "volume_up" ["+"],
    VolumeDown "volume_down" ["-"],
    EditQuery "edit_query" ["/"],
    ClearFilter "clear_filter" ["esc"],
    Update "update" ["u"],
    SwitchProfile "switch_profile" ["o"],
    Help "help" ["?"],
    Quit "quit" ["q"],
    Scripts "scripts" [],
}

const COLUMNS: &[View] = &[View::Directories, View::Artists];
const LIBRARY: &[View] = &[View::Directories, View::Artists, View::Search];
const QUEUE: &[View] = &[View::Queue];

impl Name {
    pub fn parse(s: &str) -> Option<Name> {
        Name::ALL.iter().copied().find(|n| n.as_str() == s)
    }

    /// Where the action applies; fixed, only keys are configurable.
    pub fn scope(self) -> &'static [View] {
        use Name::*;
        match self {
            Left | Right => COLUMNS,
            Add | AddNext => LIBRARY,
            Remove | Clear | MoveDown | MoveUp => QUEUE,
            _ => &View::ALL,
        }
    }

    pub fn action(self) -> Option<Action> {
        use Name::*;
        Some(match self {
            Down => Action::Down,
            Up => Action::Up,
            First => Action::First,
            Last => Action::Last,
            HalfDown => Action::HalfDown,
            HalfUp => Action::HalfUp,
            Left => Action::Left,
            Right => Action::Right,
            ViewQueue => Action::Show(View::Queue),
            ViewDirectories => Action::Show(View::Directories),
            ViewArtists => Action::Show(View::Artists),
            ViewSearch => Action::Show(View::Search),
            NextView => Action::NextView,
            PrevView => Action::PrevView,
            Enter => Action::Enter,
            Add => Action::Add,
            AddNext => Action::AddNext,
            Remove => Action::Remove,
            Clear => Action::Clear,
            MoveDown => Action::MoveDown,
            MoveUp => Action::MoveUp,
            Toggle => Action::Toggle,
            Stop => Action::Stop,
            Next => Action::Next,
            Previous => Action::Previous,
            SeekForward => Action::SeekForward,
            SeekBack => Action::SeekBack,
            Repeat => Action::Repeat,
            Random => Action::Random,
            VolumeUp => Action::VolumeUp,
            VolumeDown => Action::VolumeDown,
            EditQuery => Action::EditQuery,
            ClearFilter => Action::ClearFilter,
            Update => Action::Update,
            SwitchProfile => Action::SwitchProfile,
            Help => Action::Help,
            Quit => Action::Quit,
            ViewScripts | Scripts => return None,
        })
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Name::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown action {s:?}")))
    }
}

/// Which keys run which action, defaults merged with the user's overrides.
#[derive(Clone, Debug)]
pub struct Keymap {
    keys: HashMap<Name, Vec<Key>>,
}

impl Default for Keymap {
    fn default() -> Keymap {
        let keys = Name::ALL
            .iter()
            .map(|&n| {
                let keys = n
                    .defaults()
                    .iter()
                    .map(|k| Key::parse(k).expect("default key"));
                (n, keys.collect())
            })
            .collect();
        Keymap { keys }
    }
}

impl Keymap {
    /// Each listed action gets exactly the given keys; an empty list
    /// unbinds it.
    pub fn with_overrides(over: Vec<(Name, Vec<Key>)>) -> Result<Keymap, String> {
        let mut map = Keymap::default();
        map.keys.extend(over);
        map.check()?;
        Ok(map)
    }

    /// No key may run two actions in the same view.
    pub(crate) fn check(&self) -> Result<(), String> {
        for (i, &a) in Name::ALL.iter().enumerate() {
            for &b in &Name::ALL[i + 1..] {
                let overlap = a.scope().iter().any(|v| b.scope().contains(v));
                if !overlap {
                    continue;
                }
                if let Some(k) = self.keys(a).iter().find(|k| self.keys(b).contains(k)) {
                    return Err(format!(
                        "key \"{k}\" is bound to both {:?} and {:?}",
                        a.as_str(),
                        b.as_str()
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn lookup(&self, view: View, key: Key) -> Option<Action> {
        Name::ALL
            .iter()
            .find(|n| n.scope().contains(&view) && self.keys(**n).contains(&key))
            .and_then(|n| n.action())
    }

    pub fn keys(&self, name: Name) -> &[Key] {
        self.keys.get(&name).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Key {
        Key::parse(s).unwrap()
    }

    #[test]
    fn defaults_pass_the_check() {
        Keymap::default().check().unwrap();
    }

    #[test]
    fn every_name_round_trips() {
        for n in Name::ALL {
            assert_eq!(Name::parse(n.as_str()), Some(*n));
        }
        assert_eq!(Name::parse("quitt"), None);
    }

    #[test]
    fn d_is_remove_in_queue_and_nothing_elsewhere() {
        let m = Keymap::default();
        assert_eq!(m.lookup(View::Queue, k("d")), Some(Action::Remove));
        assert_eq!(m.lookup(View::Directories, k("d")), None);
    }

    #[test]
    fn override_replaces_defaults() {
        let m = Keymap::with_overrides(vec![(Name::Toggle, vec![k("t")])]).unwrap();
        assert_eq!(m.lookup(View::Queue, k("t")), Some(Action::Toggle));
        assert_eq!(m.lookup(View::Queue, k("p")), None);
        assert_eq!(m.lookup(View::Queue, k("space")), None);
    }

    #[test]
    fn unbinding_removes_all_keys() {
        let m = Keymap::with_overrides(vec![(Name::Quit, vec![])]).unwrap();
        assert_eq!(m.lookup(View::Queue, k("q")), None);
        assert!(m.keys(Name::Quit).is_empty());
    }

    #[test]
    fn override_clashing_with_default_is_an_error() {
        let e = Keymap::with_overrides(vec![(Name::Stop, vec![k("d")])]).unwrap_err();
        assert!(
            e.contains("\"d\"") && e.contains("remove") && e.contains("stop"),
            "{e}"
        );
    }

    #[test]
    fn disjoint_scopes_may_share_a_key() {
        Keymap::with_overrides(vec![
            (Name::Add, vec![k("x")]),
            (Name::Remove, vec![k("x")]),
        ])
        .unwrap();
    }

    #[test]
    fn scripts_names_do_nothing_yet() {
        assert_eq!(Name::Scripts.action(), None);
        assert_eq!(Name::ViewScripts.action(), None);
        assert!(Keymap::default().keys(Name::Scripts).is_empty());
    }
}
