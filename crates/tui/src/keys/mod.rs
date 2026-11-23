#[cfg_attr(not(test), expect(dead_code, reason = "the key table uses it next"))]
mod key;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Queue,
    Directories,
    Artists,
    Search,
}

impl View {
    pub const ALL: [View; 4] = [View::Queue, View::Directories, View::Artists, View::Search];

    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    First,
    Last,
    HalfDown,
    HalfUp,
    Left,
    Right,
    Show(View),
    NextView,
    PrevView,
    Enter,
    Add,
    AddNext,
    Remove,
    Clear,
    MoveDown,
    MoveUp,
    Toggle,
    Stop,
    Next,
    Previous,
    SeekForward,
    SeekBack,
    Repeat,
    Random,
    VolumeUp,
    VolumeDown,
    EditQuery,
    Update,
    SwitchProfile,
    Help,
    Quit,
    Type(char),
    Backspace,
    Submit,
    Cancel,
    ClearFilter,
}

pub fn action(view: View, key: KeyEvent, editing: bool) -> Option<Action> {
    use Action::*;
    use KeyCode::Char;
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    if editing {
        return match key.code {
            Char('n') if ctrl => Some(Down),
            Char('p') if ctrl => Some(Up),
            Char(c) if plain => Some(Type(c)),
            KeyCode::Down => Some(Down),
            KeyCode::Up => Some(Up),
            KeyCode::Backspace => Some(Backspace),
            KeyCode::Enter => Some(Submit),
            KeyCode::Esc => Some(Cancel),
            _ => None,
        };
    }
    let columns = matches!(view, View::Directories | View::Artists);
    let queue = view == View::Queue;
    Some(match key.code {
        Char('d') if ctrl => HalfDown,
        Char('u') if ctrl => HalfUp,
        Char(_) if !plain => return None,
        Char('j') => Down,
        Char('k') => Up,
        Char('g') => First,
        Char('G') => Last,
        Char('h') if columns => Left,
        Char('l') if columns => Right,
        Char(c @ '1'..='4') => Show(View::ALL[usize::from(c as u8 - b'1')]),
        KeyCode::Tab => NextView,
        KeyCode::BackTab => PrevView,
        KeyCode::Enter => Enter,
        KeyCode::Esc => ClearFilter,
        Char('a') if !queue => Add,
        Char('A') if !queue => AddNext,
        Char('d') if queue => Remove,
        Char('D') if queue => Clear,
        Char('J') if queue => MoveDown,
        Char('K') if queue => MoveUp,
        Char('p' | ' ') => Toggle,
        Char('s') => Stop,
        Char('>') => Next,
        Char('<') => Previous,
        Char('f') => SeekForward,
        Char('b') => SeekBack,
        Char('r') => Repeat,
        Char('z') => Random,
        Char('+') => VolumeUp,
        Char('-') => VolumeDown,
        Char('/') => EditQuery,
        Char('u') => Update,
        Char('o') => SwitchProfile,
        Char('?') => Help,
        Char('q') => Quit,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventState;

    use super::*;
    use Action::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn keys_map_per_view() {
        let none = KeyModifiers::NONE;
        let c = |ch| key(KeyCode::Char(ch), none);
        let rows = [
            (View::Queue, c('j'), Some(Down)),
            (View::Queue, c('G'), Some(Last)),
            (
                View::Queue,
                key(KeyCode::Char('d'), KeyModifiers::CONTROL),
                Some(HalfDown),
            ),
            (
                View::Queue,
                key(KeyCode::Char('u'), KeyModifiers::CONTROL),
                Some(HalfUp),
            ),
            (
                View::Queue,
                key(KeyCode::Char('x'), KeyModifiers::CONTROL),
                None,
            ),
            (View::Queue, c('d'), Some(Remove)),
            (View::Queue, c('D'), Some(Clear)),
            (View::Queue, c('J'), Some(MoveDown)),
            (View::Queue, c('K'), Some(MoveUp)),
            (View::Queue, c('a'), None),
            (View::Queue, c('h'), None),
            (View::Queue, c('l'), None),
            (View::Directories, c('l'), Some(Right)),
            (View::Artists, c('h'), Some(Left)),
            (View::Directories, c('d'), None),
            (View::Artists, c('A'), Some(AddNext)),
            (View::Search, c('a'), Some(Add)),
            (View::Search, c('l'), None),
            (View::Search, c('3'), Some(Show(View::Artists))),
            (
                View::Queue,
                key(KeyCode::BackTab, KeyModifiers::SHIFT),
                Some(PrevView),
            ),
            (View::Queue, key(KeyCode::Enter, none), Some(Enter)),
            (View::Queue, c(' '), Some(Toggle)),
            (View::Queue, c('/'), Some(EditQuery)),
            (View::Queue, c('o'), Some(SwitchProfile)),
            (View::Queue, c('?'), Some(Help)),
            (View::Queue, c('q'), Some(Quit)),
            (View::Queue, key(KeyCode::Down, none), None),
            (View::Queue, key(KeyCode::Esc, none), Some(ClearFilter)),
            (View::Directories, c('/'), Some(EditQuery)),
            (View::Artists, key(KeyCode::Esc, none), Some(ClearFilter)),
        ];
        for (view, k, want) in rows {
            assert_eq!(action(view, k, false), want, "{view:?} {k:?}");
        }
    }

    #[test]
    fn the_query_line_takes_text() {
        let none = KeyModifiers::NONE;
        let rows = [
            (key(KeyCode::Char('q'), none), Some(Type('q'))),
            (
                key(KeyCode::Char('Q'), KeyModifiers::SHIFT),
                Some(Type('Q')),
            ),
            (key(KeyCode::Char('o'), none), Some(Type('o'))),
            (key(KeyCode::Backspace, none), Some(Backspace)),
            (key(KeyCode::Enter, none), Some(Submit)),
            (key(KeyCode::Esc, none), Some(Cancel)),
            (key(KeyCode::Tab, none), None),
            (key(KeyCode::Down, none), Some(Down)),
            (key(KeyCode::Up, none), Some(Up)),
            (key(KeyCode::Char('n'), KeyModifiers::CONTROL), Some(Down)),
            (key(KeyCode::Char('p'), KeyModifiers::CONTROL), Some(Up)),
            (key(KeyCode::Char('x'), KeyModifiers::CONTROL), None),
        ];
        for view in View::ALL {
            for (k, want) in rows {
                assert_eq!(action(view, k, true), want, "{view:?} {k:?}");
            }
        }
    }

    #[test]
    fn releases_are_ignored() {
        let k = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        );
        assert_eq!(action(View::Queue, k, false), None);
    }
}
