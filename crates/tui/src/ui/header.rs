use std::time::Instant;

use lossless_proto::{PlayState, Repeat, clock, describe, rate_str};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::widgets::Paragraph;

use crate::app::App;

pub fn draw(app: &App, now: Instant, f: &mut Frame, area: Rect) {
    let [l1, l2, l3] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    let name = app.profiles.get(app.profile).map_or("", |p| p.name());
    let Some(s) = &app.status else {
        f.render_widget(Paragraph::new(format!("[{name}]")), l1);
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
    f.render_widget(Paragraph::new(line), l1);
    if let Some(o) = &s.output {
        f.render_widget(Paragraph::new(rate_str(o)).alignment(Alignment::Right), l1);
    }
    if let Some(t) = &s.track {
        f.render_widget(Paragraph::new(describe(t)), l2);
    }
    let pos = app.clock.position(now);
    let dur = s.duration_ms;
    let label = format!(
        " {} / {}",
        clock(pos),
        dur.map_or_else(|| "?".into(), clock)
    );
    let width = usize::from(l3.width).saturating_sub(label.chars().count());
    let filled = dur.filter(|&d| d > 0).map_or(0, |d| {
        (u128::from(pos.min(d)) * width as u128 / u128::from(d)) as usize
    });
    let bar = format!(
        "{}{}{label}",
        "\u{2501}".repeat(filled),
        "\u{2500}".repeat(width - filled)
    );
    f.render_widget(Paragraph::new(bar), l3);
}
