use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::widgets::Paragraph;

use crate::app::App;

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let text = match (&app.footer, app.connected) {
        (Some(t), _) => t.as_str(),
        (None, false) => "disconnected",
        (None, true) => "",
    };
    f.render_widget(Paragraph::new(text), area);
    f.render_widget(Paragraph::new("?: keys").alignment(Alignment::Right), area);
}
