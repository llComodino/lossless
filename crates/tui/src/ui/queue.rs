use lossless_proto::{Track, clock, describe, fit, rate_str};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, View};
use crate::theme;

pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let playing = app.status.as_ref().and_then(|s| s.index);
    let rows = app.filters[View::Queue.index()].visible(&app.queue);
    track_rows(
        &rows,
        app.queue_selected,
        app.queue_offset,
        playing,
        true,
        f,
        area,
    );
}

/// Track rows as shown, `(real index, track)`: numbered by the real index
/// when `real_numbers`, else by position, the row at position `selected`
/// highlighted and the real index `playing` marked.
pub fn track_rows(
    rows: &[(usize, &Track)],
    selected: usize,
    offset: usize,
    playing: Option<usize>,
    real_numbers: bool,
    f: &mut Frame,
    area: Rect,
) {
    let height = usize::from(area.height);
    let width = usize::from(area.width);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(pos, &(i, t))| {
            let number = if real_numbers { i + 1 } else { pos + 1 };
            let mark = if playing == Some(i) { '\u{25b6}' } else { ' ' };
            let dur = t.duration_ms.map_or_else(|| "?".into(), clock);
            let fmt = t.format.as_ref().map_or_else(String::new, rate_str);
            let tail = format!("  {dur:>6}  {fmt:<14}");
            let title = width.saturating_sub(7 + tail.chars().count());
            let head = format!("{mark} {:>3}  {}", number, fit(&describe(t), title));
            let row = fit(&format!("{head}{tail}"), width);
            let (style, tail_style) = if pos == selected {
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
