mod footer;
mod header;
mod help;
mod queue;

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::{App, View};

pub fn draw(app: &App, f: &mut Frame) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(f.area());
    header::draw(app, Instant::now(), f, head);
    let block = Block::bordered().title(tabs(app.view));
    let inner = block.inner(body);
    f.render_widget(block, body);
    match app.view {
        View::Queue => queue::draw(app, f, inner),
        view => f.render_widget(
            Paragraph::new(format!("{} (not implemented yet)", label(view))),
            inner,
        ),
    }
    footer::draw(app, f, foot);
    if app.help {
        help::draw(f);
    }
}

fn label(view: View) -> &'static str {
    match view {
        View::Queue => "Queue",
        View::Directories => "Directories",
        View::Artists => "Artists",
        View::Search => "Search",
    }
}

fn tabs(current: View) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, view) in View::ALL.into_iter().enumerate() {
        let style = if view == current {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!(" {} {} ", i + 1, label(view)), style));
        spans.push(Span::raw("\u{2500}"));
    }
    spans.pop();
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use lossless_proto::profile::Profile;
    use lossless_proto::{PlayState, Repeat, Status, StreamFormat, Track};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    use super::draw;
    use crate::app::App;
    use crate::clock::Clock;

    fn track(artist: &str, title: &str) -> Track {
        Track {
            path: format!("/m/{title}.flac"),
            title: Some(title.into()),
            artist: Some(artist.into()),
            album: Some("IRIS OUT".into()),
            track_number: None,
            disc_number: None,
            duration_ms: Some(151_000),
            format: Some(StreamFormat {
                rate: 48_000,
                bits: 24,
                channels: 2,
            }),
        }
    }

    fn app(profile: &str, volume: Option<u8>) -> App {
        let mut app = App::new(vec![Profile::parse(profile).unwrap()], 0, true);
        app.connected = true;
        app.queue = vec![
            track("Kenshi Yonezu", "IRIS OUT"),
            track("Kenshi Yonezu", "KICK BACK"),
        ];
        let status = Status {
            state: PlayState::Paused,
            index: Some(1),
            position_ms: 72_000,
            duration_ms: Some(151_000),
            output: Some(StreamFormat {
                rate: 96_000,
                bits: 24,
                channels: 2,
            }),
            track: Some(track("Kenshi Yonezu", "KICK BACK")),
            last_error: None,
            repeat: Repeat::Queue,
            random: false,
            volume,
        };
        app.clock = Clock::from_event(PlayState::Paused, 72_000, Some(151_000), Instant::now());
        app.status = Some(status);
        app
    }

    fn render(app: &App) -> (Vec<String>, Terminal<TestBackend>) {
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| draw(app, f)).unwrap();
        let lines = (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| t.backend().buffer()[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        (lines, t)
    }

    #[test]
    fn header_shows_state_modes_volume_format() {
        let (l, _) = render(&app("default", Some(72)));
        assert!(l[0].starts_with("[default]"), "{:?}", l[0]);
        for s in ["\u{23f8} paused", "repeat:queue", "random:off", "vol 72%"] {
            assert!(l[0].contains(s), "{s} in {:?}", l[0]);
        }
        assert!(l[0].trim_end().ends_with("24-bit/96 kHz"), "{:?}", l[0]);
        assert!(
            l[1].starts_with("Kenshi Yonezu - KICK BACK (IRIS OUT)"),
            "{:?}",
            l[1]
        );
        assert!(l[2].trim_end().ends_with("1:12 / 2:31"), "{:?}", l[2]);
    }

    #[test]
    fn header_names_the_profile() {
        let (l, _) = render(&app("pw", None));
        assert!(l[0].starts_with("[pw]"), "{:?}", l[0]);
    }

    #[test]
    fn header_hides_volume_without_mixer() {
        let (l, _) = render(&app("default", None));
        assert!(!l[0].contains("vol"), "{:?}", l[0]);
    }

    #[test]
    fn tabs_name_every_view() {
        let (l, _) = render(&app("default", None));
        let tabs = &l[3];
        for s in ["1 Queue", "2 Directories", "3 Artists", "4 Search"] {
            assert!(tabs.contains(s), "{s} in {tabs:?}");
        }
    }

    #[test]
    fn queue_marks_the_playing_entry() {
        let (l, _) = render(&app("default", None));
        let rows: Vec<&String> = l
            .iter()
            .filter(|r| r.contains("KICK BACK") && r.contains('\u{25b6}'))
            .collect();
        assert_eq!(rows.len(), 1, "{l:#?}");
        assert!(
            rows[0].contains("2:31") && rows[0].contains("24-bit/48 kHz"),
            "{:?}",
            rows[0]
        );
        let first = l
            .iter()
            .find(|r| r.contains("IRIS OUT") && r.contains("2:31") && !r.contains("KICK"))
            .unwrap();
        assert!(!first.contains('\u{25b6}'));
    }

    #[test]
    fn queue_selection_is_reversed() {
        let mut a = app("default", None);
        a.queue_selected = 0;
        let (l, t) = render(&a);
        let y = l
            .iter()
            .position(|r| r.contains("1  Kenshi") || r.contains("1 Kenshi"))
            .unwrap();
        assert!(
            t.backend().buffer()[(10, y as u16)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !t.backend().buffer()[(10, y as u16 + 1)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn footer_shows_errors() {
        let mut a = app("default", None);
        a.footer = Some("x.flac: decode failed, skipped".into());
        let (l, _) = render(&a);
        assert!(
            l[23].starts_with("x.flac: decode failed, skipped")
                || l[23].trim_start().starts_with("x.flac"),
            "{:?}",
            l[23]
        );
        assert!(l[23].trim_end().ends_with("?: keys"));
        a.footer = None;
        a.connected = false;
        let (l, _) = render(&a);
        assert!(l[23].contains("disconnected"), "{:?}", l[23]);
    }

    #[test]
    fn help_lists_every_key() {
        let mut a = app("default", None);
        a.help = true;
        let (l, _) = render(&a);
        let screen = l.join("\n");
        for k in [
            "j/k",
            "g/G",
            "Ctrl-d/Ctrl-u",
            "h/l",
            "1-4",
            "Tab/S-Tab",
            "Enter",
            "a / A",
            "d / D",
            "J/K",
            "p or Space",
            "s",
            ">/<",
            "f/b",
            "r / z",
            "+/-",
            "/",
            "u",
            "o",
            "?",
            "q",
        ] {
            assert!(screen.contains(k), "{k} missing:\n{screen}");
        }
    }
}
