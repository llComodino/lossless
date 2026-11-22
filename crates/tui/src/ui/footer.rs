use lossless_proto::fit;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::theme;

const HINT: &str = "?: keys";
/// Narrower than this the hint gives way to the message.
const HINT_MIN_WIDTH: u16 = 24;

/// The current view's query, while editing or with a filter active.
fn query(app: &App) -> Option<String> {
    let f = &app.filters[app.view.index()];
    if f.editing {
        Some(format!("/ {}_", f.query))
    } else {
        let n = f.hits.as_ref()?.len();
        let total = app.total(app.view);
        Some(format!("/ {}  {n} of {total}", f.query))
    }
}

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let query = query(app);
    let text = match (&app.footer, &query, app.connected) {
        (Some(t), ..) => t.as_str(),
        (None, Some(q), _) => q.as_str(),
        (None, None, false) => "disconnected",
        (None, None, true) => "",
    };
    let hint = HINT.len() as u16;
    let message = if area.width >= HINT_MIN_WIDTH {
        let hint_area = Rect::new(area.right() - hint, area.y, hint, area.height);
        f.render_widget(Paragraph::new(HINT).style(theme::dim()), hint_area);
        Rect::new(area.x, area.y, area.width - hint - 1, area.height)
    } else {
        area
    };
    let style = if text.starts_with("error: ") {
        Style::new().fg(theme::RED)
    } else if app.footer.is_none() && query.is_some() {
        Style::new().fg(theme::YELLOW)
    } else {
        Style::new()
    };
    f.render_widget(
        Paragraph::new(fit(text, usize::from(message.width))).style(style),
        message,
    );
}
