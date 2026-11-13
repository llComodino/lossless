use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Clear, Paragraph};

const KEYS: [(&str, &str); 17] = [
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
    ("/", "Search view, edit query"),
    ("u", "update library"),
    ("o", "switch to the next profile"),
    ("?, q", "key help, quit"),
];

pub fn draw(f: &mut Frame) {
    let area = f.area();
    let w = 72.min(area.width);
    let h = (KEYS.len() as u16 + 2).min(area.height);
    let rect = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
    let lines: Vec<String> = KEYS.iter().map(|(k, a)| format!("{k:<26}{a}")).collect();
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines.join("\n")).block(Block::bordered().title(" keys ")),
        rect,
    );
}
