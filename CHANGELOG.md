# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/). Until 1.0 the protocol and
config may change between minor versions.

## [Unreleased]

First development version, not yet tagged.

### Added

- `losslessd`: bit-perfect playback of FLAC and MP3 through ALSA at each
  file's own rate and bit depth, in the narrowest lossless sample format
  (S16_LE, S24_3LE, S32_LE).
- Gapless playback: same-format tracks share one open stream.
- USB DAC care: the device stays open across same-format tracks, a settle
  delay precedes reopening at a new rate, transient open errors are retried
  with backoff.
- Queue with play, pause, toggle, stop, next, previous, jump, clear and
  sample-accurate seek.
- Queue editing: `add --at`, `replace`, `remove` and `move`, with the playing
  track keeping its place.
- Repeat (off, queue, one) and random modes: `set_mode` and the `mode` event
  in the protocol, `repeat` and `random` in the client and status. Repeat
  one stays gapless; random plays each entry once per cycle.
- Library scan with tags (artist, album, title, track and disc number,
  duration, format) and word search; rescan on demand. Unreadable entries
  inside the music folder are skipped, not fatal.
- Library listings: `browse` by folder, `artists`, `albums` and `find`, over
  the socket and in `lossless`.
- Newline-delimited JSON protocol over a unix socket, with request ids and
  events (`state`, `track_changed`, `queue_changed`, `library_changed`) for
  subscribed clients.
- Event-driven position: no ticks, zero wakeups while idle; clients derive
  the position from the last state event.
- Volume: the DAC's hardware mixer (changes made outside the daemon
  reported too; a card missing at startup is picked up once it appears) or
  software gain on default/pipewire/pulse, chosen by `mixer` and
  `mixer_control`; `set_volume` and the `volume` event, `volume` in status,
  and `lossless volume [N|+N|-N]`.
- Typed errors: every error reply carries a code (`bad_request`,
  `not_found`, `out_of_range`, `no_mixer`, `no_music_dir`, `scan_running`,
  `device`, `decode`, `io`) and a message; failures the daemon recovers from
  by itself are sent as an `error` event and kept in status as `last_error`.
- Leveled logging to stderr (`log_level` off, warn, info, debug; off by
  default) and `losslessd --log-level`.
- State file (`$XDG_STATE_HOME/lossless/state.json`): the queue, position
  and modes survive a restart, restored paused with the device closed.
- Profiles: `losslessd --profile NAME` runs one daemon per output, each with
  its own config (`NAME.toml`), state and socket; `lossless -p NAME` talks
  to it. Without a profile every path is as before.
- TOML config (`device`, `music_dir`, `socket`, `log_level`, `mixer`,
  `mixer_control`) with strict validation.
- `lossless`: command line client (`status`, `queue`, `add`, `play`,
  `pause`, `toggle`, `stop`, `next`, `prev`, `seek`, `clear`, `search`,
  `update`, `watch`, `--json`, `--socket`).
- `losslessd-tui`: terminal UI with queue, directory, artist/album and search
  views, profile switching and a key help screen. Event-driven: it wakes only
  for input, daemon events and the progress clock while playing.
- Tests: bit-exact decode and seek against generated FLAC fixtures, player
  state machine against a fake sink, daemon over a real socket, CLI as a
  real binary, property tests, and fuzz targets for requests, config files,
  device names, decoding, tags and state files.
- `tracks` command: the whole library with its `generation`.
- `library_changed` carries `generation`, `added` and `removed`;
  `lossless watch` prints `library changed: T tracks (+A -R)`.
- `losslessd-tui`: `/` opens a live fuzzy filter in every view (`Esc`
  clears; moving queue entries is off while filtered); the UI keeps a local
  library copy patched from the deltas.
- `losslessd-tui`: Gruber Darker colors on a `#181818` background.
- `losslessd-tui`: every key is configurable in
  `$XDG_CONFIG_HOME/lossless/tui.ron`; defaults unchanged, `?` shows the
  keys in effect.

### Fixed

- Seek by timestamp is sample-exact (it was off by one frame through
  seconds rounding).
- Events are sent only after the status they describe is published.
- Commands return only once they have taken effect.
- Timestamp conversions saturate instead of overflowing; 0 Hz streams are
  rejected.
- The client rejects non-finite and negative seek times and fits narrow
  widths without underflow.
- The scan flag is cleared even if a scan panics.
- A subscriber that stops reading no longer stalls events for the others:
  each has its own writer, and one about 256 events behind is disconnected
  (it resyncs on reconnect).

### Security

- Request lines are capped at 4 MiB; invalid UTF-8 gets an error reply.
- Tags are parsed only as FLAC or MP3, chosen by extension: content sniffing
  let crafted files hang the AAC parser.
- The socket is created owner-only (0600) and only a stale socket is ever
  replaced, never a regular file or a symlink.
- ALSA device names in the config are restricted to plain hardware names:
  ALSA's `file` plugin would run `file:'|cmd'` through a shell.
- Config paths must be absolute without `.`/`..` or control characters; the
  config must be a regular file of at most 64 KiB.
