use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::theme;

pub(super) const KEYS: [(&str, &str); 18] = [
    ("j/k, g/G, Ctrl-d/Ctrl-u", "move, first/last, half page"),
    ("h/l", "column left/right; l on a track adds it"),
    ("1-4, Tab/S-Tab", "switch view"),
    ("Enter", "Queue: play entry; else replace queue and play"),
    ("a / A", "add at end / add after current"),
    ("d / D", "remove selected / clear queue"),
    ("J/K", "move selected entry down/up"),
    ("p or Space", "toggle play/pause"),
    ("s", "stop"),
    (">/<", "next/prev"),
    ("f/b", "seek +-5 s"),
    ("r / z", "cycle repeat / toggle random"),
    ("+/-", "volume +-5"),
    ("/", "filter this tab (fuzzy)"),
    ("Esc", "clear the filter"),
    ("u", "update library"),
    ("o", "switch to the next profile"),
    ("?, q", "key help, quit"),
];

pub(super) fn row(k: &str, a: &str) -> String {
    format!("{k:<26}{a}")
}

pub fn draw(f: &mut Frame) {
    let area = f.area();
    let widest = KEYS
        .iter()
        .map(|(k, a)| row(k, a).chars().count())
        .max()
        .unwrap_or(0);
    let w = (widest as u16 + 2).min(area.width);
    let h = (KEYS.len() as u16 + 2).min(area.height);
    let rect = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
    let lines: Vec<String> = KEYS.iter().map(|(k, a)| row(k, a)).collect();
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
