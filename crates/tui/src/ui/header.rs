use std::time::Instant;

use lossless_proto::{PlayState, Repeat, clock, describe, fit, rate_str};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::theme;

/// `s` with each `(needle, skip)` match accented from `skip` bytes in.
fn accent<'a>(s: &'a str, marks: &[(String, usize)]) -> Line<'a> {
    let mut ranges: Vec<(usize, usize)> = marks
        .iter()
        .filter_map(|(n, skip)| s.find(n.as_str()).map(|i| (i + skip, i + n.len())))
        .collect();
    ranges.sort_unstable();
    let mut spans = Vec::new();
    let mut at = 0;
    for (a, b) in ranges {
        if a < at {
            continue;
        }
        spans.push(Span::raw(&s[at..a]));
        spans.push(Span::styled(&s[a..b], theme::accent()));
        at = b;
    }
    spans.push(Span::raw(&s[at..]));
    Line::from(spans)
}

pub fn draw(app: &App, now: Instant, f: &mut Frame, area: Rect) {
    let [l1, l2, l3] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    let name = app.profiles.get(app.profile).map_or("", |p| p.name());
    let Some(s) = &app.status else {
        let head = format!("[{name}]");
        f.render_widget(Paragraph::new(accent(&head, &[(head.clone(), 0)])), l1);
        return;
    };
    let state = match s.state {
        PlayState::Playing => "\u{25b6} playing",
        PlayState::Paused => "\u{23f8} paused",
        PlayState::Stopped => "\u{25a0} stopped",
    };
    let repeat = match s.repeat {
        Repeat::Off => "off",
        Repeat::Queue => "queue",
        Repeat::One => "one",
    };
    let random = if s.random { "on" } else { "off" };
    let mut line = format!("[{name}]  {state}   repeat:{repeat}  random:{random}");
    if let Some(v) = s.volume {
        line.push_str(&format!("  vol {v}%"));
    }
    let width = usize::from(l1.width);
    let output = s
        .output
        .as_ref()
        .map(rate_str)
        .filter(|o| width > o.len() + 40);
    let room = width.saturating_sub(output.as_ref().map_or(0, |o| o.len() + 1));
    let mut marks = vec![(format!("[{name}]"), 0)];
    if repeat != "off" {
        marks.push((format!("repeat:{repeat}"), "repeat:".len()));
    }
    if s.random {
        marks.push(("random:on".into(), 0));
    }
    let shown = fit(&line, room);
    f.render_widget(Paragraph::new(accent(&shown, &marks)), l1);
    if let Some(o) = output {
        let w = o.len() as u16;
        let right = Rect::new(l1.right() - w, l1.y, w, 1);
        f.render_widget(Paragraph::new(o).style(theme::dim()), right);
    }
    if let Some(t) = &s.track {
        f.render_widget(Paragraph::new(fit(&describe(t), usize::from(l2.width))), l2);
    }
    let pos = app.clock.position(now);
    let dur = s.duration_ms;
    let label = format!(
        " {} / {}",
        clock(pos),
        dur.map_or_else(|| "?".into(), clock)
    );
    let width = usize::from(l3.width).saturating_sub(label.chars().count());
    let total = usize::from(l3.width);
    let filled = dur.filter(|&d| d > 0).map_or(0, |d| {
        (u128::from(pos.min(d)) * width as u128 / u128::from(d)) as usize
    });
    let bar = format!(
        "{}{}{label}",
        "\u{2501}".repeat(filled),
        "\u{2500}".repeat(width - filled)
    );
    let bar = fit(&bar, total);
    let on = bar.chars().take_while(|&c| c == '\u{2501}').count();
    let off = bar
        .chars()
        .skip(on)
        .take_while(|&c| c == '\u{2500}')
        .count();
    let cut = |n: usize| bar.char_indices().nth(n).map_or(bar.len(), |(i, _)| i);
    let (a, b) = (cut(on), cut(on + off));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(&bar[..a], Style::new().fg(theme::GREEN)),
            Span::styled(&bar[a..b], theme::border()),
            Span::raw(&bar[b..]),
        ])),
        l3,
    );
}
