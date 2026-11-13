use lossless_proto::{Entry, describe, fit};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::Columns;

const MIN_THREE: u16 = 60;

pub fn draw(cols: &Columns<Entry>, f: &mut Frame, area: Rect) {
    if area.width < MIN_THREE {
        list(&cols.items, Some(cols.selected), Style::new(), f, area);
        return;
    }
    let [parent, current, preview] = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(40),
        Constraint::Percentage(35),
    ])
    .spacing(1)
    .areas(area);
    let came_from = cols.path.last().and_then(|name| {
        cols.parent
            .iter()
            .position(|e| matches!(e, Entry::Dir { name: n, .. } if n == name))
    });
    list(
        &cols.parent,
        came_from,
        Style::new().add_modifier(Modifier::BOLD),
        f,
        parent,
    );
    list(&cols.items, Some(cols.selected), Style::new(), f, current);
    list(&cols.preview, None, Style::new(), f, preview);
}

fn label(e: &Entry) -> String {
    match e {
        Entry::Dir { name, .. } => format!("{name}/"),
        Entry::Track(t) => describe(t),
    }
}

fn list(items: &[Entry], selected: Option<usize>, mark: Style, f: &mut Frame, area: Rect) {
    let height = usize::from(area.height);
    let width = usize::from(area.width);
    let offset = selected.map_or(0, |s| (s + 1).saturating_sub(height));
    let style = if mark == Style::new() {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        mark
    };
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, e)| {
            let line = fit(&label(e), width);
            if selected == Some(i) {
                Line::styled(line, style)
            } else {
                Line::raw(line)
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}
