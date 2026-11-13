use std::collections::HashMap;
use std::time::Instant;

use crossterm::event::{KeyEvent, KeyEventKind};
use lossless_proto::profile::Profile;
use lossless_proto::{
    Body, Command, Entry, Event, Outcome, PlayState, Repeat, ServerMessage, Status, Track,
};

use crate::clock::Clock;
use crate::conn::{Msg, Resync};
pub use crate::keys::View;
use crate::keys::{self, Action};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Col {
    Parent,
    Current,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Replace,
    Add(Option<usize>),
}

/// What a reply is for; replies to ids not in `App::pending` are dropped.
#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    Subscribe,
    Status,
    Queue,
    /// A column of Directories or Artists at `path`; `select` names the
    /// entry to land on (the one `h` came out of).
    List {
        view: View,
        path: Vec<String>,
        column: Col,
        select: Option<String>,
    },
    /// `find` whose result paths become a `replace` or `add`.
    Then(Op),
    Search,
    Done,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Send(Command, Pending),
    Reconnect,
    Switch,
}

/// Miller columns: `items` lists `path`, `parent` the path without its
/// last name (empty at the top), `preview` the selected item.
/// Artists and albums are `Entry::Dir` named after them.
#[derive(Clone, Debug, PartialEq)]
pub struct Columns<T> {
    pub path: Vec<String>,
    pub parent: Vec<T>,
    pub items: Vec<T>,
    pub selected: usize,
    pub preview: Vec<Entry>,
}

impl<T> Default for Columns<T> {
    fn default() -> Self {
        Self {
            path: Vec::new(),
            parent: Vec::new(),
            items: Vec::new(),
            selected: 0,
            preview: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Search {
    pub query: String,
    pub editing: bool,
    pub results: Vec<Track>,
    pub selected: usize,
}

pub struct App {
    pub profiles: Vec<Profile>,
    pub profile: usize,
    pub switchable: bool,
    pub generation: u32,
    pub view: View,
    pub status: Option<Status>,
    pub queue: Vec<Track>,
    pub queue_selected: usize,
    pub clock: Clock,
    pub dirs: Columns<Entry>,
    pub artists: Columns<Entry>,
    pub search: Search,
    pub footer: Option<String>,
    pub connected: bool,
    pub help: bool,
    pub quit: bool,
    /// Rows visible in the list, set from the terminal size.
    pub page: usize,
    pending: HashMap<u64, Pending>,
}

fn stopped() -> Clock {
    Clock::from_event(PlayState::Stopped, 0, None, Instant::now())
}

impl App {
    pub fn new(profiles: Vec<Profile>, start: usize, switchable: bool) -> App {
        App {
            profiles,
            profile: start,
            switchable,
            generation: 0,
            view: View::Queue,
            status: None,
            queue: Vec::new(),
            queue_selected: 0,
            clock: stopped(),
            dirs: Columns::default(),
            artists: Columns::default(),
            search: Search::default(),
            footer: None,
            connected: false,
            help: false,
            quit: false,
            page: 10,
            pending: HashMap::new(),
        }
    }

    pub fn expect(&mut self, id: u64, pending: Pending) {
        self.pending.insert(id, pending);
    }

    /// After a connect, reconnect or switch: drops everything the old
    /// connection knew except the browse paths; the Subscribe reply
    /// refetches the columns.
    pub fn connected(&mut self, generation: u32, resync: &Resync) {
        self.reset(generation);
        self.connected = true;
        self.footer = None;
        self.expect(resync.subscribe, Pending::Subscribe);
        self.expect(resync.status, Pending::Status);
        self.expect(resync.queue, Pending::Queue);
    }

    /// The next key retries the same socket (`Out::Reconnect`).
    pub fn connect_failed(&mut self, generation: u32) {
        self.reset(generation);
        self.connected = false;
        self.footer = Some(match self.profiles.get(self.profile) {
            Some(p) if self.switchable => format!("{}: not running (o to switch)", p.name()),
            _ => DISCONNECTED.into(),
        });
    }

    fn reset(&mut self, generation: u32) {
        self.generation = generation;
        self.pending.clear();
        self.status = None;
        self.clock = stopped();
        self.queue.clear();
        self.queue_selected = 0;
        for cols in [&mut self.dirs, &mut self.artists] {
            cols.parent.clear();
            cols.items.clear();
            cols.preview.clear();
            cols.selected = 0;
        }
        self.search.results.clear();
        self.search.selected = 0;
    }

    fn columns(&mut self, view: View) -> &mut Columns<Entry> {
        match view {
            View::Artists => &mut self.artists,
            _ => &mut self.dirs,
        }
    }

    fn cursor(&mut self) -> (&mut usize, usize) {
        match self.view {
            View::Queue => (&mut self.queue_selected, self.queue.len()),
            View::Directories => (&mut self.dirs.selected, self.dirs.items.len()),
            View::Artists => (&mut self.artists.selected, self.artists.items.len()),
            View::Search => (&mut self.search.selected, self.search.results.len()),
        }
    }

    fn key(&mut self, key: KeyEvent, now: Instant) -> Vec<Out> {
        if key.kind == KeyEventKind::Release {
            return vec![];
        }
        self.footer = None;
        if self.help {
            self.help = false;
            return vec![];
        }
        let action = keys::action(self.view, key, self.search.editing);
        if !self.connected {
            return match action {
                Some(Action::Quit) => self.act(Action::Quit, now),
                Some(Action::SwitchProfile) => self.act(Action::SwitchProfile, now),
                _ => vec![Out::Reconnect],
            };
        }
        action.map_or_else(Vec::new, |a| self.act(a, now))
    }

    fn act(&mut self, action: Action, now: Instant) -> Vec<Out> {
        use Action::*;
        let half = (self.page / 2).max(1);
        match action {
            Down => self.select(|s| s.saturating_add(1)),
            Up => self.select(|s| s.saturating_sub(1)),
            First => self.select(|_| 0),
            Last => self.select(|_| usize::MAX),
            HalfDown => self.select(|s| s.saturating_add(half)),
            HalfUp => self.select(|s| s.saturating_sub(half)),
            Left => self.left(),
            Right => self.right(),
            Show(v) => self.show(v),
            NextView => self.show(View::ALL[(self.view.index() + 1) % 4]),
            PrevView => self.show(View::ALL[(self.view.index() + 3) % 4]),
            Enter if self.view == View::Queue => {
                self.on_queue(|i| Command::Play { index: Some(i) })
            }
            Enter => self.take(Op::Replace),
            Add => self.take(Op::Add(None)),
            AddNext => self.take(Op::Add(
                self.status.as_ref().and_then(|s| s.index).map(|i| i + 1),
            )),
            Remove => self.on_queue(|i| Command::Remove { indices: vec![i] }),
            Clear => send(Command::Clear),
            MoveDown => self.shift(true),
            MoveUp => self.shift(false),
            Toggle => send(Command::Toggle),
            Stop => send(Command::Stop),
            Next => send(Command::Next),
            Previous => send(Command::Previous),
            SeekForward => send(Command::Seek {
                ms: self.clock.position(now).saturating_add(SEEK_MS),
            }),
            SeekBack => send(Command::Seek {
                ms: self.clock.position(now).saturating_sub(SEEK_MS),
            }),
            Repeat => send(Command::SetMode {
                repeat: Some(next_repeat(self.status.as_ref())),
                random: None,
            }),
            Random => send(Command::SetMode {
                repeat: None,
                random: Some(!self.status.as_ref().is_some_and(|s| s.random)),
            }),
            VolumeUp => self.volume(|v| v.saturating_add(VOLUME_STEP).min(100)),
            VolumeDown => self.volume(|v| v.saturating_sub(VOLUME_STEP)),
            EditQuery => {
                self.view = View::Search;
                self.search.editing = true;
                vec![]
            }
            Update => send(Command::Update),
            SwitchProfile if !self.switchable => {
                self.footer = Some("--socket given: no profiles".into());
                vec![]
            }
            SwitchProfile => {
                self.profile = (self.profile + 1) % self.profiles.len().max(1);
                vec![Out::Switch]
            }
            Help => {
                self.help = true;
                vec![]
            }
            Quit => {
                self.quit = true;
                vec![]
            }
            Type(c) => {
                self.search.query.push(c);
                vec![]
            }
            Backspace => {
                self.search.query.pop();
                vec![]
            }
            Submit => {
                self.search.editing = false;
                if self.search.query.trim().is_empty() {
                    return vec![];
                }
                vec![Out::Send(
                    Command::Search {
                        query: self.search.query.clone(),
                    },
                    Pending::Search,
                )]
            }
            Cancel => {
                self.search.editing = false;
                vec![]
            }
        }
    }

    fn show(&mut self, view: View) -> Vec<Out> {
        self.view = view;
        vec![]
    }

    fn select(&mut self, to: impl FnOnce(usize) -> usize) -> Vec<Out> {
        let (sel, len) = self.cursor();
        let new = clamp(to(*sel), len);
        if new == *sel {
            return vec![];
        }
        *sel = new;
        match self.view {
            View::Directories | View::Artists => self.preview(self.view),
            _ => vec![],
        }
    }

    fn on_queue(&self, cmd: impl FnOnce(usize) -> Command) -> Vec<Out> {
        if self.queue_selected < self.queue.len() {
            send(cmd(self.queue_selected))
        } else {
            vec![]
        }
    }

    fn shift(&mut self, down: bool) -> Vec<Out> {
        let from = self.queue_selected;
        let to = if down { from + 1 } else { from.wrapping_sub(1) };
        if from >= self.queue.len() || to >= self.queue.len() {
            return vec![];
        }
        self.queue_selected = to;
        send(Command::Move { from, to })
    }

    fn volume(&mut self, f: impl FnOnce(u8) -> u8) -> Vec<Out> {
        match self.status.as_ref().and_then(|s| s.volume) {
            Some(v) => send(Command::SetVolume { percent: f(v) }),
            None => {
                self.footer = Some("no volume control".into());
                vec![]
            }
        }
    }

    /// Replace with, or add, the selection of a browse or search view.
    fn take(&mut self, op: Op) -> Vec<Out> {
        let path = match self.view {
            View::Queue => return vec![],
            View::Search => match self.search.results.get(self.search.selected) {
                Some(t) => t.path.clone(),
                None => return vec![],
            },
            view => {
                let cols = self.columns(view);
                match (cols.items.get(cols.selected), view, cols.path.as_slice()) {
                    (Some(Entry::Track(t)), ..) => t.path.clone(),
                    (Some(Entry::Dir { path, .. }), View::Directories, _) => path.clone(),
                    (Some(Entry::Dir { name, .. }), _, []) => {
                        return find(Some(name.clone()), None, op);
                    }
                    (Some(Entry::Dir { name, .. }), _, [artist, ..]) => {
                        return find(Some(artist.clone()), Some(name.clone()), op);
                    }
                    (None, ..) => return vec![],
                }
            }
        };
        vec![done(op.command(vec![path]))]
    }

    fn right(&mut self) -> Vec<Out> {
        let view = self.view;
        let cols = self.columns(view);
        match cols.items.get(cols.selected) {
            Some(Entry::Dir { name, .. }) => {
                cols.path.push(name.clone());
                cols.parent = std::mem::replace(&mut cols.items, std::mem::take(&mut cols.preview));
                cols.selected = 0;
                vec![list(view, cols.path.clone(), Col::Current, None)]
            }
            Some(Entry::Track(t)) => send(Command::Add {
                paths: vec![t.path.clone()],
                at: None,
            }),
            None => vec![],
        }
    }

    fn left(&mut self) -> Vec<Out> {
        let view = self.view;
        let cols = self.columns(view);
        let Some(name) = cols.path.pop() else {
            return vec![];
        };
        cols.preview = std::mem::take(&mut cols.items);
        let loaded = !cols.parent.is_empty();
        cols.items = std::mem::take(&mut cols.parent);
        cols.selected = cols
            .items
            .iter()
            .position(|e| matches!(e, Entry::Dir { name: n, .. } if *n == name))
            .unwrap_or(0);
        let mut outs = Vec::new();
        if !loaded {
            outs.push(list(view, cols.path.clone(), Col::Current, Some(name)));
        }
        outs.extend(parent_of(view, &cols.path));
        outs
    }

    /// Fetches the preview of the selected folder, artist or album.
    fn preview(&mut self, view: View) -> Vec<Out> {
        let cols = self.columns(view);
        cols.preview.clear();
        match cols.items.get(cols.selected) {
            Some(Entry::Dir { name, .. }) => {
                let mut path = cols.path.clone();
                path.push(name.clone());
                vec![list(view, path, Col::Preview, None)]
            }
            _ => vec![],
        }
    }

    fn refetch(&mut self) -> Vec<Out> {
        let mut outs = Vec::new();
        for v in [View::Directories, View::Artists] {
            let path = self.columns(v).path.clone();
            outs.extend(parent_of(v, &path));
            outs.push(list(v, path, Col::Current, None));
        }
        outs
    }

    fn reply(&mut self, id: u64, result: Outcome, now: Instant) -> Vec<Out> {
        let Some(pending) = self.pending.remove(&id) else {
            return vec![];
        };
        let body = match result {
            Outcome::Ok(body) => body,
            Outcome::Err(e) => {
                self.footer = Some(format!("error: {e}"));
                return vec![];
            }
        };
        match (pending, body) {
            (Pending::Subscribe, _) => self.refetch(),
            (Pending::Status, Body::Status(s)) => {
                self.clock = Clock::from_event(s.state, s.position_ms, s.duration_ms, now);
                self.status = Some(*s);
                vec![]
            }
            (Pending::Queue, Body::Queue(q)) => {
                self.queue = q;
                self.queue_selected = clamp(self.queue_selected, self.queue.len());
                vec![]
            }
            (
                Pending::List {
                    view,
                    path,
                    column,
                    select,
                },
                body,
            ) => self.listed(view, path, column, select, body),
            (Pending::Then(op), Body::Tracks(tracks)) if !tracks.is_empty() => {
                send(op.command(tracks.into_iter().map(|t| t.path).collect()))
            }
            (Pending::Search, Body::Tracks(tracks)) => {
                self.search.results = tracks;
                self.search.selected = clamp(self.search.selected, self.search.results.len());
                vec![]
            }
            _ => vec![],
        }
    }

    fn listed(
        &mut self,
        view: View,
        path: Vec<String>,
        column: Col,
        select: Option<String>,
        body: Body,
    ) -> Vec<Out> {
        let entries = match body {
            Body::Entries(e) => e,
            Body::Names(names) => names
                .into_iter()
                .map(|n| Entry::Dir {
                    path: n.clone(),
                    name: n,
                })
                .collect(),
            Body::Tracks(t) => t.into_iter().map(Entry::Track).collect(),
            _ => return vec![],
        };
        let cols = self.columns(view);
        match column {
            Col::Current if cols.path == path => {
                cols.items = entries;
                let found = select.and_then(|s| {
                    cols.items
                        .iter()
                        .position(|e| matches!(e, Entry::Dir { name, .. } if *name == s))
                });
                cols.selected = clamp(found.unwrap_or(cols.selected), cols.items.len());
                self.preview(view)
            }
            Col::Preview => {
                let current = match cols.items.get(cols.selected) {
                    Some(Entry::Dir { name, .. }) => {
                        path.split_last() == Some((name, cols.path.as_slice()))
                    }
                    _ => false,
                };
                if current {
                    cols.preview = entries;
                }
                vec![]
            }
            Col::Parent => {
                if cols.path.split_last().map(|(_, p)| p) == Some(path.as_slice()) {
                    cols.parent = entries;
                }
                vec![]
            }
            Col::Current => vec![],
        }
    }

    fn event(&mut self, event: Event, now: Instant) -> Vec<Out> {
        match event {
            Event::State { state, position_ms } => {
                let duration = self.status.as_ref().and_then(|s| s.duration_ms);
                if let Some(s) = &mut self.status {
                    s.state = state;
                    s.position_ms = position_ms;
                }
                self.clock = Clock::from_event(state, position_ms, duration, now);
                vec![]
            }
            Event::TrackChanged { index } => {
                if let Some(s) = &mut self.status {
                    s.index = index;
                }
                vec![Out::Send(Command::Status, Pending::Status)]
            }
            Event::QueueChanged => vec![Out::Send(Command::Queue, Pending::Queue)],
            Event::Mode { repeat, random } => {
                if let Some(s) = &mut self.status {
                    s.repeat = repeat;
                    s.random = random;
                }
                vec![]
            }
            Event::Volume { percent } => {
                if let Some(s) = &mut self.status {
                    s.volume = Some(percent);
                }
                vec![]
            }
            Event::LibraryChanged { .. } => self.refetch(),
            Event::Error(e) => {
                self.footer = Some(format!("error: {e}"));
                vec![]
            }
        }
    }
}

impl Op {
    fn command(self, paths: Vec<String>) -> Command {
        match self {
            Op::Replace => Command::Replace { paths },
            Op::Add(at) => Command::Add { paths, at },
        }
    }
}

const DISCONNECTED: &str = "disconnected: press any key to reconnect";
const SEEK_MS: u64 = 5_000;
const VOLUME_STEP: u8 = 5;

fn next_repeat(status: Option<&Status>) -> Repeat {
    match status.map_or(Repeat::Off, |s| s.repeat) {
        Repeat::Off => Repeat::Queue,
        Repeat::Queue => Repeat::One,
        Repeat::One => Repeat::Off,
    }
}

fn clamp(i: usize, len: usize) -> usize {
    i.min(len.saturating_sub(1))
}

fn done(cmd: Command) -> Out {
    Out::Send(cmd, Pending::Done)
}

fn send(cmd: Command) -> Vec<Out> {
    vec![done(cmd)]
}

fn find(artist: Option<String>, album: Option<String>, op: Op) -> Vec<Out> {
    vec![Out::Send(
        Command::Find { artist, album },
        Pending::Then(op),
    )]
}

fn parent_of(view: View, path: &[String]) -> Option<Out> {
    let (_, up) = path.split_last()?;
    Some(list(view, up.to_vec(), Col::Parent, None))
}

/// The request that lists a Directories or Artists column at `path`.
fn list(view: View, path: Vec<String>, column: Col, select: Option<String>) -> Out {
    let cmd = match (view, path.as_slice()) {
        (View::Directories, p) => Command::Browse { path: p.join("/") },
        (_, []) => Command::Artists,
        (_, [artist]) => Command::Albums {
            artist: Some(artist.clone()),
        },
        (_, [artist, album, ..]) => Command::Find {
            artist: Some(artist.clone()),
            album: Some(album.clone()),
        },
    };
    Out::Send(
        cmd,
        Pending::List {
            view,
            path,
            column,
            select,
        },
    )
}

pub fn update(app: &mut App, msg: Msg, now: Instant) -> Vec<Out> {
    match msg {
        Msg::Server(g, _) | Msg::Disconnected(g) if g != app.generation => vec![],
        Msg::Server(_, ServerMessage::Reply { id, result }) => app.reply(id, result, now),
        Msg::Server(_, ServerMessage::Event { event }) => app.event(event, now),
        Msg::Disconnected(_) => {
            app.connected = false;
            app.pending.clear();
            app.footer = Some(DISCONNECTED.into());
            vec![]
        }
        Msg::Key(key) => app.key(key, now),
        Msg::Resize | Msg::Tick => vec![],
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use lossless_proto::{ErrorCode, WireError};

    use super::*;

    fn track(path: &str) -> Track {
        Track {
            path: path.into(),
            title: None,
            artist: None,
            album: None,
            track_number: None,
            disc_number: None,
            duration_ms: Some(60_000),
            format: None,
        }
    }

    fn dir(name: &str, path: &str) -> Entry {
        Entry::Dir {
            name: name.into(),
            path: path.into(),
        }
    }

    fn named(name: &str) -> Entry {
        dir(name, name)
    }

    fn status(index: Option<usize>, volume: Option<u8>) -> Status {
        Status {
            state: PlayState::Paused,
            index,
            position_ms: 10_000,
            duration_ms: Some(60_000),
            output: None,
            track: None,
            last_error: None,
            repeat: Repeat::Off,
            random: false,
            volume,
        }
    }

    fn profiles(names: &[&str]) -> Vec<Profile> {
        names.iter().map(|n| Profile::parse(n).unwrap()).collect()
    }

    fn app() -> App {
        let mut app = App::new(profiles(&["default", "pw", "usb"]), 0, true);
        app.generation = 1;
        app.connected = true;
        app.status = Some(status(Some(1), Some(50)));
        app.clock = Clock::from_event(PlayState::Paused, 10_000, Some(60_000), Instant::now());
        app.queue = vec![track("q0"), track("q1"), track("q2")];
        app.queue_selected = 1;
        app.dirs.items = vec![dir("a", "a"), dir("b", "b"), Entry::Track(track("x.flac"))];
        app.artists.items = vec![named("Ann"), named("Bob")];
        app.search.results = vec![track("hit.flac")];
        app
    }

    fn press(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(c: char) -> Msg {
        press(KeyCode::Char(c))
    }

    fn ctrl(c: char) -> Msg {
        Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn reply(generation: u32, id: u64, result: Outcome) -> Msg {
        Msg::Server(generation, ServerMessage::Reply { id, result })
    }

    fn event(generation: u32, event: Event) -> Msg {
        Msg::Server(generation, ServerMessage::Event { event })
    }

    fn run(app: &mut App, msg: Msg) -> Vec<Out> {
        update(app, msg, Instant::now())
    }

    fn done(c: Command) -> Out {
        Out::Send(c, Pending::Done)
    }

    fn paths(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    fn list(view: View, path: &[&str], column: Col, select: Option<&str>) -> Pending {
        Pending::List {
            view,
            path: paths(path),
            column,
            select: select.map(Into::into),
        }
    }

    #[test]
    fn global_keys_send_their_command_in_every_view() {
        use Command::*;
        let rows = [
            ('p', Toggle),
            (' ', Toggle),
            ('s', Stop),
            ('>', Next),
            ('<', Previous),
            ('f', Seek { ms: 15_000 }),
            ('b', Seek { ms: 5_000 }),
            (
                'r',
                SetMode {
                    repeat: Some(Repeat::Queue),
                    random: None,
                },
            ),
            (
                'z',
                SetMode {
                    repeat: None,
                    random: Some(true),
                },
            ),
            ('+', SetVolume { percent: 55 }),
            ('-', SetVolume { percent: 45 }),
            ('u', Update),
        ];
        for view in View::ALL {
            for (c, cmd) in &rows {
                let mut app = app();
                app.view = view;
                assert_eq!(run(&mut app, ch(*c)), [done(cmd.clone())], "{view:?} {c:?}");
            }
        }
    }

    #[test]
    fn view_keys_send_their_commands() {
        use Command::*;
        let at = |p: &str, at| Add {
            paths: paths(&[p]),
            at,
        };
        let find = |artist: &str, op| {
            Out::Send(
                Find {
                    artist: Some(artist.into()),
                    album: None,
                },
                Pending::Then(op),
            )
        };
        let rows: Vec<(View, Msg, Vec<Out>)> = vec![
            (
                View::Queue,
                press(KeyCode::Enter),
                vec![done(Play { index: Some(1) })],
            ),
            (
                View::Queue,
                ch('d'),
                vec![done(Remove { indices: vec![1] })],
            ),
            (View::Queue, ch('D'), vec![done(Clear)]),
            (View::Queue, ch('J'), vec![done(Move { from: 1, to: 2 })]),
            (View::Queue, ch('K'), vec![done(Move { from: 1, to: 0 })]),
            (View::Queue, ch('a'), vec![]),
            (View::Queue, ch('A'), vec![]),
            (View::Queue, ch('l'), vec![]),
            (View::Queue, ch('j'), vec![]),
            (
                View::Directories,
                press(KeyCode::Enter),
                vec![done(Replace {
                    paths: paths(&["a"]),
                })],
            ),
            (View::Directories, ch('a'), vec![done(at("a", None))]),
            (View::Directories, ch('A'), vec![done(at("a", Some(2)))]),
            (
                View::Directories,
                ch('l'),
                vec![Out::Send(
                    Browse { path: "a".into() },
                    list(View::Directories, &["a"], Col::Current, None),
                )],
            ),
            (View::Directories, ch('h'), vec![]),
            (
                View::Directories,
                ch('j'),
                vec![Out::Send(
                    Browse { path: "b".into() },
                    list(View::Directories, &["b"], Col::Preview, None),
                )],
            ),
            (View::Directories, ch('k'), vec![]),
            (View::Directories, ch('d'), vec![]),
            (View::Directories, ch('J'), vec![]),
            (
                View::Artists,
                press(KeyCode::Enter),
                vec![find("Ann", Op::Replace)],
            ),
            (View::Artists, ch('a'), vec![find("Ann", Op::Add(None))]),
            (View::Artists, ch('A'), vec![find("Ann", Op::Add(Some(2)))]),
            (
                View::Artists,
                ch('l'),
                vec![Out::Send(
                    Albums {
                        artist: Some("Ann".into()),
                    },
                    list(View::Artists, &["Ann"], Col::Current, None),
                )],
            ),
            (
                View::Artists,
                ch('j'),
                vec![Out::Send(
                    Albums {
                        artist: Some("Bob".into()),
                    },
                    list(View::Artists, &["Bob"], Col::Preview, None),
                )],
            ),
            (View::Artists, ch('D'), vec![]),
            (
                View::Search,
                press(KeyCode::Enter),
                vec![done(Replace {
                    paths: paths(&["hit.flac"]),
                })],
            ),
            (View::Search, ch('a'), vec![done(at("hit.flac", None))]),
            (View::Search, ch('A'), vec![done(at("hit.flac", Some(2)))]),
            (View::Search, ch('l'), vec![]),
            (View::Search, ch('d'), vec![]),
        ];
        for (view, msg, want) in rows {
            let mut app = app();
            app.view = view;
            let label = format!(
                "{view:?} {:?}",
                match &msg {
                    Msg::Key(k) => k.code,
                    _ => unreachable!(),
                }
            );
            assert_eq!(run(&mut app, msg), want, "{label}");
        }
    }

    #[test]
    fn deeper_columns_send_their_commands() {
        use Command::*;
        let mut app = app();
        app.view = View::Artists;
        app.artists.path = paths(&["Ann"]);
        app.artists.items = vec![named("Red")];
        let find = Find {
            artist: Some("Ann".into()),
            album: Some("Red".into()),
        };
        assert_eq!(
            run(&mut app, ch('a')),
            [Out::Send(find.clone(), Pending::Then(Op::Add(None)))]
        );
        assert_eq!(
            run(&mut app, ch('l')),
            [Out::Send(
                find,
                list(View::Artists, &["Ann", "Red"], Col::Current, None)
            )]
        );
        app.artists.items = vec![Entry::Track(track("t.flac"))];
        let add = done(Add {
            paths: paths(&["t.flac"]),
            at: None,
        });
        assert_eq!(run(&mut app, ch('l')), std::slice::from_ref(&add));
        assert_eq!(run(&mut app, ch('a')), [add]);
        assert_eq!(
            run(&mut app, ch('h')),
            [Out::Send(
                Artists,
                list(View::Artists, &[], Col::Parent, None)
            )]
        );
        let mut app = super::tests::app();
        app.view = View::Directories;
        app.dirs.selected = 2;
        assert_eq!(
            run(&mut app, ch('l')),
            [done(Add {
                paths: paths(&["x.flac"]),
                at: None
            })]
        );
    }

    #[test]
    fn find_replies_become_replace_or_add() {
        let mut app = app();
        app.expect(7, Pending::Then(Op::Add(Some(2))));
        app.expect(8, Pending::Then(Op::Replace));
        let tracks = Outcome::Ok(Body::Tracks(vec![track("1"), track("2")]));
        assert_eq!(
            run(&mut app, reply(1, 7, tracks.clone())),
            [done(Command::Add {
                paths: paths(&["1", "2"]),
                at: Some(2)
            })]
        );
        assert_eq!(
            run(&mut app, reply(1, 8, tracks)),
            [done(Command::Replace {
                paths: paths(&["1", "2"])
            })]
        );
        assert_eq!(run(&mut app, reply(1, 8, Outcome::Ok(Body::Done))), []);
    }

    #[test]
    fn movement_keys_clamp_and_page() {
        let mut app = app();
        app.page = 4;
        let steps = [
            (ch('j'), 2),
            (ch('j'), 2),
            (ch('g'), 0),
            (ch('k'), 0),
            (ch('G'), 2),
            (ctrl('u'), 0),
            (ctrl('d'), 2),
        ];
        for (msg, want) in steps {
            assert_eq!(run(&mut app, msg), []);
            assert_eq!(app.queue_selected, want);
        }
        app.page = 1;
        assert_eq!(run(&mut app, ctrl('u')), []);
        assert_eq!(app.queue_selected, 1);
    }

    #[test]
    fn views_switch_by_number_and_tab() {
        let mut app = app();
        run(&mut app, ch('3'));
        assert_eq!(app.view, View::Artists);
        run(&mut app, press(KeyCode::Tab));
        assert_eq!(app.view, View::Search);
        run(&mut app, press(KeyCode::Tab));
        assert_eq!(app.view, View::Queue);
        run(&mut app, press(KeyCode::BackTab));
        assert_eq!(app.view, View::Search);
        assert!(!app.search.editing);
    }

    #[test]
    fn the_query_is_edited_locally_and_sent_on_enter() {
        let mut app = app();
        assert_eq!(run(&mut app, ch('/')), []);
        assert_eq!(app.view, View::Search);
        assert!(app.search.editing);
        for c in "iriq".chars() {
            assert_eq!(run(&mut app, ch(c)), []);
        }
        assert_eq!(run(&mut app, press(KeyCode::Backspace)), []);
        assert_eq!(run(&mut app, ch('s')), []);
        assert_eq!(
            run(&mut app, press(KeyCode::Enter)),
            [Out::Send(
                Command::Search {
                    query: "iris".into()
                },
                Pending::Search
            )]
        );
        assert!(!app.search.editing);
        run(&mut app, ch('/'));
        assert_eq!(run(&mut app, ch('o')), []);
        assert_eq!(run(&mut app, press(KeyCode::Esc)), []);
        assert!(!app.search.editing);
        assert_eq!(app.search.query, "iriso");
        app.expect(3, Pending::Search);
        app.search.selected = 4;
        let hits = Outcome::Ok(Body::Tracks(vec![track("1"), track("2")]));
        assert_eq!(run(&mut app, reply(1, 3, hits)), []);
        assert_eq!(app.search.results.len(), 2);
        assert_eq!(app.search.selected, 1);
    }

    #[test]
    fn help_quit_and_volume_without_mixer() {
        let mut app = app();
        run(&mut app, ch('?'));
        assert!(app.help);
        assert_eq!(run(&mut app, ch('s')), []);
        assert!(!app.help);
        app.status.as_mut().unwrap().volume = None;
        assert_eq!(run(&mut app, ch('+')), []);
        assert_eq!(app.footer.as_deref(), Some("no volume control"));
        app.status.as_mut().unwrap().volume = Some(98);
        assert_eq!(
            run(&mut app, ch('+')),
            [done(Command::SetVolume { percent: 100 })]
        );
        assert_eq!(app.footer, None);
        app.status.as_mut().unwrap().volume = Some(3);
        assert_eq!(
            run(&mut app, ch('-')),
            [done(Command::SetVolume { percent: 0 })]
        );
        app.status.as_mut().unwrap().repeat = Repeat::One;
        assert_eq!(
            run(&mut app, ch('r')),
            [done(Command::SetMode {
                repeat: Some(Repeat::Off),
                random: None
            })]
        );
        run(&mut app, ch('q'));
        assert!(app.quit);
    }

    #[test]
    fn play_next_inserts_after_current() {
        let mut app = app();
        app.view = View::Search;
        let add = |at| {
            done(Command::Add {
                paths: paths(&["hit.flac"]),
                at,
            })
        };
        assert_eq!(run(&mut app, ch('A')), [add(Some(2))]);
        app.status = Some(status(None, None));
        assert_eq!(run(&mut app, ch('A')), [add(None)]);
        app.status = None;
        assert_eq!(run(&mut app, ch('A')), [add(None)]);
    }

    #[test]
    fn selection_clamps_when_the_queue_shrinks() {
        let mut app = app();
        app.queue_selected = 2;
        assert_eq!(
            run(&mut app, event(1, Event::QueueChanged)),
            [Out::Send(Command::Queue, Pending::Queue)]
        );
        app.expect(5, Pending::Queue);
        run(
            &mut app,
            reply(1, 5, Outcome::Ok(Body::Queue(vec![track("q0")]))),
        );
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue_selected, 0);
        app.expect(6, Pending::Queue);
        run(&mut app, reply(1, 6, Outcome::Ok(Body::Queue(vec![]))));
        assert_eq!(app.queue_selected, 0);
        app.view = View::Queue;
        assert_eq!(run(&mut app, ch('d')), []);
        assert_eq!(run(&mut app, press(KeyCode::Enter)), []);
    }

    #[test]
    fn stale_browse_replies_are_ignored() {
        let mut app = app();
        app.view = View::Directories;
        app.dirs.preview = vec![dir("b", "a/b")];
        let [Out::Send(cmd, first)] = &run(&mut app, ch('l'))[..] else {
            panic!("one browse")
        };
        assert_eq!(cmd, &Command::Browse { path: "a".into() });
        app.expect(1, first.clone());
        assert_eq!(app.dirs.items, [dir("b", "a/b")]);
        let [Out::Send(cmd, second)] = &run(&mut app, ch('l'))[..] else {
            panic!("one browse")
        };
        assert_eq!(cmd, &Command::Browse { path: "a/b".into() });
        app.expect(2, second.clone());
        let inner = vec![Entry::Track(track("a/b/t.flac"))];
        assert_eq!(
            run(
                &mut app,
                reply(1, 2, Outcome::Ok(Body::Entries(inner.clone())))
            ),
            []
        );
        assert_eq!(app.dirs.items, inner);
        let outer = vec![dir("b", "a/b"), dir("c", "a/c")];
        assert_eq!(
            run(&mut app, reply(1, 1, Outcome::Ok(Body::Entries(outer)))),
            []
        );
        assert_eq!(app.dirs.items, inner);
        assert_eq!(app.dirs.path, paths(&["a", "b"]));
    }

    #[test]
    fn left_lands_on_the_folder_it_came_from() {
        let mut app = app();
        app.view = View::Directories;
        app.dirs.path = paths(&["a"]);
        app.dirs.items = vec![Entry::Track(track("a/t.flac"))];
        let outs = run(&mut app, ch('h'));
        assert_eq!(
            outs,
            [Out::Send(
                Command::Browse { path: "".into() },
                list(View::Directories, &[], Col::Current, Some("a"))
            )]
        );
        assert_eq!(app.dirs.preview, [Entry::Track(track("a/t.flac"))]);
        app.expect(4, list(View::Directories, &[], Col::Current, Some("a")));
        let top = vec![named("0"), dir("a", "a")];
        assert_eq!(
            run(&mut app, reply(1, 4, Outcome::Ok(Body::Entries(top)))),
            [Out::Send(
                Command::Browse { path: "a".into() },
                list(View::Directories, &["a"], Col::Preview, None)
            )]
        );
        assert_eq!(app.dirs.selected, 1);
        app.expect(5, list(View::Directories, &["a"], Col::Preview, None));
        let inner = vec![Entry::Track(track("a/t.flac"))];
        run(
            &mut app,
            reply(1, 5, Outcome::Ok(Body::Entries(inner.clone()))),
        );
        assert_eq!(app.dirs.preview, inner);
    }

    #[test]
    fn right_moves_current_to_parent() {
        let mut app = app();
        app.view = View::Directories;
        app.dirs.preview = vec![dir("b", "a/b")];
        run(&mut app, ch('l'));
        assert_eq!(
            app.dirs.parent,
            [dir("a", "a"), dir("b", "b"), Entry::Track(track("x.flac"))]
        );
        assert_eq!(app.dirs.items, [dir("b", "a/b")]);
    }

    #[test]
    fn left_requests_the_grandparent() {
        let mut app = app();
        app.view = View::Directories;
        app.dirs.path = paths(&["a", "b"]);
        app.dirs.parent = vec![dir("b", "a/b"), dir("c", "a/c")];
        app.dirs.items = vec![Entry::Track(track("a/b/t.flac"))];
        let outs = run(&mut app, ch('h'));
        assert_eq!(
            outs,
            [Out::Send(
                Command::Browse { path: "".into() },
                list(View::Directories, &[], Col::Parent, None)
            )]
        );
        assert_eq!(app.dirs.items, [dir("b", "a/b"), dir("c", "a/c")]);
        assert_eq!(app.dirs.selected, 0);
        assert_eq!(app.dirs.preview, [Entry::Track(track("a/b/t.flac"))]);
        assert!(app.dirs.parent.is_empty());
        app.expect(7, list(View::Directories, &[], Col::Parent, None));
        let top = vec![named("0"), dir("a", "a")];
        run(
            &mut app,
            reply(1, 7, Outcome::Ok(Body::Entries(top.clone()))),
        );
        assert_eq!(app.dirs.parent, top);
    }

    #[test]
    fn artists_parent_follows_depth() {
        let mut app = app();
        app.view = View::Artists;
        app.artists.path = paths(&["Ann", "Alb"]);
        app.artists.parent = vec![named("Alb")];
        app.artists.items = vec![Entry::Track(track("t"))];
        assert_eq!(
            run(&mut app, ch('h')),
            [Out::Send(
                Command::Artists,
                list(View::Artists, &[], Col::Parent, None)
            )]
        );
        assert!(run(&mut app, ch('h')).len() <= 1);
        assert!(app.artists.parent.is_empty());
    }

    #[test]
    fn stale_parent_replies_are_ignored() {
        let mut app = app();
        app.view = View::Directories;
        app.dirs.path = paths(&["a", "b"]);
        app.expect(8, list(View::Directories, &["a"], Col::Parent, None));
        app.expect(9, list(View::Directories, &[], Col::Parent, None));
        let stale = vec![named("zzz")];
        run(&mut app, reply(1, 9, Outcome::Ok(Body::Entries(stale))));
        assert!(app.dirs.parent.is_empty());
        let good = vec![dir("b", "a/b")];
        run(
            &mut app,
            reply(1, 8, Outcome::Ok(Body::Entries(good.clone()))),
        );
        assert_eq!(app.dirs.parent, good);
        app.dirs.path = paths(&["a"]);
        app.expect(10, list(View::Directories, &["a"], Col::Parent, None));
        run(
            &mut app,
            reply(1, 10, Outcome::Ok(Body::Entries(vec![named("q")]))),
        );
        assert_eq!(app.dirs.parent, good);
    }

    #[test]
    fn refetch_also_fetches_the_parent() {
        let mut app = app();
        app.dirs.path = paths(&["a", "b"]);
        let outs = run(&mut app, event(1, Event::LibraryChanged { tracks: 0 }));
        assert!(
            outs.contains(&Out::Send(
                Command::Browse { path: "a".into() },
                list(View::Directories, &["a"], Col::Parent, None)
            )),
            "{outs:?}"
        );
    }

    #[test]
    fn events_update_status_and_clock() {
        let mut app = app();
        let t = Instant::now();
        update(
            &mut app,
            event(
                1,
                Event::State {
                    state: PlayState::Playing,
                    position_ms: 2_000,
                },
            ),
            t,
        );
        assert_eq!(app.status.as_ref().unwrap().state, PlayState::Playing);
        assert_eq!(
            app.clock
                .position(t + std::time::Duration::from_millis(500)),
            2_500
        );
        assert_eq!(
            run(&mut app, event(1, Event::TrackChanged { index: Some(2) })),
            [Out::Send(Command::Status, Pending::Status)]
        );
        run(
            &mut app,
            event(
                1,
                Event::Mode {
                    repeat: Repeat::One,
                    random: true,
                },
            ),
        );
        run(&mut app, event(1, Event::Volume { percent: 7 }));
        let s = app.status.as_ref().unwrap();
        assert_eq!(
            (s.index, s.repeat, s.random, s.volume),
            (Some(2), Repeat::One, true, Some(7))
        );
        app.expect(9, Pending::Status);
        let mut fresh = status(Some(0), None);
        fresh.position_ms = 30_000;
        run(
            &mut app,
            reply(1, 9, Outcome::Ok(Body::Status(Box::new(fresh.clone())))),
        );
        assert_eq!(app.status, Some(fresh));
        assert_eq!(app.clock.position(t), 30_000);
    }

    #[test]
    fn errors_show_in_the_footer() {
        let mut app = app();
        let err = WireError {
            code: ErrorCode::NotFound,
            message: "x.flac: not found".into(),
        };
        app.expect(4, Pending::Done);
        assert_eq!(run(&mut app, reply(1, 4, Outcome::Err(err.clone()))), []);
        assert_eq!(app.footer.as_deref(), Some("error: x.flac: not found"));
        run(&mut app, ch('j'));
        assert_eq!(app.footer, None);
        run(&mut app, event(1, Event::Error(err.clone())));
        assert_eq!(app.footer.as_deref(), Some("error: x.flac: not found"));
        run(&mut app, ch('j'));
        run(&mut app, reply(1, 99, Outcome::Err(err)));
        assert_eq!(app.footer, None);
    }

    #[test]
    fn disconnect_then_key_reconnects() {
        let mut app = app();
        assert_eq!(run(&mut app, Msg::Disconnected(0)), []);
        assert!(app.connected);
        assert_eq!(run(&mut app, Msg::Disconnected(1)), []);
        assert!(!app.connected);
        assert_eq!(
            app.footer.as_deref(),
            Some("disconnected: press any key to reconnect")
        );
        assert_eq!(run(&mut app, ch('p')), [Out::Reconnect]);
        assert_eq!(app.footer, None);
        app.connected(
            2,
            &Resync {
                subscribe: 10,
                status: 11,
                queue: 12,
            },
        );
        assert!(app.connected);
        assert_eq!(app.generation, 2);
        assert_eq!(run(&mut app, ch('p')), [done(Command::Toggle)]);
        app.connected = false;
        run(&mut app, ch('q'));
        assert!(app.quit);
    }

    #[test]
    fn o_cycles_profiles_and_wraps() {
        let mut app = app();
        app.profile = 1;
        assert_eq!(run(&mut app, ch('o')), [Out::Switch]);
        assert_eq!(app.profile, 2);
        assert_eq!(run(&mut app, ch('o')), [Out::Switch]);
        assert_eq!(app.profile, 0);
        app.view = View::Search;
        run(&mut app, ch('/'));
        assert_eq!(run(&mut app, ch('o')), []);
        assert_eq!(app.profile, 0);
        let mut app = App::new(profiles(&["default"]), 0, false);
        app.connected = true;
        assert_eq!(run(&mut app, ch('o')), []);
        assert_eq!(app.footer.as_deref(), Some("--socket given: no profiles"));
    }

    #[test]
    fn switching_drops_the_old_daemons_replies() {
        let mut app = app();
        app.dirs.path = paths(&["a"]);
        app.artists.path = paths(&["Ann"]);
        app.expect(3, Pending::Queue);
        assert_eq!(run(&mut app, ch('o')), [Out::Switch]);
        let resync = Resync {
            subscribe: 10,
            status: 11,
            queue: 12,
        };
        app.connected(2, &resync);
        assert_eq!(app.status, None);
        assert!(app.queue.is_empty());
        assert!(app.dirs.items.is_empty());
        assert_eq!(app.dirs.path, paths(&["a"]));
        let old = Outcome::Ok(Body::Queue(vec![track("old")]));
        assert_eq!(run(&mut app, reply(1, 12, old.clone())), []);
        assert_eq!(run(&mut app, event(1, Event::QueueChanged)), []);
        assert_eq!(run(&mut app, reply(2, 3, old)), []);
        assert!(app.queue.is_empty());
        assert_eq!(
            run(&mut app, reply(2, 10, Outcome::Ok(Body::Done))),
            [
                Out::Send(
                    Command::Browse { path: "".into() },
                    list(View::Directories, &[], Col::Parent, None)
                ),
                Out::Send(
                    Command::Browse { path: "a".into() },
                    list(View::Directories, &["a"], Col::Current, None)
                ),
                Out::Send(
                    Command::Artists,
                    list(View::Artists, &[], Col::Parent, None)
                ),
                Out::Send(
                    Command::Albums {
                        artist: Some("Ann".into())
                    },
                    list(View::Artists, &["Ann"], Col::Current, None)
                ),
            ]
        );
        let new = Outcome::Ok(Body::Queue(vec![track("new")]));
        run(&mut app, reply(2, 12, new));
        assert_eq!(app.queue, [track("new")]);
        run(
            &mut app,
            reply(
                2,
                11,
                Outcome::Ok(Body::Status(Box::new(status(None, None)))),
            ),
        );
        assert_eq!(app.status, Some(status(None, None)));
    }

    #[test]
    fn a_profile_that_is_down_shows_in_the_footer() {
        let mut app = app();
        assert_eq!(run(&mut app, ch('o')), [Out::Switch]);
        app.connect_failed(2);
        assert_eq!(app.generation, 2);
        assert!(!app.connected);
        assert_eq!(app.status, None);
        assert_eq!(app.footer.as_deref(), Some("pw: not running (o to switch)"));
        assert_eq!(run(&mut app, ch('j')), [Out::Reconnect]);
        assert_eq!(app.profile, 1);
        app.connect_failed(3);
        assert_eq!(run(&mut app, ch('o')), [Out::Switch]);
        assert_eq!(app.profile, 2);
        let mut app = App::new(profiles(&["default"]), 0, false);
        app.connect_failed(2);
        assert_eq!(
            app.footer.as_deref(),
            Some("disconnected: press any key to reconnect")
        );
    }
}
