use ratatui::Frame;
use ratatui::layout::Rect;

use super::queue::track_rows;
use crate::app::App;

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let s = &app.search;
    track_rows(&app.library.tracks, s.selected, s.offset, None, f, area);
}
