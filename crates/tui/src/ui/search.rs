use ratatui::Frame;
use ratatui::layout::Rect;

use super::queue::track_rows;
use crate::app::{App, View};

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let s = &app.search;
    let rows = app.filters[View::Search.index()].visible(&app.library.tracks);
    track_rows(&rows, s.selected, s.offset, None, false, f, area);
}
