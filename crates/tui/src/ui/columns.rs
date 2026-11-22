use lossless_proto::{Entry, describe, fit};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::Columns;
use crate::filter::Filter;
use crate::theme;

const MIN_THREE: u16 = 60;

/// The current column is shown through `filter`.
pub fn draw(cols: &Columns<Entry>, filter: &Filter, f: &mut Frame, area: Rect) {
    let current_items: Vec<&Entry> = filter
        .visible(&cols.items)
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    if area.width < MIN_THREE {
        list(
            &current_items,
            Some(cols.selected),
            cols.offset,
            theme::selected(),
            f,
            area,
        );
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
    let rows = usize::from(parent.height);
    let parent_items: Vec<&Entry> = cols.parent.iter().collect();
    list(
        &parent_items,
        came_from,
        came_from.map_or(0, |s| (s + 1).saturating_sub(rows)),
        Style::new().fg(theme::FG).add_modifier(Modifier::BOLD),
        f,
        parent,
    );
    list(
        &current_items,
        Some(cols.selected),
        cols.offset,
        theme::selected(),
        f,
        current,
    );
    let preview_items: Vec<&Entry> = cols.preview.iter().collect();
    list(&preview_items, None, 0, theme::selected(), f, preview);
}

fn label(e: &Entry) -> String {
    match e {
        Entry::Dir { name, .. } => format!("{name}/"),
        Entry::Track(t) => describe(t),
    }
}

fn list(
    items: &[&Entry],
    selected: Option<usize>,
    offset: usize,
    style: Style,
    f: &mut Frame,
    area: Rect,
) {
    let height = usize::from(area.height);
    let width = usize::from(area.width);
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
