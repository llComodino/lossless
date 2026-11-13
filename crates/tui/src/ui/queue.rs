use lossless_proto::{Track, clock, describe, fit, rate_str};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::App;

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let playing = app.status.as_ref().and_then(|s| s.index);
    track_rows(&app.queue, app.queue_selected, playing, f, area);
}

/// Numbered track rows (title, duration, format), `selected` reversed and
/// `playing` marked.
pub fn track_rows(
    tracks: &[Track],
    selected: usize,
    playing: Option<usize>,
    f: &mut Frame,
    area: Rect,
) {
    let height = usize::from(area.height);
    let offset = (selected + 1).saturating_sub(height);
    let width = usize::from(area.width);
    let lines: Vec<Line> = tracks
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, t)| {
            let mark = if playing == Some(i) { '\u{25b6}' } else { ' ' };
            let dur = t.duration_ms.map_or_else(|| "?".into(), clock);
            let fmt = t.format.as_ref().map_or_else(String::new, rate_str);
            let tail = format!("  {dur:>6}  {fmt:<14}");
            let title = width.saturating_sub(7 + tail.chars().count());
            let row = format!("{mark} {:>3}  {}{tail}", i + 1, fit(&describe(t), title));
            let style = if i == selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            Line::styled(fit(&row, width), style)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}
