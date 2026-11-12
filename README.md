# lossless

A music daemon for Linux, written in Rust, that plays every file
**bit-perfect**: the samples in the file are the samples that reach the DAC,
at the file's own sample rate and bit depth. It aims to cover what most
people use MPD for (a library, a queue, gapless playback, clients that
come and go) without ever putting a resampler, a mixer or a format
converter in the signal path.

- `losslessd`: the daemon
- `lossless`: a command line client
- `losslessd-tui`: a terminal UI (planned, see [Roadmap](#roadmap))

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
| Volume: DAC hardware mixer, or software gain on the PipeWire path | planned | the bit-perfect path never scales samples |
| Queue and position restored after restart | planned | |
| Terminal UI (`losslessd-tui`) | planned | |
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
# mixer_control = "PCM"
```

The config is validated strictly. `device` must be a plain device name:
`default`, `null`, `pipewire`, `pulse`, `sysdefault`, or
`hw`/`plughw`/`front`/`sysdefault` followed by `:CARD=<id>[,DEV=<n>]` (or
`:<id>,<n>`). Paths must be absolute or start with `~/`. Anything else is
refused at startup with a message saying why.

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

Start the daemon in the foreground (it logs to stderr):

```sh
losslessd
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
lossless replace "Radiohead/OK Computer"   # swap the queue, stops playback
lossless remove 2 3              # 1-based queue positions
lossless move 3 1
lossless play                    # or: play 3 (1-based queue entry)
lossless status
lossless seek 2:30
lossless repeat queue            # off | queue | one
lossless random on               # on | off
lossless pause | toggle | stop | next | prev
lossless queue
lossless clear
lossless watch                   # print events until interrupted
lossless --json status           # raw protocol replies
```

`status` shows what the device was actually opened with, then the modes,
e.g. `playing  #1  0:12/4:31  output 24-bit/96 kHz/2ch repeat:off random:off`.
To check from the kernel
side that nothing converted the stream, read the device's hardware
parameters while playing:

```sh
cat /proc/asound/PRO2/pcm0p/sub0/hw_params   # format S24_3LE, rate 96000
```

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

After `{"id":3,"cmd":"subscribe"}` the connection also receives events:

```json
{"type":"event","event":"state","state":"paused","position_ms":61250}
{"type":"event","event":"track_changed","index":1}
{"type":"event","event":"queue_changed"}
{"type":"event","event":"library_changed","tracks":914}
```

There are no position ticks: while playing, clients advance `position_ms`
from the last `state` event with their own clock. Requests are capped at
64 KiB per line. The types live in `crates/proto`.

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

1. Daemon: typed errors with codes, leveled logging (off by default), queue
   insert/remove/move/replace, repeat and random, browsing by folder,
   artist and album, hardware and software volume, a state file.
2. `losslessd-tui`: an rmpc-style terminal UI (queue, directories,
   artists/albums, search).
3. MPRIS bridge.

## License

[MIT](LICENSE)
