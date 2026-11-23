pub mod modal;
mod sanitize;
pub mod terminal;

use std::path::Path;

use lossless_proto::Entry;

use crate::app::{App, View};
use crate::config::Mode;

/// A script about to run: what to start and what it gets to know.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// Tells this run's messages from an earlier one's.
    pub id: u64,
    pub name: String,
    pub command: String,
    pub mode: Mode,
    pub env: Vec<(String, String)>,
    /// Absolute paths, one per line of `LOSSLESS_SELECTION`.
    pub selection: Vec<String>,
}

/// The script at `idx` of the Scripts list, set up from the app's state.
pub fn build(app: &App, idx: usize, id: u64) -> Option<Run> {
    let s = app.scripts.items.get(idx)?;
    let mut env = vec![(
        "LOSSLESS_SOCKET".to_string(),
        app.socket.display().to_string(),
    )];
    let mut add = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            env.push((k.into(), v));
        }
    };
    add(
        "LOSSLESS_PROFILE",
        app.switchable
            .then(|| app.profiles.get(app.profile).map(|p| p.name().to_string()))
            .flatten(),
    );
    add("LOSSLESS_MUSIC_DIR", app.music_dir.clone());
    add("LOSSLESS_CURRENT", current(app));
    Some(Run {
        id,
        name: s.name.clone(),
        command: s.command.clone(),
        mode: s.mode,
        env,
        selection: selection(app, app.view),
    })
}

fn current(app: &App) -> Option<String> {
    let status = app.status.as_ref()?;
    let i = status.index?;
    app.queue
        .get(i)
        .or(status.track.as_ref())
        .map(|t| t.path.clone())
}

/// What `view` has selected, as absolute track paths in library order.
pub fn selection(app: &App, view: View) -> Vec<String> {
    let lib = &app.library;
    match view {
        View::Scripts => selection(app, app.last_view),
        View::Queue => app
            .real(View::Queue)
            .and_then(|i| app.queue.get(i))
            .map(|t| t.path.clone())
            .into_iter()
            .collect(),
        View::Search => app
            .real(View::Search)
            .and_then(|i| lib.tracks.get(i))
            .map(|t| t.path.clone())
            .into_iter()
            .collect(),
        View::Directories => match app.chosen(view) {
            Some(Entry::Track(t)) => vec![t.path.clone()],
            Some(Entry::Dir { path, .. }) => match &app.music_dir {
                Some(root) => {
                    let base = Path::new(root).join(path);
                    lib.tracks
                        .iter()
                        .filter(|t| Path::new(&t.path).starts_with(&base))
                        .map(|t| t.path.clone())
                        .collect()
                }
                None => vec![],
            },
            None => vec![],
        },
        View::Artists => match (app.chosen(view), app.artists.path.as_slice()) {
            (Some(Entry::Track(t)), _) => vec![t.path.clone()],
            (Some(Entry::Dir { name, .. }), []) => app.found(Some(name), None),
            (Some(Entry::Dir { name, .. }), [artist, ..]) => app.found(Some(artist), Some(name)),
            (None, _) => vec![],
        },
    }
}

/// `exited N` or `killed by signal N`.
pub fn status_text(st: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(c), _) => format!("exited {c}"),
        (None, Some(s)) => format!("killed by signal {s}"),
        (None, None) => "ended".into(),
    }
}
