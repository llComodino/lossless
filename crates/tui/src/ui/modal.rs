use lossless_proto::fit;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::Modal;
use crate::theme;

/// A box over 80% of the screen: output bottom-anchored, scrolled back by
/// `scroll` lines, then the input line or how the script ended.
pub fn draw(m: &Modal, f: &mut Frame) {
    let area = f.area();
    let (w, h) = (area.width * 4 / 5, area.height * 4 / 5);
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let block = Block::bordered()
        .border_style(theme::border())
        .title(format!(" {} ", m.name));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block.style(theme::base()), rect);
    if inner.height == 0 {
        return;
    }
    let width = usize::from(inner.width);
    let rows = usize::from(inner.height - 1);
    let end = m.lines.len().saturating_sub(m.scroll);
    let start = end.saturating_sub(rows);
    let lines: Vec<Line> = m
        .lines
        .range(start..end)
        .map(|l| Line::raw(fit(l, width)))
        .collect();
    let last = match &m.done {
        Some(d) => Line::styled(fit(&format!("[{d}, any key closes]"), width), theme::dim()),
        None => Line::raw(fit(&format!("> {}_", m.input), width)),
    };
    let out = Rect::new(
        inner.x,
        inner.y + inner.height - 1 - lines.len() as u16,
        inner.width,
        lines.len() as u16,
    );
    f.render_widget(Paragraph::new(lines), out);
    f.render_widget(
        Paragraph::new(last),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}
