use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Instant;

use crossterm::event::{KeyEvent, KeyEventKind};
use lossless_proto::profile::Profile;
use lossless_proto::{
    Body, Command, Entry, Event, Outcome, PlayState, Repeat, ServerMessage, Status, Track, describe,
};

use crate::clock::Clock;
use crate::conn::{Msg, Resync};
use crate::filter::{Filter, Ranker};
pub use crate::keys::View;
use crate::keys::{self, Action};
use crate::local::{Delta, Local};

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
    /// A column of Directories at `path`; `select` names the
    /// entry to land on (the one `h` came out of).
    List {
        view: View,
        path: Vec<String>,
        column: Col,
        select: Option<String>,
    },
    Tracks,
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
/// Artists and albums are `Entry::Dir` named after them, built from the
/// library copy.
#[derive(Clone, Debug, PartialEq)]
pub struct Columns<T> {
    pub path: Vec<String>,
    pub parent: Vec<T>,
    pub items: Vec<T>,
    pub selected: usize,
    /// First visible row of `items`.
    pub offset: usize,
    pub preview: Vec<Entry>,
}

impl<T> Default for Columns<T> {
    fn default() -> Self {
        Self {
            path: Vec::new(),
            parent: Vec::new(),
            items: Vec::new(),
            selected: 0,
            offset: 0,
            preview: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Search {
    pub selected: usize,
    pub offset: usize,
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
    pub queue_offset: usize,
    pub clock: Clock,
    pub dirs: Columns<Entry>,
    pub artists: Columns<Entry>,
    pub search: Search,
    pub library: Local,
    /// One per view, by `View::index()`; each view's `selected` and
    /// `offset` are positions in its filtered list.
    pub filters: [Filter; 4],
    ranker: Ranker,
    pub footer: Option<String>,
    pub connected: bool,
    pub help: bool,
    pub quit: bool,
    /// Rows visible in the list, set from the terminal size.
    pub page: usize,
    /// The next queue reply selects the playing entry.
    first_queue: bool,
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
            queue_offset: 0,
            clock: stopped(),
            dirs: Columns::default(),
            artists: Columns::default(),
            search: Search::default(),
            library: Local::default(),
            filters: Default::default(),
            ranker: Ranker::new(),
            footer: None,
            connected: false,
            help: false,
            quit: false,
            page: 10,
            first_queue: false,
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
        self.first_queue = true;
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

    /// A lost daemon plays nothing: the clock freezes where it was.
    fn stop_clock(&mut self, now: Instant) {
        let position = self.clock.position(now);
        let duration = self.status.as_ref().and_then(|s| s.duration_ms);
        self.clock = Clock::from_event(PlayState::Stopped, position, duration, now);
        if let Some(s) = &mut self.status {
            s.state = PlayState::Stopped;
            s.position_ms = position;
        }
    }

    fn reset(&mut self, generation: u32) {
        self.generation = generation;
        self.pending.clear();
        self.status = None;
        self.clock = stopped();
        self.queue.clear();
        self.queue_selected = 0;
        self.queue_offset = 0;
        for cols in [&mut self.dirs, &mut self.artists] {
            cols.parent.clear();
            cols.items.clear();
            cols.preview.clear();
            cols.selected = 0;
            cols.offset = 0;
        }
        self.search.selected = 0;
        self.search.offset = 0;
        self.library.clear();
        for view in View::ALL {
            self.rank(view);
        }
    }

    /// Keeps each list's window on its selection, moving it only when the
    /// selection would leave it, and clamps it after a shrink or resize.
    pub fn follow(&mut self) {
        let page = self.page.max(1);
        let [q, d, a, s] = View::ALL.map(|v| self.visible(v));
        scroll(&mut self.queue_offset, self.queue_selected, q, page);
        scroll(&mut self.dirs.offset, self.dirs.selected, d, page);
        scroll(&mut self.artists.offset, self.artists.selected, a, page);
        scroll(&mut self.search.offset, self.search.selected, s, page);
    }

    fn columns(&mut self, view: View) -> &mut Columns<Entry> {
        match view {
            View::Artists => &mut self.artists,
            _ => &mut self.dirs,
        }
    }

    fn cols(&self, view: View) -> &Columns<Entry> {
        match view {
            View::Artists => &self.artists,
            _ => &self.dirs,
        }
    }

    /// Length of `view`'s unfiltered list.
    pub fn total(&self, view: View) -> usize {
        match view {
            View::Queue => self.queue.len(),
            View::Directories => self.dirs.items.len(),
            View::Artists => self.artists.items.len(),
            View::Search => self.library.tracks.len(),
        }
    }

    fn visible(&self, view: View) -> usize {
        self.filters[view.index()].len(self.total(view))
    }

    fn selected(&self, view: View) -> usize {
        match view {
            View::Queue => self.queue_selected,
            View::Directories => self.dirs.selected,
            View::Artists => self.artists.selected,
            View::Search => self.search.selected,
        }
    }

    /// The real index of `view`'s selection.
    fn real(&self, view: View) -> Option<usize> {
        self.filters[view.index()].real(self.selected(view), self.total(view))
    }

    fn chosen(&self, view: View) -> Option<&Entry> {
        self.real(view).and_then(|i| self.cols(view).items.get(i))
    }

    fn cursor_of(&mut self, view: View) -> (&mut usize, usize) {
        let len = self.visible(view);
        let sel = match view {
            View::Queue => &mut self.queue_selected,
            View::Directories => &mut self.dirs.selected,
            View::Artists => &mut self.artists.selected,
            View::Search => &mut self.search.selected,
        };
        (sel, len)
    }

    fn cursor(&mut self) -> (&mut usize, usize) {
        self.cursor_of(self.view)
    }

    /// Re-ranks `view` against its current list.
    fn rank(&mut self, view: View) {
        let f = &mut self.filters[view.index()];
        if f.query.is_empty() {
            f.hits = None;
            return;
        }
        let (q, r) = (f.query.as_str(), &mut self.ranker);
        f.hits = Some(match view {
            View::Queue => r.rank(q, self.queue.iter().map(describe)),
            View::Directories => r.rank(q, self.dirs.items.iter().map(name)),
            View::Artists => r.rank(q, self.artists.items.iter().map(name)),
            View::Search => r.rank(q, self.library.tracks.iter().map(describe)),
        });
    }

    /// Re-ranks `view` and puts its selection on the real entry `found`,
    /// else clamps it.
    fn place(&mut self, view: View, found: Option<usize>) {
        self.rank(view);
        let to = found.and_then(|r| self.filters[view.index()].position(r));
        let (sel, len) = self.cursor_of(view);
        *sel = clamp(to.unwrap_or(*sel), len);
    }

    /// Re-applies `view`'s filter; `reset` (a keystroke) selects the best
    /// match. A preview follows a selection that changed entry.
    fn refilter(&mut self, view: View, reset: bool) -> Vec<Out> {
        let before = self.real(view);
        if reset {
            self.rank(view);
            *self.cursor_of(view).0 = 0;
        } else {
            self.place(view, None);
        }
        if self.real(view) == before {
            vec![]
        } else {
            self.refresh(view)
        }
    }

    /// Clears the current view's filter, keeping the selected entry.
    fn clear_filter(&mut self) -> Vec<Out> {
        let view = self.view;
        let real = self.real(view);
        self.filters[view.index()] = Filter::default();
        let (sel, len) = self.cursor();
        match real {
            Some(r) => {
                *sel = r;
                vec![]
            }
            None => {
                *sel = clamp(*sel, len);
                self.refresh(view)
            }
        }
    }

    fn query(&mut self, edit: impl FnOnce(&mut String)) -> Vec<Out> {
        edit(&mut self.filters[self.view.index()].query);
        self.refilter(self.view, true)
    }

    fn refresh(&mut self, view: View) -> Vec<Out> {
        match view {
            View::Directories => self.preview(View::Directories),
            View::Artists => {
                self.rebuild_preview();
                vec![]
            }
            _ => vec![],
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
        let editing = self.filters[self.view.index()].editing;
        let action = keys::action(self.view, key, editing);
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
                self.filters[self.view.index()].editing = true;
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
            Type(c) => self.query(|q| q.push(c)),
            Backspace => self.query(|q| {
                q.pop();
            }),
            Submit => {
                self.filters[self.view.index()].editing = false;
                vec![]
            }
            Cancel | ClearFilter => self.clear_filter(),
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
        self.refresh(self.view)
    }

    fn on_queue(&self, cmd: impl FnOnce(usize) -> Command) -> Vec<Out> {
        self.real(View::Queue)
            .map_or_else(Vec::new, |i| send(cmd(i)))
    }

    fn shift(&mut self, down: bool) -> Vec<Out> {
        if self.filters[View::Queue.index()].hits.is_some() {
            self.footer = Some("clear the filter (Esc) to move entries".into());
            return vec![];
        }
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
            View::Search => {
                match self
                    .real(View::Search)
                    .and_then(|i| self.library.tracks.get(i))
                {
                    Some(t) => t.path.clone(),
                    None => return vec![],
                }
            }
            view => match (self.chosen(view), view, self.cols(view).path.as_slice()) {
                (Some(Entry::Track(t)), ..) => t.path.clone(),
                (Some(Entry::Dir { path, .. }), View::Directories, _) => path.clone(),
                (Some(Entry::Dir { name, .. }), _, []) => {
                    let name = name.clone();
                    return self.take_found(Some(&name), None, op);
                }
                (Some(Entry::Dir { name, .. }), _, [artist, ..]) => {
                    let (artist, name) = (artist.clone(), name.clone());
                    return self.take_found(Some(&artist), Some(&name), op);
                }
                (None, ..) => return vec![],
            },
        };
        vec![done(op.command(vec![path]))]
    }

    fn take_found(&self, artist: Option<&str>, album: Option<&str>, op: Op) -> Vec<Out> {
        let lib = &self.library;
        let paths: Vec<String> = lib
            .catalog
            .find(&lib.tracks, artist, album)
            .into_iter()
            .map(|i| lib.tracks[i].path.clone())
            .collect();
        if paths.is_empty() {
            vec![]
        } else {
            send(op.command(paths))
        }
    }

    /// Entering a folder clears the view's filter, as a new list starts.
    fn right(&mut self) -> Vec<Out> {
        let view = self.view;
        match self.chosen(view) {
            Some(Entry::Dir { name, .. }) => {
                let name = name.clone();
                self.filters[view.index()] = Filter::default();
                let cols = self.columns(view);
                cols.path.push(name);
                cols.parent = std::mem::replace(&mut cols.items, std::mem::take(&mut cols.preview));
                cols.selected = 0;
                if view == View::Artists {
                    self.rebuild_artists(None);
                    return vec![];
                }
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
        let Some(name) = self.columns(view).path.pop() else {
            return vec![];
        };
        self.filters[view.index()] = Filter::default();
        let cols = self.columns(view);
        cols.preview = std::mem::take(&mut cols.items);
        let loaded = !cols.parent.is_empty();
        cols.items = std::mem::take(&mut cols.parent);
        cols.selected = cols
            .items
            .iter()
            .position(|e| matches!(e, Entry::Dir { name: n, .. } if *n == name))
            .unwrap_or(0);
        if view == View::Artists {
            self.rebuild_artists(Some(name));
            return vec![];
        }
        let mut outs = Vec::new();
        if !loaded {
            outs.push(list(view, cols.path.clone(), Col::Current, Some(name)));
        }
        outs.extend(parent_of(view, &cols.path));
        outs
    }

    /// Fetches the preview of the selected folder, artist or album.
    fn preview(&mut self, view: View) -> Vec<Out> {
        self.columns(view).preview.clear();
        match self.chosen(view) {
            Some(Entry::Dir { name, .. }) => {
                let mut path = self.cols(view).path.clone();
                path.push(name.clone());
                vec![list(view, path, Col::Preview, None)]
            }
            _ => vec![],
        }
    }

    /// The Artists column at `path`, from the library copy.
    pub fn artist_column(&self, path: &[String]) -> Vec<Entry> {
        let lib = &self.library;
        let dirs = |names: Vec<String>| {
            names
                .into_iter()
                .map(|n| Entry::Dir {
                    path: n.clone(),
                    name: n,
                })
                .collect()
        };
        match path {
            [] => dirs(lib.catalog.artists()),
            [artist] => dirs(lib.catalog.albums(Some(artist))),
            [artist, album, ..] => lib
                .catalog
                .find(&lib.tracks, Some(artist), Some(album))
                .into_iter()
                .map(|i| Entry::Track(lib.tracks[i].clone()))
                .collect(),
        }
    }

    /// Refills the parent, current and preview columns of Artists at
    /// `artists.path`; `select` names the entry to land on.
    fn rebuild_artists(&mut self, select: Option<String>) {
        let path = self.artists.path.clone();
        let items = self.artist_column(&path);
        let parent = match path.split_last() {
            Some((_, up)) => self.artist_column(up),
            None => Vec::new(),
        };
        let found = select.and_then(|s| {
            items
                .iter()
                .position(|e| matches!(e, Entry::Dir { name, .. } if *name == s))
        });
        self.artists.items = items;
        self.artists.parent = parent;
        self.place(View::Artists, found);
        self.rebuild_preview();
    }

    fn rebuild_preview(&mut self) {
        let preview = match self.chosen(View::Artists) {
            Some(Entry::Dir { name, .. }) => {
                let mut path = self.artists.path.clone();
                path.push(name.clone());
                self.artist_column(&path)
            }
            _ => Vec::new(),
        };
        self.artists.preview = preview;
    }

    fn refetch(&mut self) -> Vec<Out> {
        let mut outs = Vec::new();
        let path = self.dirs.path.clone();
        outs.extend(parent_of(View::Directories, &path));
        outs.push(list(View::Directories, path, Col::Current, None));
        outs
    }

    fn library_delta(&mut self, delta: Delta) -> Vec<Out> {
        match delta {
            Delta::Applied => {
                self.rebuild_artists(None);
                let mut outs = self.refilter(View::Search, false);
                outs.extend(self.refetch());
                outs
            }
            Delta::Refetch => vec![Out::Send(Command::Tracks, Pending::Tracks)],
            Delta::Stale => vec![],
        }
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
            (Pending::Subscribe, _) => {
                let mut outs = self.refetch();
                outs.push(Out::Send(Command::Tracks, Pending::Tracks));
                outs
            }
            (Pending::Status, Body::Status(s)) => {
                self.clock = Clock::from_event(s.state, s.position_ms, s.duration_ms, now);
                self.status = Some(*s);
                vec![]
            }
            (Pending::Queue, Body::Queue(q)) => {
                self.queue = q;
                let playing = if std::mem::take(&mut self.first_queue) {
                    self.status.as_ref().and_then(|s| s.index)
                } else {
                    None
                };
                self.place(View::Queue, playing);
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
            (Pending::Tracks, Body::Library { generation, tracks }) => {
                let delta = self.library.replace(generation, tracks);
                self.library_delta(delta)
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
        let current = match self.chosen(view) {
            Some(Entry::Dir { name, .. }) => {
                path.split_last() == Some((name, self.cols(view).path.as_slice()))
            }
            _ => false,
        };
        let cols = self.columns(view);
        match column {
            Col::Current if cols.path == path => {
                let found = select.and_then(|s| {
                    entries
                        .iter()
                        .position(|e| matches!(e, Entry::Dir { name, .. } if *name == s))
                });
                cols.items = entries;
                self.place(view, found);
                self.preview(view)
            }
            Col::Preview => {
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
            Event::LibraryChanged {
                generation,
                added,
                removed,
                ..
            } => {
                let delta = self.library.apply(generation, added, removed);
                self.library_delta(delta)
            }
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

fn scroll(offset: &mut usize, selected: usize, len: usize, rows: usize) {
    if selected < *offset {
        *offset = selected;
    } else if selected >= *offset + rows {
        *offset = selected + 1 - rows;
    }
    *offset = (*offset).min(len.saturating_sub(rows));
}

/// A column entry as matched: the name without the trailing `/`.
fn name(e: &Entry) -> Cow<'_, str> {
    match e {
        Entry::Dir { name, .. } => Cow::Borrowed(name),
        Entry::Track(t) => Cow::Owned(describe(t)),
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

fn parent_of(view: View, path: &[String]) -> Option<Out> {
    let (_, up) = path.split_last()?;
    Some(list(view, up.to_vec(), Col::Parent, None))
}

/// The request that lists a Directories column at `path`.
fn list(view: View, path: Vec<String>, column: Col, select: Option<String>) -> Out {
    Out::Send(
        Command::Browse {
            path: path.join("/"),
        },
        Pending::List {
            view,
            path,
            column,
            select,
        },
    )
}

pub fn update(app: &mut App, msg: Msg, now: Instant) -> Vec<Out> {
    let outs = match msg {
        Msg::Server(g, _) | Msg::Disconnected(g) if g != app.generation => vec![],
        Msg::Server(_, ServerMessage::Reply { id, result }) => app.reply(id, result, now),
        Msg::Server(_, ServerMessage::Event { event }) => app.event(event, now),
        Msg::Disconnected(_) => {
            app.connected = false;
            app.pending.clear();
            app.stop_clock(now);
            app.footer = Some(DISCONNECTED.into());
            vec![]
        }
        Msg::Key(key) => app.key(key, now),
        Msg::Resize | Msg::Tick => vec![],
    };
    app.follow();
    outs
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
        app.library.replace(
            1,
            vec![
                track("hit.flac"),
                tagged("m/a1.flac", "Ann", "Red", 1),
                tagged("m/a2.flac", "Ann", "Red", 2),
                tagged("m/a3.flac", "Ann", "Blue", 1),
                tagged("m/b1.flac", "Bob", "Gold", 1),
            ],
        );
        app.rebuild_artists(None);
        app
    }

    fn tagged(path: &str, artist: &str, album: &str, no: u32) -> Track {
        Track {
            artist: Some(artist.into()),
            album: Some(album.into()),
            track_number: Some(no),
            ..track(path)
        }
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
                vec![done(Replace {
                    paths: paths(&["m/a3.flac", "m/a1.flac", "m/a2.flac"]),
                })],
            ),
            (
                View::Artists,
                ch('a'),
                vec![done(Add {
                    paths: paths(&["m/a3.flac", "m/a1.flac", "m/a2.flac"]),
                    at: None,
                })],
            ),
            (
                View::Artists,
                ch('A'),
                vec![done(Add {
                    paths: paths(&["m/a3.flac", "m/a1.flac", "m/a2.flac"]),
                    at: Some(2),
                })],
            ),
            (View::Artists, ch('l'), vec![]),
            (View::Artists, ch('j'), vec![]),
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

    fn entries(names: &[&str]) -> Vec<Entry> {
        names.iter().map(|n| named(n)).collect()
    }

    fn tracks_of(paths: &[&str]) -> Vec<Entry> {
        paths.iter().map(|p| Entry::Track(tagged_of(p))).collect()
    }

    fn tagged_of(path: &str) -> Track {
        let app = app();
        app.library
            .tracks
            .iter()
            .find(|t| t.path == path)
            .unwrap()
            .clone()
    }

    #[test]
    fn artists_come_from_the_copy() {
        let mut app = app();
        app.view = View::Artists;
        assert_eq!(app.artists.items, entries(&["Ann", "Bob", "Unknown"]));
        assert_eq!(app.artists.preview, entries(&["Blue", "Red"]));
        assert_eq!(run(&mut app, ch('j')), []);
        assert_eq!(app.artists.preview, entries(&["Gold"]));
        assert_eq!(run(&mut app, ch('k')), []);
        assert_eq!(run(&mut app, ch('l')), []);
        assert_eq!(app.artists.path, paths(&["Ann"]));
        assert_eq!(app.artists.parent, entries(&["Ann", "Bob", "Unknown"]));
        assert_eq!(app.artists.items, entries(&["Blue", "Red"]));
        assert_eq!(app.artists.preview, tracks_of(&["m/a3.flac"]),);
        assert_eq!(run(&mut app, ch('j')), []);
        assert_eq!(app.artists.preview, tracks_of(&["m/a1.flac", "m/a2.flac"]),);
        assert_eq!(run(&mut app, ch('l')), []);
        assert_eq!(app.artists.path, paths(&["Ann", "Red"]));
        assert_eq!(app.artists.parent, entries(&["Blue", "Red"]));
        assert_eq!(app.artists.items, tracks_of(&["m/a1.flac", "m/a2.flac"]));
        assert!(app.artists.preview.is_empty());
        assert_eq!(run(&mut app, ch('h')), []);
        assert_eq!(app.artists.path, paths(&["Ann"]));
        assert_eq!(app.artists.selected, 1);
        assert_eq!(app.artists.items, entries(&["Blue", "Red"]));
        assert_eq!(run(&mut app, ch('h')), []);
        assert_eq!(app.artists.selected, 0);
        assert!(app.artists.parent.is_empty());
        assert_eq!(app.artists.items, entries(&["Ann", "Bob", "Unknown"]));
    }

    #[test]
    fn artist_columns_match_the_catalog() {
        let app = app();
        let c = &app.library.catalog;
        assert_eq!(app.artist_column(&[]), entries_of(&c.artists()));
        assert_eq!(
            app.artist_column(&paths(&["Ann"])),
            entries_of(&c.albums(Some("Ann")))
        );
        let want: Vec<Entry> = c
            .find(&app.library.tracks, Some("Ann"), Some("Red"))
            .into_iter()
            .map(|i| Entry::Track(app.library.tracks[i].clone()))
            .collect();
        assert_eq!(app.artist_column(&paths(&["Ann", "Red"])), want);
    }

    fn entries_of(names: &[String]) -> Vec<Entry> {
        names.iter().map(|n| named(n)).collect()
    }

    #[test]
    fn artist_add_sends_paths() {
        let mut app = app();
        app.view = View::Artists;
        run(&mut app, ch('l'));
        run(&mut app, ch('j'));
        assert_eq!(
            run(&mut app, ch('a')),
            [done(Command::Add {
                paths: paths(&["m/a1.flac", "m/a2.flac"]),
                at: None
            })]
        );
        run(&mut app, ch('l'));
        let add = done(Command::Add {
            paths: paths(&["m/a1.flac"]),
            at: None,
        });
        assert_eq!(run(&mut app, ch('l')), std::slice::from_ref(&add));
        assert_eq!(run(&mut app, ch('a')), [add]);
    }

    #[test]
    fn artist_columns_follow_the_copy() {
        let mut app = app();
        app.view = View::Artists;
        run(&mut app, ch('l'));
        run(&mut app, ch('j'));
        assert_eq!(app.artists.path, paths(&["Ann"]));
        let gone = Event::LibraryChanged {
            generation: 2,
            tracks: 0,
            added: vec![tagged("m/a4.flac", "Ann", "Zed", 1)],
            removed: paths(&["m/a1.flac", "m/a2.flac"]),
        };
        let outs = run(&mut app, event(1, gone));
        assert!(outs.iter().all(|o| matches!(
            o,
            Out::Send(
                Command::Browse { .. },
                Pending::List {
                    view: View::Directories,
                    ..
                }
            )
        )));
        assert_eq!(app.artists.items, entries(&["Blue", "Zed"]));
        assert_eq!(app.artists.selected, 1);
        assert_eq!(app.artists.preview, tracks_new(&["m/a4.flac"], &app));
        let gone = Event::LibraryChanged {
            generation: 3,
            tracks: 0,
            added: vec![],
            removed: paths(&["m/a3.flac", "m/a4.flac"]),
        };
        run(&mut app, event(1, gone));
        assert!(app.artists.items.is_empty());
        assert_eq!(app.artists.path, paths(&["Ann"]));
        assert_eq!(app.artists.selected, 0);
    }

    fn tracks_new(p: &[&str], app: &App) -> Vec<Entry> {
        p.iter()
            .map(|p| {
                Entry::Track(
                    app.library
                        .tracks
                        .iter()
                        .find(|t| t.path == *p)
                        .unwrap()
                        .clone(),
                )
            })
            .collect()
    }

    #[test]
    fn search_tab_lists_the_library() {
        let mut app = app();
        app.view = View::Search;
        app.page = 3;
        assert_eq!(run(&mut app, ch('G')), []);
        assert_eq!(app.search.selected, 4);
        app.follow();
        assert_eq!(app.search.offset, 2);
        assert_eq!(
            run(&mut app, press(KeyCode::Enter)),
            [done(Command::Replace {
                paths: paths(&["m/b1.flac"])
            })]
        );
        run(&mut app, ch('/'));
        assert_eq!(run(&mut app, ch('x')), []);
        assert_eq!(run(&mut app, press(KeyCode::Enter)), []);
        assert!(!app.filters[View::Search.index()].editing);
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
        assert!(app.filters.iter().all(|f| !f.editing));
    }

    #[test]
    fn the_query_is_edited_locally() {
        let mut app = app();
        app.view = View::Search;
        assert_eq!(run(&mut app, ch('/')), []);
        assert_eq!(app.view, View::Search);
        let f = |app: &App| app.filters[View::Search.index()].clone();
        assert!(f(&app).editing);
        for c in "iriq".chars() {
            assert_eq!(run(&mut app, ch(c)), []);
        }
        assert_eq!(run(&mut app, press(KeyCode::Backspace)), []);
        assert_eq!(run(&mut app, ch('s')), []);
        assert_eq!(f(&app).query, "iris");
        assert_eq!(run(&mut app, press(KeyCode::Enter)), []);
        assert!(!f(&app).editing);
        assert_eq!(f(&app).query, "iris");
        run(&mut app, ch('/'));
        assert_eq!(run(&mut app, ch('o')), []);
        assert_eq!(run(&mut app, press(KeyCode::Esc)), []);
        assert_eq!(f(&app), Filter::default());
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
        app.artists.path = paths(&["Ann", "Red"]);
        app.rebuild_artists(None);
        assert_eq!(app.artists.parent, entries(&["Blue", "Red"]));
        assert_eq!(run(&mut app, ch('h')), []);
        assert_eq!(app.artists.parent, entries(&["Ann", "Bob", "Unknown"]));
        assert_eq!(run(&mut app, ch('h')), []);
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
        let outs = run(
            &mut app,
            event(
                1,
                Event::LibraryChanged {
                    generation: 2,
                    tracks: 0,
                    added: vec![],
                    removed: vec![],
                },
            ),
        );
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
    fn a_lost_daemon_stops_the_clock() {
        use std::time::Duration;

        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = app();
        let now = Instant::now();
        app.status.as_mut().unwrap().state = PlayState::Playing;
        app.clock = Clock::from_event(PlayState::Playing, 10_000, Some(60_000), now);
        assert!(app.clock.next_wake(now).is_some());
        update(&mut app, Msg::Disconnected(1), now);
        assert_eq!(app.clock.next_wake(now), None);
        assert_eq!(app.clock.position(now + Duration::from_secs(5)), 10_000);
        assert_eq!(app.queue.len(), 3);
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| crate::ui::draw(&app, f)).unwrap();
        let top: String = (0..80)
            .map(|x| t.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(!top.contains("playing"), "{top:?}");
    }

    fn long_queue(len: usize) -> App {
        let mut app = app();
        app.queue = (0..len).map(|i| track(&format!("q{i}"))).collect();
        app.queue_selected = 0;
        app.page = 10;
        app
    }

    fn press_n(app: &mut App, c: char, n: usize) {
        for _ in 0..n {
            run(app, ch(c));
        }
    }

    #[test]
    fn moving_up_inside_the_window_keeps_the_offset() {
        let mut app = long_queue(40);
        press_n(&mut app, 'j', 25);
        assert_eq!((app.queue_selected, app.queue_offset), (25, 16));
        press_n(&mut app, 'k', 3);
        assert_eq!((app.queue_selected, app.queue_offset), (22, 16));
    }

    #[test]
    fn moving_past_the_bottom_scrolls_by_one() {
        let mut app = long_queue(40);
        press_n(&mut app, 'j', 9);
        assert_eq!((app.queue_selected, app.queue_offset), (9, 0));
        press_n(&mut app, 'j', 1);
        assert_eq!((app.queue_selected, app.queue_offset), (10, 1));
    }

    #[test]
    fn a_shrinking_list_clamps_the_offset() {
        let mut app = long_queue(40);
        app.queue_selected = 39;
        app.follow();
        assert_eq!(app.queue_offset, 30);
        app.generation = 1;
        app.expect(5, Pending::Queue);
        let q = (0..12).map(|i| track(&format!("n{i}"))).collect();
        run(&mut app, reply(1, 5, Outcome::Ok(Body::Queue(q))));
        assert_eq!((app.queue_selected, app.queue_offset), (11, 2));
        app.page = 20;
        app.follow();
        assert_eq!(app.queue_offset, 0);
    }

    #[test]
    fn the_first_queue_reply_selects_the_playing_entry() {
        let mut app = long_queue(0);
        app.connected(
            2,
            &Resync {
                subscribe: 10,
                status: 11,
                queue: 12,
            },
        );
        run(
            &mut app,
            reply(
                2,
                11,
                Outcome::Ok(Body::Status(Box::new(status(Some(30), None)))),
            ),
        );
        let q = (0..40).map(|i| track(&format!("q{i}"))).collect();
        run(&mut app, reply(2, 12, Outcome::Ok(Body::Queue(q))));
        assert_eq!(app.queue_selected, 30);
        assert!(app.queue_offset <= 30 && 30 < app.queue_offset + app.page);
        app.queue_selected = 5;
        app.expect(13, Pending::Queue);
        run(
            &mut app,
            reply(2, 13, Outcome::Ok(Body::Queue(vec![track("a"); 40]))),
        );
        assert_eq!(app.queue_selected, 5);
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
                Out::Send(Command::Tracks, Pending::Tracks),
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
    fn connect_fetches_the_library() {
        let mut app = app();
        let resync = Resync {
            subscribe: 10,
            status: 11,
            queue: 12,
        };
        app.connected(2, &resync);
        let outs = run(&mut app, reply(2, 10, Outcome::Ok(Body::Done)));
        assert!(
            outs.contains(&Out::Send(Command::Tracks, Pending::Tracks)),
            "{outs:?}"
        );
        app.expect(13, Pending::Tracks);
        let library = Body::Library {
            generation: 4,
            tracks: vec![track("/m/a.flac")],
        };
        let outs = run(&mut app, reply(2, 13, Outcome::Ok(library)));
        assert!(
            outs.contains(&Out::Send(
                Command::Browse { path: "".into() },
                list(View::Directories, &[], Col::Current, None)
            )),
            "{outs:?}"
        );
        assert_eq!(app.library.tracks, [track("/m/a.flac")]);
        assert_eq!(app.artists.items, entries(&["Unknown"]));
    }

    #[test]
    fn library_events_follow_the_generation() {
        let mut app = app();
        app.library.replace(4, vec![track("/m/a.flac")]);
        let changed = |generation, added: Vec<Track>| Event::LibraryChanged {
            generation,
            tracks: 0,
            added,
            removed: vec![],
        };
        assert_eq!(run(&mut app, event(1, changed(4, vec![]))), []);
        let outs = run(&mut app, event(1, changed(5, vec![track("/m/b.flac")])));
        assert!(!outs.is_empty());
        assert_eq!(app.library.tracks, [track("/m/a.flac"), track("/m/b.flac")]);
        assert_eq!(
            run(&mut app, event(1, changed(7, vec![]))),
            [Out::Send(Command::Tracks, Pending::Tracks)]
        );
        app.expect(3, Pending::Tracks);
        let old = Body::Library {
            generation: 3,
            tracks: vec![],
        };
        assert_eq!(run(&mut app, reply(1, 3, Outcome::Ok(old))), []);
        assert_eq!(app.library.tracks.len(), 2);
    }

    #[test]
    fn switch_clears_the_library() {
        let mut app = app();
        app.library.replace(4, vec![track("/m/a.flac")]);
        assert_eq!(run(&mut app, ch('o')), [Out::Switch]);
        let resync = Resync {
            subscribe: 10,
            status: 11,
            queue: 12,
        };
        app.connected(2, &resync);
        assert!(app.library.tracks.is_empty());
        assert_eq!(app.library.replace(1, vec![]), crate::local::Delta::Applied);
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

    fn titled(path: &str, artist: &str, title: &str) -> Track {
        Track {
            artist: Some(artist.into()),
            title: Some(title.into()),
            ..track(path)
        }
    }

    fn filtered_queue() -> App {
        let mut app = app();
        app.queue = vec![
            titled("q0", "ABBA", "Fernando"),
            titled("q1", "ABBA", "The Winner Takes It All"),
            titled("q2", "Other", "Waltz Time"),
        ];
        app.queue_selected = 0;
        app
    }

    fn typed(app: &mut App, text: &str) -> Vec<Out> {
        let mut outs = run(app, ch('/'));
        for c in text.chars() {
            outs.extend(run(app, ch(c)));
        }
        outs
    }

    fn queue_lines(app: &App) -> Vec<String> {
        app.queue.iter().map(lossless_proto::describe).collect()
    }

    #[test]
    fn typing_filters_without_enter() {
        let mut app = filtered_queue();
        app.queue_selected = 2;
        assert_eq!(typed(&mut app, "wt"), []);
        let want = Ranker::new().rank("wt", queue_lines(&app));
        assert_eq!(want.len(), 2);
        let f = &app.filters[View::Queue.index()];
        assert!(f.editing);
        assert_eq!(f.hits.as_ref(), Some(&want));
        assert_eq!(app.queue_selected, 0);
        assert_eq!(app.view, View::Queue);
        assert_eq!(run(&mut app, press(KeyCode::Down)), []);
        assert_eq!(app.queue_selected, 1);
        assert_eq!(run(&mut app, ctrl('p')), []);
        assert_eq!(app.queue_selected, 0);
        assert_eq!(run(&mut app, press(KeyCode::Backspace)), []);
        assert_eq!(
            app.filters[0].hits,
            Some(Ranker::new().rank("w", queue_lines(&app)))
        );
        run(&mut app, press(KeyCode::Backspace));
        assert_eq!(app.filters[0].hits, None);
    }

    #[test]
    fn enter_plays_the_real_index() {
        let mut app = filtered_queue();
        typed(&mut app, "waltz");
        assert_eq!(app.filters[0].hits, Some(vec![2]));
        assert_eq!(run(&mut app, press(KeyCode::Enter)), []);
        assert!(!app.filters[0].editing);
        assert_eq!(
            run(&mut app, press(KeyCode::Enter)),
            [done(Command::Play { index: Some(2) })]
        );
    }

    #[test]
    fn remove_uses_the_real_index() {
        let mut app = filtered_queue();
        typed(&mut app, "waltz");
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(
            run(&mut app, ch('d')),
            [done(Command::Remove { indices: vec![2] })]
        );
        typed(&mut app, "zzz");
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(run(&mut app, ch('d')), []);
        assert_eq!(run(&mut app, press(KeyCode::Enter)), []);
    }

    #[test]
    fn move_is_refused_while_filtered() {
        let mut app = filtered_queue();
        typed(&mut app, "abba");
        run(&mut app, press(KeyCode::Enter));
        for k in ['J', 'K'] {
            assert_eq!(run(&mut app, ch(k)), []);
            assert_eq!(
                app.footer.as_deref(),
                Some("clear the filter (Esc) to move entries")
            );
        }
    }

    #[test]
    fn escape_clears_and_keeps_the_entry() {
        let mut app = filtered_queue();
        typed(&mut app, "waltz");
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(run(&mut app, press(KeyCode::Esc)), []);
        assert_eq!(app.filters[0], Filter::default());
        assert_eq!(app.queue_selected, 2);
        typed(&mut app, "winner");
        assert_eq!(run(&mut app, press(KeyCode::Esc)), []);
        assert_eq!(app.filters[0], Filter::default());
        assert_eq!(app.queue_selected, 1);
        typed(&mut app, "zzz");
        run(&mut app, press(KeyCode::Esc));
        assert_eq!(app.queue_selected, 0);
    }

    #[test]
    fn queue_filter_survives_a_shrinking_queue() {
        let mut app = filtered_queue();
        typed(&mut app, "a");
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.filters[0].hits.as_ref().map(Vec::len), Some(3));
        run(&mut app, ch('G'));
        assert_eq!(app.queue_selected, 2);
        app.expect(5, Pending::Queue);
        let q = vec![titled("n0", "Ann", "Away"), titled("n1", "Ann", "Bye")];
        run(&mut app, reply(1, 5, Outcome::Ok(Body::Queue(q))));
        let want = Ranker::new().rank("a", queue_lines(&app));
        assert_eq!(app.filters[0].hits.as_ref(), Some(&want));
        assert_eq!(app.queue_selected, want.len() - 1);
        let [Out::Send(Command::Play { index: Some(i) }, _)] =
            run(&mut app, press(KeyCode::Enter))[..]
        else {
            panic!("one play")
        };
        assert!(i < app.queue.len());
    }

    #[test]
    fn directories_preview_follows_the_top_match() {
        let mut app = app();
        app.view = View::Directories;
        assert_eq!(run(&mut app, ch('/')), []);
        assert_eq!(
            run(&mut app, ch('b')),
            [Out::Send(
                Command::Browse { path: "b".into() },
                list(View::Directories, &["b"], Col::Preview, None)
            )]
        );
        assert_eq!(app.dirs.selected, 0);
        assert_eq!(
            run(&mut app, press(KeyCode::Backspace)),
            [Out::Send(
                Command::Browse { path: "a".into() },
                list(View::Directories, &["a"], Col::Preview, None)
            )]
        );
        app.expect(3, list(View::Directories, &["a"], Col::Preview, None));
        app.expect(4, list(View::Directories, &["b"], Col::Preview, None));
        run(&mut app, ch('b'));
        let stale = vec![Entry::Track(track("a/t.flac"))];
        run(&mut app, reply(1, 3, Outcome::Ok(Body::Entries(stale))));
        assert!(app.dirs.preview.is_empty());
        let fresh = vec![Entry::Track(track("b/t.flac"))];
        run(
            &mut app,
            reply(1, 4, Outcome::Ok(Body::Entries(fresh.clone()))),
        );
        assert_eq!(app.dirs.preview, fresh);
    }

    #[test]
    fn a_listing_reapplies_the_directories_filter() {
        let mut app = app();
        app.view = View::Directories;
        typed(&mut app, "b");
        run(&mut app, press(KeyCode::Enter));
        app.expect(6, list(View::Directories, &[], Col::Current, None));
        let top = vec![named("a"), named("bb"), named("c"), named("b")];
        let outs = run(&mut app, reply(1, 6, Outcome::Ok(Body::Entries(top))));
        let want = Ranker::new().rank("b", ["a", "bb", "c", "b"]);
        assert_eq!(app.filters[1].hits.as_ref(), Some(&want));
        let top = match &app.dirs.items[want[0]] {
            Entry::Dir { name, .. } => name.clone(),
            _ => unreachable!(),
        };
        assert_eq!(
            outs,
            [Out::Send(
                Command::Browse { path: top.clone() },
                list(View::Directories, &[top.as_str()], Col::Preview, None)
            )]
        );
    }

    #[test]
    fn entering_a_column_clears_its_filter() {
        let mut app = app();
        app.view = View::Directories;
        typed(&mut app, "b");
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(
            run(&mut app, ch('l')),
            [Out::Send(
                Command::Browse { path: "b".into() },
                list(View::Directories, &["b"], Col::Current, None)
            )]
        );
        assert_eq!(app.dirs.path, paths(&["b"]));
        assert_eq!(app.filters[1], Filter::default());
        typed(&mut app, "zz");
        run(&mut app, press(KeyCode::Enter));
        run(&mut app, ch('h'));
        assert_eq!(app.filters[1], Filter::default());
        assert!(app.dirs.path.is_empty());
        app.view = View::Artists;
        typed(&mut app, "bob");
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.artists.preview, entries(&["Gold"]));
        assert_eq!(run(&mut app, ch('l')), []);
        assert_eq!(app.artists.path, paths(&["Bob"]));
        assert_eq!(app.artists.items, entries(&["Gold"]));
        assert_eq!(app.filters[2], Filter::default());
        typed(&mut app, "zz");
        run(&mut app, press(KeyCode::Enter));
        run(&mut app, ch('h'));
        assert_eq!(app.filters[2], Filter::default());
        assert_eq!(app.artists.selected, 1);
    }

    #[test]
    fn artists_filter_follows_the_copy() {
        let mut app = app();
        app.view = View::Artists;
        typed(&mut app, "bo");
        assert_eq!(app.artists.preview, entries(&["Gold"]));
        let added = Event::LibraryChanged {
            generation: 2,
            tracks: 0,
            added: vec![tagged("m/c1.flac", "Bono", "Solo", 1)],
            removed: vec![],
        };
        run(&mut app, event(1, added));
        let names = ["Ann", "Bob", "Bono", "Unknown"];
        assert_eq!(app.artists.items, entries(&names));
        let want = Ranker::new().rank("bo", names);
        assert_eq!(app.filters[2].hits.as_ref(), Some(&want));
        run(&mut app, press(KeyCode::Enter));
        let top = names[want[0]];
        let lib = &app.library;
        let want_paths: Vec<String> = lib
            .catalog
            .find(&lib.tracks, Some(top), None)
            .into_iter()
            .map(|i| lib.tracks[i].path.clone())
            .collect();
        assert_eq!(
            run(&mut app, ch('a')),
            [done(Command::Add {
                paths: want_paths,
                at: None
            })]
        );
    }

    #[test]
    fn delta_reapplies_the_search_filter() {
        let mut app = app();
        app.view = View::Search;
        typed(&mut app, "zed");
        run(&mut app, press(KeyCode::Enter));
        assert_eq!(app.filters[3].hits, Some(vec![]));
        assert_eq!(run(&mut app, press(KeyCode::Enter)), []);
        let added = Event::LibraryChanged {
            generation: 2,
            tracks: 0,
            added: vec![titled("m/z.flac", "Zed", "Zz")],
            removed: vec![],
        };
        run(&mut app, event(1, added));
        let at = app
            .library
            .tracks
            .iter()
            .position(|t| t.path == "m/z.flac")
            .unwrap();
        assert_eq!(app.filters[3].hits, Some(vec![at]));
        assert_eq!(
            run(&mut app, press(KeyCode::Enter)),
            [done(Command::Replace {
                paths: paths(&["m/z.flac"])
            })]
        );
    }

    fn footer_of(app: &App) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| crate::ui::draw(app, f)).unwrap();
        (0..80)
            .map(|x| t.backend().buffer()[(x, 23)].symbol().to_string())
            .collect()
    }

    #[test]
    fn footer_shows_query_and_count() {
        let mut app = filtered_queue();
        typed(&mut app, "wt");
        assert!(
            footer_of(&app).starts_with("/ wt_ "),
            "{:?}",
            footer_of(&app)
        );
        run(&mut app, press(KeyCode::Enter));
        assert!(
            footer_of(&app).starts_with("/ wt  2 of 3 "),
            "{:?}",
            footer_of(&app)
        );
        run(&mut app, ch('J'));
        assert!(footer_of(&app).starts_with("clear the filter (Esc) to move entries"));
        app.view = View::Search;
        assert!(!footer_of(&app).contains("/ wt"));
        app.view = View::Queue;
        run(&mut app, press(KeyCode::Esc));
        assert!(!footer_of(&app).contains("/ "));
    }

    #[test]
    fn filters_survive_a_reconnect() {
        let mut app = filtered_queue();
        typed(&mut app, "wt");
        run(&mut app, press(KeyCode::Enter));
        let resync = Resync {
            subscribe: 10,
            status: 11,
            queue: 12,
        };
        app.connected(2, &resync);
        assert_eq!(app.filters[0].hits, Some(vec![]));
        let q = vec![titled("n0", "Ann", "Wet")];
        run(&mut app, reply(2, 12, Outcome::Ok(Body::Queue(q))));
        assert_eq!(app.filters[0].hits, Some(vec![0]));
    }
}
