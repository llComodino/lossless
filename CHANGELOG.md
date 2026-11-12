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
- Library scan with tags (artist, album, title, track number, duration,
  format) and word search; rescan on demand.
- Library listings: `browse` by folder, `artists`, `albums` and `find`, over
  the socket and in `lossless`.
- Newline-delimited JSON protocol over a unix socket, with request ids and
  events (`state`, `track_changed`, `queue_changed`, `library_changed`) for
  subscribed clients.
- Event-driven position: no ticks, zero wakeups while idle; clients derive
  the position from the last state event.
- TOML config (`device`, `music_dir`, `socket`) with strict validation.
- `lossless`: command line client (`status`, `queue`, `add`, `play`,
  `pause`, `toggle`, `stop`, `next`, `prev`, `seek`, `clear`, `search`,
  `update`, `watch`, `--json`, `--socket`).
- Tests: bit-exact decode and seek against generated FLAC fixtures, player
  state machine against a fake sink, daemon over a real socket, CLI as a
  real binary, property tests, and fuzz targets for requests, config files,
  device names, decoding and tags.

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

### Security

- Request lines are capped at 64 KiB; invalid UTF-8 gets an error reply.
- Tags are parsed only as FLAC or MP3, chosen by extension: content sniffing
  let crafted files hang the AAC parser.
- The socket is created owner-only (0600) and only a stale socket is ever
  replaced, never a regular file or a symlink.
- ALSA device names in the config are restricted to plain hardware names:
  ALSA's `file` plugin would run `file:'|cmd'` through a shell.
- Config paths must be absolute without `.`/`..` or control characters; the
  config must be a regular file of at most 64 KiB.
