use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::theme;

use crate::keys::{Keymap, Name};

const ROWS: [(&[Name], &str); 18] = {
    use Name::*;
    [
        (
            &[Down, Up, First, Last, HalfDown, HalfUp],
            "move, first/last, half page",
        ),
        (&[Left, Right], "column left/right; l on a track adds it"),
        (
            &[
                ViewQueue,
                ViewDirectories,
                ViewArtists,
                ViewSearch,
                NextView,
                PrevView,
            ],
            "switch view",
        ),
        (&[Enter], "Queue: play entry; else replace queue and play"),
        (&[Add, AddNext], "add at end / add after current"),
        (&[Remove, Clear], "remove selected / clear queue"),
        (&[MoveDown, MoveUp], "move selected entry down/up"),
        (&[Toggle], "toggle play/pause"),
        (&[Stop], "stop"),
        (&[Next, Previous], "next/prev"),
        (&[SeekForward, SeekBack], "seek +-5 s"),
        (&[Repeat, Random], "cycle repeat / toggle random"),
        (&[VolumeUp, VolumeDown], "volume +-5"),
        (&[EditQuery], "filter this tab (fuzzy)"),
        (&[ClearFilter], "clear the filter"),
        (&[Update], "update library"),
        (&[SwitchProfile], "switch to the next profile"),
        (&[Help, Quit], "key help, quit"),
    ]
};

/// Key column and description per row, from the bindings in effect: an
/// action's keys joined by spaces, a row's actions by `/`, `-` for an
/// unbound action; a row with nothing bound is left out.
pub(super) fn rows(map: &Keymap) -> Vec<(String, &'static str)> {
    ROWS.iter()
        .filter(|(names, _)| names.iter().any(|n| !map.keys(*n).is_empty()))
        .map(|(names, desc)| {
            let keys: Vec<String> = names
                .iter()
                .map(|n| match map.keys(*n) {
                    [] => "-".to_string(),
                    ks => ks
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" "),
                })
                .collect();
            (keys.join("/"), *desc)
        })
        .collect()
}

pub(super) fn lines(map: &Keymap) -> Vec<String> {
    let rows = rows(map);
    let width = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    rows.iter()
        .map(|(k, d)| format!("{k:<width$}{d}"))
        .collect()
}

pub fn draw(f: &mut Frame, map: &Keymap) {
    let area = f.area();
    let lines = lines(map);
    let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let w = (widest as u16 + 2).min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines.join("\n")).style(theme::base()).block(
            Block::bordered()
                .border_style(theme::border())
                .title(" keys "),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Key, Name};

    fn find<'a>(rows: &'a [(String, &str)], desc: &str) -> Option<&'a str> {
        rows.iter()
            .find(|(_, d)| *d == desc)
            .map(|(k, _)| k.as_str())
    }

    #[test]
    fn default_rows_show_today_keys() {
        let r = rows(&Keymap::default());
        assert_eq!(r.len(), 18);
        assert_eq!(
            find(&r, "move, first/last, half page"),
            Some("j/k/g/G/ctrl-d/ctrl-u")
        );
        assert_eq!(find(&r, "toggle play/pause"), Some("p space"));
    }

    #[test]
    fn remapped_keys_show_up() {
        let m = Keymap::with_overrides(vec![(Name::Quit, vec![Key::parse("Q").unwrap()])]).unwrap();
        assert_eq!(find(&rows(&m), "key help, quit"), Some("?/Q"));
    }

    #[test]
    fn fully_unbound_group_is_hidden() {
        let m =
            Keymap::with_overrides(vec![(Name::MoveDown, vec![]), (Name::MoveUp, vec![])]).unwrap();
        assert_eq!(find(&rows(&m), "move selected entry down/up"), None);
        let m = Keymap::with_overrides(vec![(Name::MoveUp, vec![])]).unwrap();
        assert_eq!(find(&rows(&m), "move selected entry down/up"), Some("J/-"));
    }
}
