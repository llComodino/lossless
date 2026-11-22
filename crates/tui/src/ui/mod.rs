mod columns;
mod footer;
mod header;
mod help;
mod queue;
mod search;

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

use crate::app::{App, View};
use crate::theme;

pub fn draw(app: &App, f: &mut Frame) {
    f.render_widget(Block::new().style(theme::base()), f.area());
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(f.area());
    header::draw(app, Instant::now(), f, head);
    let block = Block::bordered()
        .border_style(theme::border())
        .title(tabs(app.view));
    let inner = block.inner(body);
    f.render_widget(block, body);
    match app.view {
        View::Queue => queue::draw(app, f, inner),
        View::Directories => columns::draw(&app.dirs, &app.filters[1], f, inner),
        View::Artists => columns::draw(&app.artists, &app.filters[2], f, inner),
        View::Search => search::draw(app, f, inner),
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
            Style::new().fg(theme::YELLOW).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme::GREY)
        };
        spans.push(Span::styled(format!(" {} {} ", i + 1, label(view)), style));
        spans.push(Span::styled("\u{2500}", theme::border()));
    }
    spans.pop();
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use lossless_proto::profile::Profile;
    use lossless_proto::{Entry, PlayState, Repeat, Status, StreamFormat, Track};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    use super::draw;
    use crate::app::{App, View};
    use crate::clock::Clock;
    use crate::filter::Filter;
    use crate::theme;

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
    fn selection_is_yellow_on_dark() {
        let mut a = app("default", None);
        a.queue_selected = 0;
        let (l, t) = render(&a);
        let y = l
            .iter()
            .position(|r| r.contains("1  Kenshi") || r.contains("1 Kenshi"))
            .unwrap();
        let b = t.backend().buffer();
        assert_eq!(b[(10, y as u16)].fg, theme::YELLOW);
        assert_eq!(b[(10, y as u16)].bg, theme::SELECTED_BG);
        assert_ne!(b[(10, y as u16 + 1)].bg, theme::SELECTED_BG);
    }

    #[test]
    fn playing_row_is_green() {
        let mut a = app("default", None);
        a.queue_selected = 0;
        let (l, t) = render(&a);
        let y = l
            .iter()
            .position(|r| r.contains('\u{25b6}') && r.contains("KICK BACK"))
            .unwrap() as u16;
        assert_eq!(t.backend().buffer()[(10, y)].fg, theme::GREEN);
        a.queue_selected = 1;
        let (_, t) = render(&a);
        let c = &t.backend().buffer()[(10, y)];
        assert_eq!((c.fg, c.bg), (theme::YELLOW, theme::SELECTED_BG));
        assert_eq!(t.backend().buffer()[(1, y)].symbol(), "\u{25b6}");
    }

    #[test]
    fn background_is_painted() {
        let (_, t) = render(&app("default", None));
        let b = t.backend().buffer();
        assert_eq!(b[(40, 20)].bg, theme::BG);
        assert_eq!(b[(40, 20)].fg, theme::FG);
        assert_eq!(b[(0, 4)].fg, theme::GREY);
    }

    #[test]
    fn header_accents_and_progress_bar() {
        let (_, t) = render(&app("default", Some(72)));
        let b = t.backend().buffer();
        assert_eq!(b[(1, 0)].fg, theme::NIAGARA);
        assert_eq!(b[(0, 2)].fg, theme::GREEN);
        assert_eq!(b[(60, 2)].fg, theme::GREY);
    }

    #[test]
    fn errors_are_red() {
        let mut a = app("default", None);
        a.footer = Some("error: x".into());
        let (_, t) = render(&a);
        assert_eq!(t.backend().buffer()[(0, 23)].fg, theme::RED);
        a.footer = Some("note".into());
        let (_, t) = render(&a);
        assert_ne!(t.backend().buffer()[(0, 23)].fg, theme::RED);
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
    fn long_footer_keeps_its_cut_end_and_the_hint() {
        let mut a = app("default", None);
        a.footer = Some(format!("error: {}", "d/".repeat(60)));
        let (l, _) = render(&a);
        let row = l[23].trim_end();
        assert!(row.ends_with("... ?: keys"), "{row:?}");
        let msg = row.trim_end_matches("?: keys").trim_end();
        assert!(
            msg.starts_with("error: d/d/") && msg.ends_with("..."),
            "{row:?}"
        );
    }

    #[test]
    fn help_lists_every_key() {
        let mut a = app("default", None);
        let (without, _) = render(&a);
        a.help = true;
        let (l, _) = render(&a);
        for (k, d) in super::help::KEYS {
            let row = super::help::row(k, d);
            assert!(
                l.iter().any(|r| r.contains(&row)),
                "row {row:?} missing:\n{}",
                l.join("\n")
            );
            assert!(!without.iter().any(|r| r.contains(&row)));
        }
        let keys: String = super::help::KEYS
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(" | ");
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
            "Esc",
            "u",
            "o",
            "?",
            "q",
        ] {
            assert!(keys.contains(k), "{k} not in the key table");
        }
    }

    fn sized(app: &App, w: u16, h: u16) -> (Vec<String>, Terminal<TestBackend>) {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(app, f)).unwrap();
        let buf = t.backend().buffer().clone();
        let lines = (0..h)
            .map(|y| {
                let mut row = String::new();
                let mut skip = 0;
                for x in 0..w {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let sym = buf[(x, y)].symbol();
                    skip = UnicodeWidthStr::width(sym).saturating_sub(1);
                    row.push_str(sym);
                }
                row
            })
            .collect();
        (lines, t)
    }

    fn col_of(row: &str, marker: &str) -> Option<usize> {
        row.find(marker).map(|i| UnicodeWidthStr::width(&row[..i]))
    }

    fn entry_dir(name: &str, path: &str) -> Entry {
        Entry::Dir {
            name: name.into(),
            path: path.into(),
        }
    }

    fn browsing(view: View) -> App {
        let mut a = app("default", None);
        a.view = view;
        let cols = match view {
            View::Artists => &mut a.artists,
            _ => &mut a.dirs,
        };
        cols.path = vec!["Rock".into()];
        cols.parent = vec![entry_dir("Jazz", "Jazz"), entry_dir("Rock", "Rock")];
        cols.items = vec![
            entry_dir("Album One", "Rock/Album One"),
            entry_dir("Two", "Rock/Two"),
        ];
        cols.selected = 1;
        cols.preview = (0..2)
            .map(|_| Entry::Track(track("Kenshi Yonezu", "PREVIEWTRACK")))
            .collect();
        a
    }

    #[test]
    fn directories_show_three_columns() {
        let a = browsing(View::Directories);
        let (l, t) = sized(&a, 100, 24);
        let row = l.iter().find(|r| r.contains("Two/")).expect("current row");
        let (p, c, v) = (
            col_of(row, "Rock/").unwrap(),
            col_of(row, "Two/").unwrap(),
            col_of(row, "Kenshi").unwrap(),
        );
        assert!(p < c && c < v, "{row:?}");
        assert!(row.contains("Jazz") || l.iter().any(|r| r.contains("Jazz/")));
        assert!(l.iter().any(|r| r.contains("Album One/")));
        let y = l.iter().position(|r| r.contains("Two/")).unwrap() as u16;
        assert_eq!(t.backend().buffer()[(c as u16, y)].fg, theme::YELLOW);
        assert_ne!(t.backend().buffer()[(v as u16, y)].fg, theme::YELLOW);
    }

    #[test]
    fn narrow_panes_show_only_the_current_column() {
        let a = browsing(View::Directories);
        let (l, _) = sized(&a, 50, 24);
        assert!(l.iter().any(|r| r.contains("Two/")));
        assert!(
            !l.iter()
                .any(|r| r.contains("Jazz") || r.contains("PREVIEWTRACK"))
        );
    }

    #[test]
    fn artists_show_artist_album_tracks() {
        let mut a = browsing(View::Artists);
        a.artists.path = vec!["Rock".into(), "Album One".into()];
        a.artists.parent = vec![
            entry_dir("Album One", "Album One"),
            entry_dir("Other", "Other"),
        ];
        a.artists.items = vec![Entry::Track(track("Kenshi Yonezu", "IRIS OUT"))];
        a.artists.selected = 0;
        a.artists.preview = vec![];
        let (l, _) = sized(&a, 100, 24);
        let row = l
            .iter()
            .find(|r| r.contains("Kenshi Yonezu - IRIS OUT"))
            .unwrap();
        assert!(
            col_of(row, "Album One/").is_none()
                || col_of(row, "Album One/") < col_of(row, "Kenshi")
        );
        assert!(l.iter().any(|r| r.contains("Album One/")));
        assert!(l.iter().any(|r| r.contains("3 Artists")));
    }

    #[test]
    fn search_lists_the_library() {
        let mut a = app("default", None);
        a.view = View::Search;
        a.library.replace(
            0,
            vec![track("Kenshi Yonezu", "IRIS OUT"), track("X", "Other")],
        );
        a.search.selected = 1;
        let (l, t) = sized(&a, 80, 24);
        let r = l
            .iter()
            .find(|r| r.contains("Kenshi Yonezu - IRIS OUT"))
            .unwrap();
        assert!(r.contains("2:31") && r.contains("24-bit/48 kHz"), "{r:?}");
        let y = l.iter().position(|r| r.contains("X - Other")).unwrap() as u16;
        assert_eq!(t.backend().buffer()[(10, y)].bg, theme::SELECTED_BG);
    }

    fn filtered(hits: Vec<usize>) -> Filter {
        Filter {
            query: "k".into(),
            editing: false,
            hits: Some(hits),
        }
    }

    #[test]
    fn filtered_rows_keep_their_real_numbers() {
        let mut a = app("default", None);
        a.filters[View::Queue.index()] = filtered(vec![1]);
        a.queue_selected = 0;
        let (l, t) = render(&a);
        let y = l
            .iter()
            .position(|r| r.contains("   2  Kenshi Yonezu - KICK BACK"))
            .unwrap();
        assert_eq!(t.backend().buffer()[(10, y as u16)].bg, theme::SELECTED_BG);
        assert!(!l.iter().any(|r| r.contains("Kenshi Yonezu - IRIS OUT (")));
        assert!(l[23].starts_with("/ k  1 of 2 "), "{:?}", l[23]);
        assert_eq!(t.backend().buffer()[(0, 23)].fg, theme::YELLOW);
    }

    #[test]
    fn columns_show_the_current_column_filtered() {
        let mut a = browsing(View::Directories);
        a.filters[View::Directories.index()] = filtered(vec![1]);
        a.dirs.selected = 0;
        let (l, t) = sized(&a, 100, 24);
        assert!(!l.iter().any(|r| r.contains("Album One/")));
        let y = l.iter().position(|r| r.contains("Two/")).unwrap() as u16;
        let x = col_of(&l[y as usize], "Two/").unwrap() as u16;
        assert_eq!(t.backend().buffer()[(x, y)].bg, theme::SELECTED_BG);
        assert!(l.iter().any(|r| r.contains("Jazz/")));
    }

    #[test]
    fn tiny_terminals_render_without_panicking() {
        for (w, h) in [(40, 12), (20, 6), (1, 1), (0, 0), (5, 0), (0, 5), (3, 3)] {
            for view in View::ALL {
                for help in [false, true] {
                    let mut a = browsing(view);
                    a.view = view;
                    a.help = help;
                    a.footer = Some("a long footer message that overflows".into());
                    a.library.replace(0, vec![track("A", "B")]);
                    a.queue_selected = 1;
                    sized(&a, w, h);
                }
            }
        }
    }

    #[test]
    fn long_header_lines_truncate_by_columns() {
        let mut a = app("default", Some(50));
        a.status.as_mut().unwrap().track = Some(track("\u{65e5}\u{672c}\u{8a9e}", &"x".repeat(80)));
        let (l, _) = sized(&a, 30, 12);
        for r in &l {
            assert!(UnicodeWidthStr::width(r.as_str()) <= 30, "{r:?}");
        }
    }

    #[test]
    fn wide_titles_keep_columns_aligned() {
        let titles = [
            "plain",
            "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30bf}\u{30a4}\u{30c8}\u{30eb}",
            "love \u{2764}\u{fe0f} you",
            "caf\u{e9}",
            "cafe\u{301} au lait",
            "\u{1f3b5}\u{1f3b6}",
        ];
        let mut a = app("default", None);
        a.queue = titles.iter().map(|t| track("Art", t)).collect();
        a.library.replace(0, a.queue.clone());
        a.view = View::Queue;
        let (q, _) = sized(&a, 80, 24);
        a.view = View::Search;
        let (s, _) = sized(&a, 80, 24);
        for lines in [&q, &s] {
            let rows: Vec<&String> = lines.iter().filter(|r| r.contains("Art - ")).collect();
            assert_eq!(rows.len(), titles.len(), "{lines:#?}");
            let want = col_of(rows[0], "2:31").unwrap();
            let fmt = col_of(rows[0], "24-bit").unwrap();
            for r in rows {
                assert_eq!(col_of(r, "2:31"), Some(want), "{r:?}");
                assert_eq!(col_of(r, "24-bit"), Some(fmt), "{r:?}");
            }
        }
        let mut b = browsing(View::Directories);
        let names = [
            "ascii",
            "\u{65e5}\u{672c}\u{8a9e}",
            "\u{2764}\u{fe0f}x",
            "e\u{301}e\u{301}e\u{301}",
            "\u{1f3b5}",
        ];
        b.dirs.items = names.iter().map(|n| entry_dir(n, n)).collect();
        b.dirs.preview = vec![Entry::Track(track("Zed", "PREV"))];
        let mut xs = Vec::new();
        for i in 0..names.len() {
            b.dirs.selected = i;
            let (l, _) = sized(&b, 100, 24);
            let row = l.iter().find(|r| r.contains("Zed - PREV")).unwrap();
            xs.push(col_of(row, "Zed - PREV").unwrap());
        }
        assert!(xs.iter().all(|x| *x == xs[0]), "{xs:?}");
    }
}
