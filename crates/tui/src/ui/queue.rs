use lossless_proto::{Track, clock, describe, fit, rate_str};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::theme;

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let playing = app.status.as_ref().and_then(|s| s.index);
    track_rows(
        &app.queue,
        app.queue_selected,
        app.queue_offset,
        playing,
        f,
        area,
    );
}

/// Numbered track rows (title, duration, format), `selected` highlighted and
/// `playing` marked.
pub fn track_rows(
    tracks: &[Track],
    selected: usize,
    offset: usize,
    playing: Option<usize>,
    f: &mut Frame,
    area: Rect,
) {
    let height = usize::from(area.height);
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
            let head = format!("{mark} {:>3}  {}", i + 1, fit(&describe(t), title));
            let row = fit(&format!("{head}{tail}"), width);
            let (style, tail_style) = if i == selected {
                (theme::selected(), theme::selected())
            } else if playing == Some(i) {
                (theme::playing(), theme::dim())
            } else {
                (theme::base(), theme::dim())
            };
            match row.strip_suffix(&tail) {
                Some(head) => Line::from(vec![
                    Span::styled(head.to_owned(), style),
                    Span::styled(tail.clone(), tail_style),
                ]),
                None => Line::styled(row, style),
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}
