use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::Paragraph;

use super::queue::track_rows;
use crate::app::App;

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let [query, results] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let cursor = if app.search.editing { "_" } else { "" };
    f.render_widget(
        Paragraph::new(format!("/ {}{cursor}", app.search.query)),
        query,
    );
    let s = &app.search;
    track_rows(&s.results, s.selected, s.offset, None, f, results);
}
