# lossless

A music daemon for Linux, written in Rust, that plays every file
**bit-perfect**: the samples in the file are the samples that reach the DAC,
at the file's own sample rate and bit depth. It aims to cover what most
people use MPD for (a library, a queue, gapless playback, clients that
come and go) without ever putting a resampler, a mixer or a format
converter in the signal path.

- `losslessd`: the daemon
- `lossless`: a command line client
- `losslessd-tui`: a terminal UI

> **Status: early.** Playback, the library and the CLI work and are tested,
> and the daemon is in daily use on one machine. The protocol will change
> until 1.0. See [CHANGELOG.md](CHANGELOG.md).

## Why

[MPD](https://www.musicpd.org/) is the reference music daemon and it *can*
play bit-perfect, but only when every part of its output chain is set up
just right: `auto_resample`, `auto_format` and `auto_channels` off, a `hw`
or `front` device, no software mixer, the DAC kept away from PipeWire or
PulseAudio. Any of those set the wrong way quietly converts your audio, and
nothing tells you. It is also a large C++ codebase with network listeners
and plugin decoders, which is a lot of attack surface and memory-unsafe code
to put between your files and your DAC.

Other players that try to modernize this either resample internally (many
"engines" run at a fixed 16-bit or 44.1 kHz), or depend on a desktop sound
server that mixes and resamples by design.

lossless starts from the other end: bit-perfect output is not a setting, it
is the only thing the playback path knows how to do.

- **Bit-perfect by construction.** Files are decoded to integer samples and
  written to ALSA at exactly the source rate, in the narrowest sample format
  that holds every bit (S16_LE, S24_3LE or S32_LE; a file is zero-padded to a
  wider format only when the device lacks its own width). There is no
  resampler or dither in the code. If the device can't take a file's exact
  format, playback of that file fails with an error instead of being
  converted.
- **Gapless.** Consecutive tracks with the same format are written to one
  open stream with nothing in between.
- **Gentle on USB DACs.** Some DACs wedge their firmware when a player closes
  and reopens the stream at a new rate every few seconds. lossless keeps the
  device open across tracks that share a format, waits before reopening at a
  new rate, and retries transient open errors with backoff.
- **Memory-safe and small.** Pure safe Rust in this repository
  (`#![forbid(unsafe_code)]` in every crate); decoding by
  [symphonia](https://github.com/pdeljanov/Symphonia), tags by
  [lofty](https://github.com/Serial-ATA/lofty-rs), output through the `alsa`
  crate. About 11 MB resident with a 900-track library.
- **Event-driven.** No polling, no timers, no periodic ticks. While paused or
  stopped the daemon does not wake up at all; clients get events when
  something changes and compute the playback position themselves.
- **Local by default.** One unix socket, owner-only (mode 0600). No network
  listener.

## Features

| | lossless | notes |
|---|---|---|
| FLAC (16/24-bit, up to 384 kHz) | yes | bit-perfect |
| MP3 | yes | decoded output, 32-bit |
| Gapless playback | yes | same-format tracks share one stream |
| Library scan with tags, search | yes | artist, album, title, path; rescan on demand |
| Queue: add, clear, jump | yes | |
| Play, pause, stop, next, previous, sample-accurate seek | yes | |
| Events for clients | yes | state, track, queue, library |
| Queue: insert at position, remove, move, play now | yes | the playing track keeps its place |
| Repeat (queue / one), random | yes | repeat one stays gapless; next skips past it |
| Browse by folder, artist, album | yes | `browse`, `artists`, `albums`, `find` |
| Volume: DAC hardware mixer, or software gain on the PipeWire path | yes | the bit-perfect path never scales samples; changes from `alsamixer` are reported |
| Queue and position restored after restart | yes | paused where it was; vanished files dropped |
| Terminal UI (`losslessd-tui`) | yes | queue, directories, artists, search; switches profiles |
| MPRIS (media keys, bars) | planned | |
| Other formats (ALAC, WAV, AIFF, Opus, Vorbis) | later | via symphonia |
| DSD (DoP / native) | not yet | |
| MPD protocol compatibility | no | own JSON protocol instead |
| Network listener, HTTP streaming | not now | may come for LAN servers |
| Stored playlists | not now | |

## Requirements

- Linux with ALSA
- Rust 1.88 or newer
- the ALSA development files (`alsa-lib` on Arch/Artix, `libasound2-dev` on
  Debian/Ubuntu)

## Build and install

```sh
cargo build --release --workspace
install -m755 target/release/losslessd target/release/lossless ~/.local/bin/
```

## Configuration

`$XDG_CONFIG_HOME/lossless/config.toml` (usually
`~/.config/lossless/config.toml`). A missing file means all defaults.

```toml
# The ALSA device. Use the DAC's hardware device, addressed by card name
# (card numbers change across boots and replugs): see `aplay -l` or
# /proc/asound/cards for the name.
device = "hw:CARD=PRO2,DEV=0"

# Where your music lives. Relative paths given to `add` resolve here.
music_dir = "~/Music"

# Defaults to $XDG_RUNTIME_DIR/lossless.sock.
# socket = "/run/user/1000/lossless.sock"

# Volume: auto (default), hardware, software or none. auto uses the DAC's
# hardware mixer on a hw/plughw/front/sysdefault card, software gain on
# default/pipewire/pulse and none on null. Without a software mixer the
# samples are never touched; software gain at 100% is an exact passthrough.
# mixer = "auto"

# ALSA mixer control the hardware mixer drives (1 to 44 of A-Za-z0-9 _-).
# Percent is linear over its dB range (raw range without dB information).
# A card or control that can't be opened yet (say, the DAC is off) is tried
# again whenever the device opens and on `set_volume`; until then there is
# no volume (`no_mixer`).
# mixer_control = "PCM"

# Logging to stderr: off (default), warn, info or debug. warn notes the
# faults the daemon recovers from (skipped files, a missing mixer, dropped
# state entries); info adds startup and scans; debug every request, command
# and event. `losslessd --log-level <level>` overrides it.
# log_level = "off"
```

The config is validated strictly. `device` must be a plain device name:
`default`, `null`, `pipewire`, `pulse`, `sysdefault`, or
`hw`/`plughw`/`front`/`sysdefault` followed by `:CARD=<id>[,DEV=<n>]` (or
`:<id>,<n>`). Paths must be absolute or start with `~/`. Anything else is
refused at startup with a message saying why.

### State

The queue, the current entry, the position and the modes are saved to
`$XDG_STATE_HOME/lossless/state.json` (usually
`~/.local/state/lossless/state.json`, mode 0600) whenever they change, by
writing a temporary file and renaming it over the old one. On start the
daemon restores the session paused where it was, without opening the device
until you play. Files that have gone away are dropped (with `log_level`
warn, a warning gives the count); a corrupt or unreadable state file is logged and the queue starts
empty. A state directory that can't be created or written stops the daemon at
startup, before it binds the socket.
While playing, the position is saved on events only (no timer), so after a
crash playback resumes from the last pause, seek or track change.

### Profiles

One daemon drives one output. To have more (the DAC bit-perfect, and
PipeWire for when you share the sound card with other programs), run one
daemon per profile: `losslessd --profile NAME`, with `NAME` 1 to 32 of
`a-z 0-9 _ -`. Without the flag the profile is `default`, which uses the
paths above. A named profile gets its own:

| | path |
|---|---|
| config | `$XDG_CONFIG_HOME/lossless/NAME.toml` (missing file = defaults) |
| state | `$XDG_STATE_HOME/lossless/NAME/state.json` |
| socket | `$XDG_RUNTIME_DIR/lossless-NAME.sock`, unless the config sets `socket` |

For the DAC in `config.toml` plus PipeWire:

```toml
# ~/.config/lossless/pw.toml
device = "pipewire"
music_dir = "~/Music"
```

Run `losslessd --profile pw` as a second supervised service (a copy of the
one below with `--profile pw` added, and its own log directory), and reach
it with `lossless -p pw status`. Both daemons can play at once; a second
daemon for the same profile refuses to start while the first is listening.

Clients never read the daemon's config: they find a profile's socket from
its name alone. A profile whose config sets `socket` needs `--socket PATH`
in clients instead of `-p`.

### Keeping PipeWire or PulseAudio off the DAC

For exclusive, bit-perfect access the sound server must not open the DAC.
With WirePlumber, disable the device by name (from `pactl list cards
short`):

```lua
-- ~/.config/wireplumber/wireplumber.conf.d/52-ignore-dac.conf
monitor.alsa.rules = [
  {
    matches = [ { device.name = "alsa_card.usb-YOUR_DAC_NAME" } ]
    actions = { update-props = { device.disabled = true } }
  }
]
```

Your desktop audio keeps using the other device through PipeWire, and
lossless owns the DAC.

## Usage

Start the daemon in the foreground. It is silent by default; fatal errors
still reach stderr, and `--log-level warn|info|debug` turns logging on (see
`log_level` above):

```sh
losslessd
losslessd --log-level info
losslessd --profile pw           # a second output, see Profiles
```

and drive it with the client:

```sh
lossless update                  # scan music_dir
lossless search radiohead ok     # tracks matching every word
lossless browse [Radiohead]      # folders (trailing /) and tracks under music_dir
lossless artists                 # artist names
lossless albums [artist]         # album names, of one artist or all
lossless find --artist Radiohead --album "OK Computer"   # exact, either optional
lossless add "Radiohead/OK Computer"
lossless add --at 1 song.flac    # insert at position 1 (default: append)
lossless replace "Radiohead/OK Computer"   # swap the queue, play from the first
lossless remove 2 3              # 1-based queue positions
lossless move 3 1
lossless play                    # or: play 3 (1-based queue entry)
lossless status
lossless seek 2:30
lossless repeat queue            # off | queue | one
lossless random on               # on | off
lossless volume                  # print the level, or "no mixer"
lossless volume 40               # or: volume +5, volume -10 (clamped 0-100)
lossless pause | toggle | stop | next | prev
lossless queue
lossless clear
lossless watch                   # print events until interrupted
lossless --json status           # raw protocol replies
lossless -p pw status            # the pw profile's daemon (--socket wins)
```

`status` shows what the device was actually opened with, then the modes and
the volume when there is a mixer, e.g.
`playing  #1  0:12/4:31  output 24-bit/96 kHz/2ch repeat:off random:off vol 80%`.
To check from the kernel
side that nothing converted the stream, read the device's hardware
parameters while playing:

```sh
cat /proc/asound/PRO2/pcm0p/sub0/hw_params   # format S24_3LE, rate 96000
```

### Terminal UI

```sh
losslessd-tui                    # the default profile's daemon
losslessd-tui -p pw              # another profile (--socket PATH wins)
```

`?` lists the keys. Views are `1` Queue, `2` Directories, `3` Artists and
`4` Search; `o` switches to the next profile. `/` opens a live fuzzy filter
in every view (fzf-style, smart case): rows re-rank as you type, `Enter`
keeps the filter, `Esc` clears it. While the queue is filtered `J` and `K`
(move entry) are off; clear the filter first. Queries take fzf syntax:
words separated by spaces must all match, `'x` matches `x` exactly, `^x`
and `x$` anchor to the start or end, `!x` excludes. The filter is fuzzy,
unlike the daemon's `search` and `lossless search`, which need every word
as a plain substring. The UI keeps its own copy of the library and patches
it from `library_changed`, so filtering never asks the daemon.

Colors are Gruber Darker, painted as 24-bit RGB on a `#181818` background.

The UI is event-driven: it redraws only on input, daemon events and
whole-second progress while playing.

### Running under a supervisor

`losslessd` stays in the foreground and exits non-zero on fatal errors, so
any supervisor works. For an s6 user tree:

```execline
#!/bin/execlineb -P
backtick -n UID { id -u }
importas -u UID UID
foreground { loopwhilex if { test ! -d /run/user/${UID} } sleep 1 }
fdmove -c 2 1
export XDG_RUNTIME_DIR /run/user/${UID}
/home/you/.local/bin/losslessd
```

plus a `finish` script that sleeps a second or two, so a broken config
can't respawn in a tight loop.

## Protocol

Newline-delimited JSON over the unix socket. Each request carries an `id`
that its reply echoes:

```json
{"id":1,"cmd":"add","paths":["Radiohead/OK Computer"]}
{"type":"reply","id":1,"ok":{"kind":"done"}}

{"id":2,"cmd":"status"}
{"type":"reply","id":2,"ok":{"kind":"status","data":{"state":"playing","index":0,"position_ms":12000,"duration_ms":271000,"output":{"rate":96000,"bits":24,"channels":2},"track":{...}}}}
```

A failed request gets an error with a code and a message:

```json
{"id":3,"cmd":"play","index":14}
{"type":"reply","id":3,"err":{"code":"out_of_range","message":"index 14 out of range: queue has 12 entries"}}
```

| Code | When |
|---|---|
| `bad_request` | unparseable or oversized request, invalid values |
| `not_found` | a path that doesn't exist or isn't playable, an empty collection |
| `out_of_range` | a queue index or `at` outside the queue |
| `no_mixer` | volume without a mixer |
| `no_music_dir` | `update` or `browse` without `music_dir` |
| `scan_running` | `update` while a scan runs |
| `device` | the output device failed after retries |
| `decode` | a file failed to decode |
| `io` | any other I/O failure |

Besides `state`, `index`, `position_ms`, `duration_ms`, `output` and `track`,
status carries `repeat` (`off`, `queue`, `one`), `random`, `volume` (percent,
`null` without a mixer) and `last_error` (the last failure the daemon
recovered from by itself, as `{code, message}`, cleared once a track loads
cleanly).

After `{"id":4,"cmd":"subscribe"}` the connection also receives events.
A subscriber that falls about 256 events behind (stopped reading) is
disconnected; it resyncs on reconnect:

```json
{"type":"event","event":"state","state":"paused","position_ms":61250}
{"type":"event","event":"track_changed","index":1}
{"type":"event","event":"queue_changed"}
{"type":"event","event":"library_changed","generation":4,"tracks":914,"added":[{...}],"removed":["/music/a/old.flac"]}
{"type":"event","event":"mode","repeat":"queue","random":true}
{"type":"event","event":"volume","percent":40}
{"type":"event","event":"error","code":"decode","message":"x.flac: bad frame"}
```

`tracks` returns the whole library, `{"kind":"library","data":{"generation":N,"tracks":[...]}}`.
`library_changed` follows each rescan: `added` (new or changed tracks) and
`removed` (paths) patch a copy taken at `generation - 1`; a client that
sees a gap in `generation` fetches `tracks` again. `lossless watch` prints
it as `library changed: 914 tracks (+2 -1)`.

`volume` follows every `set_volume` and changes made on the card outside the
daemon; `error` reports a failure the daemon recovered from (a skipped file,
a device that stopped playback).

There are no position ticks: while playing, clients advance `position_ms`
from the last `state` event with their own clock. Requests are capped at
4 MiB per line. The types live in `crates/proto`.

## Security

- The socket is created with mode 0600 in `$XDG_RUNTIME_DIR` and only
  replaces an existing *stale socket* at that path, never another file or
  a symlink.
- No network listener.
- The config is treated as untrusted input: ALSA device names are
  restricted to plain hardware names, because ALSA's name syntax reaches
  plugins that write files or run shell commands. Paths are checked, and
  the file must be a regular file of at most 64 KiB.
- Tags are parsed only as FLAC or MP3, chosen by extension, never by
  sniffing the content.

## Development

```sh
cargo t            # all tests
cargo te | tlib | tproto | td | tc   # one crate
cargo cov          # coverage summary (cargo-llvm-cov)
cargo reldeps      # what the release binaries link
```

- Tests decode against generated FLAC fixtures and compare samples
  bit-for-bit; the player runs against a fake sink, the daemon over a real
  socket with a real-time fake device, and the CLI as a real binary.
- Property tests use proptest; fuzz targets (`fuzz/`, nightly
  `cargo fuzz`) cover requests, config files, device names, decoding and
  tags:

  ```sh
  cargo fuzz run decode fuzz/corpus/decode fuzz/seeds/decode -- -max_total_time=60
  ```

- Hardware test with digital silence only:
  `LOSSLESS_TEST_DEVICE=hw:CARD=<id>,DEV=0 cargo thw`.

## Roadmap

1. MPRIS bridge.

## License

[MIT](LICENSE)
