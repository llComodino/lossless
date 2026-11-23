use lossless_proto::fit;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, View};
use crate::config::Mode;
use crate::theme;

/// `key  name  mode  command`, names and modes padded to their column.
pub fn draw(app: &App, f: &mut Frame, area: Rect) {
    let list = &app.scripts;
    let rows = app.filters[View::Scripts.index()].visible(&list.items);
    let keys: Vec<String> = list
        .items
        .iter()
        .map(|s| s.key.map_or_else(|| "-".into(), |k| k.to_string()))
        .collect();
    let kw = keys.iter().map(|k| k.chars().count()).max().unwrap_or(1);
    let nw = list
        .items
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);
    let width = usize::from(area.width);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(list.offset)
        .take(usize::from(area.height))
        .map(|(pos, &(i, s))| {
            let mode = match s.mode {
                Mode::Modal => "modal",
                Mode::Terminal => "terminal",
            };
            let head = format!(" {:<kw$}  {:<nw$}  {mode:<8}  ", keys[i], s.name);
            let (style, dim) = if pos == list.selected {
                (theme::selected(), theme::selected())
            } else {
                (theme::base(), theme::dim())
            };
            let used = head.chars().count();
            if used >= width {
                return Line::styled(fit(&head, width), style);
            }
            Line::from(vec![
                Span::styled(head, style),
                Span::styled(fit(&s.command, width - used), dim),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}
