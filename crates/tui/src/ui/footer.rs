use lossless_proto::fit;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use crate::app::App;

const HINT: &str = "?: keys";
/// Narrower than this the hint gives way to the message.
const HINT_MIN_WIDTH: u16 = 24;

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let text = match (&app.footer, app.connected) {
        (Some(t), _) => t.as_str(),
        (None, false) => "disconnected",
        (None, true) => "",
    };
    let hint = HINT.len() as u16;
    let message = if area.width >= HINT_MIN_WIDTH {
        let hint_area = Rect::new(area.right() - hint, area.y, hint, area.height);
        f.render_widget(Paragraph::new(HINT), hint_area);
        Rect::new(area.x, area.y, area.width - hint - 1, area.height)
    } else {
        area
    };
    f.render_widget(
        Paragraph::new(fit(text, usize::from(message.width))),
        message,
    );
}
