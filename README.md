# voxl

A fork of [tembo/voxl](https://github.com/tembo/voxl) that went its own way: it carries the desktop over one stream through an HTTP CONNECT proxy, and lets the transport under it do its job.

Low-latency desktop streaming for the browser, written in Rust. voxl streams an X11
display as AV1 to a small web client that decodes
with WebCodecs, with input, clipboard, audio, microphone, file and folder transfer and
dynamic resolution. When the link slows, it skips screens rather than queue them, and ssh
is the only way in.

## Quick start

On the machine with the desktop, run the daemon (it listens on unix sockets only):

```sh
cargo install --git https://github.com/bjornpagen/voxl   # needs libaom, libyuv, Opus, PulseAudio and esbuild (see Usage)
DISPLAY=:1 voxl             # streams display :1 to the sessions that attach to it
```

On the machine you watch from, name the HTTP proxy that reaches the server and run the
relay:

```sh
voxl relay --proxy 127.0.0.1:1080 --ssh desktop --port 6901
```

Open `http://127.0.0.1:6901/`. The daemon runs on Linux; the relay runs on Linux and
macOS.

## How it works

```
browser page --WebSocket, 127.0.0.1--> voxl relay (the viewer's machine)
   voxl relay --ssh--> voxl attach --unix socket--> voxl daemon   (the control stream)
   voxl relay --one stream, through a proxy--> the server's proxy --unix socket--> voxl daemon
```

- **The relay** (`voxl relay`) runs where you watch. It serves the page on 127.0.0.1 and,
  for each page that connects, runs a session.
- **The control stream** is `ssh <host> voxl attach`: whatever ssh's own configuration
  makes of the host (its keys, its master connection) is the door. It carries everything
  but the picture and the sound: the hello, input, clipboard, cursor images, the
  microphone, and the relay's word of each frame the page took. `voxl attach` only joins
  ssh's stdio to the daemon's control socket, which takes connections from the daemon's
  own user and no one else.
- **The lane** is one HTTP CONNECT stream through the proxy, whose server end reaches the
  daemon's lane socket at `localhost:6901`. It carries video and audio, interleaved, one
  way, from the daemon to the relay. Each session's daemon makes a 256-bit capability and
  sends it first on the control stream; the lane presents it as its first bytes, and the
  daemon reads nothing else a lane sends. The capability dies with the control stream, and
  the lane attached with it is dropped. voxl itself encrypts nothing: the tunnel encrypts
  the wire, and every other hop is on one machine.
- **Delivery is told, never inferred.** The relay gives the page one frame at a time, and
  the next only once the page says it took the one before: it is done with that message,
  an update decoded and its picture handed to the video track, an Opus packet handed to
  the audio decoder. So the page holds at
  most one undecoded update, and nothing queues in the browser, whose socket would
  otherwise hold about a megabyte where no one sees it. The relay tells the daemon each
  frame the page took, update or Opus packet, on the control stream, and that is all the
  daemon knows of delivery: its socket accepts far more than has arrived. A screen is
  sent to a viewer only
  while what was sent it and not yet taken is within what its path carries in its round
  trip and the response time (a tenth of a second), and no frame is late: none sent has
  gone untaken longer than twice the round trip, plus the response time, plus its own
  bytes' time at the path's rate. The path is what the transport states: the tunnel server
  hands the lane's socket, with its first bytes, a statement of its connection's least
  round trip and delivery rate, and voxl estimates nothing. Until the first record comes,
  or on a lane whose transport states nothing, a frame not yet taken holds the next. A screen is captured while any viewer's link lets one go. A slow
  link or a slow page lowers its own viewer's frame rate, and the newest screen goes next;
  it holds no other viewer, nor a recording, back: frames go on without it, and it starts
  again on a keyframe once its link lets one go. Every frame is coded whole, at one
  quality.
- **The stream.** A viewer starts on a keyframe, and every frame after it refers to the
  frames before it, which the lane carried in order. Keyframes are sent only when a viewer
  starts, which a viewer that frames went on without, or a resize, also is. A lane that
  reopens says which frame its page last took, and goes on with a delta when that is
  still the newest. The desktop's size is the keyframe's: the page lays itself out from the
  pictures it decodes.

## Benchmarks

https://github.com/user-attachments/assets/acdf3f31-64fc-4acf-b5c5-ff173d7100a8

In every test below voxl averages 60 FPS with a median latency of 12–99 ms; noVNC
averages under 8 FPS.

A 16 vCPU AMD EPYC 9334 VM (no GPU), 2560x1600 Xvfb desktop, same workloads, each
stream decoded by a native client that reads a pixel marker drawn into the content.
Cells are FPS (worst second), then latency p50/p99 from the content being drawn to the
frame being decoded. voxl's column is its best run of each test. To reproduce, see
[`bench/`](bench/README.md).

| Test                           | voxl                       | [selkies](https://github.com/selkies-project/selkies) | [KasmVNC](https://github.com/kasmtech/KasmVNC) | [Neko](https://github.com/m1k1o/neko) | [Sunshine](https://github.com/LizardByte/Sunshine) | [Sunshine, tuned](https://github.com/LizardByte/Sunshine) | [noVNC](https://github.com/novnc/noVNC) | [Daytona](https://www.daytona.io) |
| ------------------------------ | -------------------------- | ----------------------------------------------------- | ---------------------------------------------- | ------------------------------------- | -------------------------------------------------- | --------------------------------------------------------- | --------------------------------------- | --------------------------------- |
| Chromium scroll, LAN           | **60.1 (59.7), 55/60 ms**  | 56.8 (54.0), 85/106 ms                                | 51.7 (39.5), 99/135 ms                         | 25.0 (23.7), 156/217 ms               | 28.3 (23.5), 88/115 ms                             | 48.5 (44.0), 94/117 ms                                    | 6.7 (6.3), 198/301 ms                   | 5.2 (2.5), 306/713 ms             |
| Scroll, 10 Mbps / 40 ms RTT    | **60.0 (59.4), 82/94 ms**  | 55.6 (47.7), 152/261 ms                               | 8.3 (7.4), 229/249 ms                          | 24.6 (14.9), 221/737 ms               | 27.9 (25.3), 160/179 ms                            | 51.3 (47.5), 147/162 ms                                   | 2.6 (2.4), 426/678 ms                   | 1.9 (1.6), 617/798 ms             |
| Scroll, 5 Mbps / 60 ms RTT     | **60.1 (59.7), 99/109 ms** | 27.9 (19.6), 2.7/3.7 s                                | 4.9 (3.6), 306/415 ms                          | 21.7 (3.8), 1.2/3.6 s                 | 29.6 (21.9), 178/207 ms                            | 44.5 (35.2), 164/191 ms                                   | 1.6 (1.4), 637/937 ms                   | 1.1 (0.9), 1.1/1.2 s              |
| 1080p animation, LAN           | **60.1 (59.9), 12/16 ms**  | 58.7 (55.9), 31/48 ms                                 | 56.7 (51.6), 51/82 ms                          | 24.8 (23.6), 122/202 ms               | 27.1 (22.7), 69/91 ms                              | 45.6 (40.3), 42/57 ms                                     | 7.7 (7.2), 149/210 ms                   | 10.5 (8.8), 138/209 ms            |
| Animation, 10 Mbps / 40 ms RTT | **60.1 (59.5), 46/71 ms**  | 39.3 (31.8), 2.3/3.1 s                                | 52.9 (49.2), 76/115 ms                         | 25.1 (9.1), 295/962 ms                | 29.7 (26.0), 108/125 ms                            | 51.1 (44.6), 86/107 ms                                    | 5.8 (5.2), 160/226 ms                   | 7.4 (7.0), 154/213 ms             |
| Animation, 5 Mbps / 60 ms RTT  | **60.0 (58.8), 66/92 ms**  | 18.4 (15.3), 2.2/4.3 s                                | 34.1 (26.4), 104/156 ms                        | 15.2 (8.6), 5.0/9.4 s                 | 34.7 (25.4), 123/143 ms                            | 45.5 (40.9), 114/131 ms                                   | 5.1 (4.7), 174/240 ms                   | 5.8 (4.4), 189/252 ms             |
| Full-screen animation, LAN     | **59.8 (54.0), 18/25 ms**  | 54.8 (49.8), 54/79 ms                                 | 34.2 (32.5), 83/102 ms                         | 22.7 (18.0), 2.9/3.3 s                | 24.9 (21.7), 73/98 ms                              | 47.4 (43.4), 46/65 ms                                     | 5.9 (5.2), 184/279 ms                   | 9.1 (7.2), 154/247 ms             |

| Server CPU, X server + streamer (cores)                   | idle | 1080p animation  | scroll           | full screen      |
| --------------------------------------------------------- | ---- | ---------------- | ---------------- | ---------------- |
| voxl                                                      | 0.00 | 1.39             | 1.33             | 1.68             |
| [selkies](https://github.com/selkies-project/selkies)     | 1.29 | not measured     | 1.54             | 1.80             |
| [KasmVNC](https://github.com/kasmtech/KasmVNC)            | 0.00 | 1.97             | 2.07             | 1.82 (at 34 FPS) |
| [Neko](https://github.com/m1k1o/neko)                     | 1.63 | 2.50 (at 25 FPS) | 2.20 (at 25 FPS) | 2.46 (at 23 FPS) |
| [Sunshine](https://github.com/LizardByte/Sunshine)        | 2.10 | 2.34 (at 27 FPS) | 2.24 (at 28 FPS) | 2.35 (at 25 FPS) |
| [Sunshine](https://github.com/LizardByte/Sunshine), tuned | 2.84 | 3.07 (at 46 FPS) | 3.14 (at 49 FPS) | 3.22 (at 47 FPS) |
| [noVNC](https://github.com/novnc/noVNC)                   | 0.01 | 0.55 (at 8 FPS)  | 0.58 (at 7 FPS)  | 0.64 (at 6 FPS)  |
| [Daytona](https://www.daytona.io) (1 CPU limit)           | 0.02 | 0.82 (at 10 FPS) | 0.53 (at 5 FPS)  | 0.81 (at 9 FPS)  |

- **selkies:** [selkies](https://github.com/selkies-project/selkies) with picom running.
- **KasmVNC:** 1.4.0 with a 60 FPS cap, JPEG/WebP quality 8–9 and video mode, no
  compositor.
- **Neko:** [m1k1o/neko](https://github.com/m1k1o/neko) 3.2 with its defaults (VP8, 25
  FPS). It doesn't adapt its bitrate (its estimator is off by default), so on slow links
  frames queue up.
- **Sunshine:** [Sunshine](https://github.com/LizardByte/Sunshine) 2025.628 streaming to
  Moonlight ([moonlight-embedded](https://github.com/moonlight-stream/moonlight-embedded)
  2.7, patched to hand frames to the benchmark client). Sunshine is built around GPU
  capture and encoding; without a GPU it falls back to copying the X11 screen and a software H.264 encoder,
  so these numbers show it on a GPU-less VM like the other tests, not on the gaming PC
  it's made for. **Sunshine** runs its defaults (`superfast` on 2 threads) at Moonlight's
  default 40 Mbps; **Sunshine, tuned** uses `ultrafast` on 4 threads at 30 Mbps (8 or 16
  threads did no better). Neither reaches 60 FPS on this VM, and it keeps
  working while the screen is still (its idle CPU). Moonlight doesn't adapt its bitrate,
  so on slow links it's set to 80% of the link. Its CPU was measured in a later session,
  where voxl used 1.74, 1.65 and 1.99 cores on the same workloads.
- **noVNC:** x11vnc and noVNC as [E2B Desktop](https://github.com/e2b-dev/desktop)
  and Daytona ship them (`-forever -wait 50 -shared`, Tight encoding). x11vnc polls the
  screen, which caps it at 6–8 FPS.
- **Daytona:** a default sandbox (1 CPU, x11vnc and noVNC at 2560x1600) streamed
  through Daytona's signed preview proxy, about 12 ms away, which its latency includes.
  The workloads run inside the sandbox, sharing its CPU, and slow links are added on top
  of that internet path.
- **Links:** a bottleneck with a deep queue: a TCP proxy for the WebSocket servers, and
  `tc netem` for Neko's WebRTC media and Sunshine's video (UDP). None of them drop
  packets, where UDP with error correction would have the edge.
- These runs predate the lane through a proxy and the AV1 encoder: voxl's video then
  crossed one WebSocket, as the other WebSocket servers' did, coded by the encoder voxl
  had before. They have not been run again.

## Usage

Build (needs Rust, pkg-config, clang, libaom, libyuv, libopus, libpulse and esbuild;
`nix-shell` provides them all):

```sh
nix-shell
cargo build --release
```

Or install it from this repository with the same libraries on your system (the crate is
`voxl-server`, the binary is `voxl`; crates.io's `voxl-server` is the original, not this fork):

```sh
cargo install --git https://github.com/bjornpagen/voxl
```

One binary does it all: `voxl` runs the daemon and `voxl relay` the relay; `voxl attach`
and `voxl files` are what the relay runs over ssh, and `voxl screenshot`, `voxl record`
and `voxl stats` are for agents and scripts.

### The daemon

Run it against an X display:

```sh
DISPLAY=:1 ./target/release/voxl
```

It listens on two unix sockets and no network port, all in its runtime folder
(`--runtime`), with the microphone's FIFO and its scratch keymap: `control`,
`lane-stream`, `mic` and `keymap`. The folder must be the daemon user's,
with no write bit for others, or the daemon does not start; make it at boot (a tmpfiles.d
rule) with the group the proxy's user is in. The server's proxy maps `localhost:6901` to
the lane socket: TCP to `lane-stream`. PulseAudio's
`module-pipe-source` reads the microphone from `mic`. Start Xvfb with `-shmem` for zero-copy
capture, and use the patched Xvfb
(`nix-build -E 'with import <nixpkgs> {}; callPackage ./xvfb.nix {}'`) for smooth
scrolling. A held key repeats as the viewer's keyboard repeats it, so while voxl runs no
keyboard on its X server autorepeats, and if voxl crashes that stays so until the X server
restarts. A flag the daemon does not read stops it at startup. `voxl --help` lists every
flag with its default, and `voxl --version` prints the version.

| Flag              | Default     |                                                     |
| ----------------- | ----------- | --------------------------------------------------- |
| `--runtime`       | `/run/voxl` | the folder of the sockets and the microphone FIFO   |
| `--resize-scale`  | `1.5`       | desktop pixels per CSS pixel of the viewer's window |
| `--audio-enabled` | `true`      | desktop audio                                       |

It needs `DISPLAY` (the X display it streams) and `HOME` (the folder the Files panel
reaches, opening and uploading into `Downloads`), and starts nothing without them. The clipboard, the Files panel, the microphone and dynamic resolution (the
desktop follows the viewer's window) are always on.

### The relay

```sh
voxl relay --proxy HOST:PORT --ssh DESTINATION --port PORT [--runtime DIR] [--place localhost:PORT]
```

| Flag        |                                                                                                  |
| ----------- | ------------------------------------------------------------------------------------------------ |
| `--proxy`   | the HTTP proxy the lane goes through (`host:port`; an IPv6 address in brackets)                  |
| `--ssh`     | where `voxl attach` and `voxl files` run: anything `ssh` takes as its destination                |
| `--port`    | the page's port on `127.0.0.1`                                                                   |
| `--runtime` | the runtime folder of the daemon `voxl attach` reaches, on the server; else the daemon's default |
| `--place`   | where the lane goes, as the server's proxy is asked for it; else the protocol's `localhost:6901` |

`--proxy`, `--ssh` and `--port` are required; `--runtime` and `--place` are for a server
that runs more than one daemon, each in a folder of its own with its lane at a port of its
own on the server's loopback (`localhost:PORT`). The relay listens on `127.0.0.1` only, and
answers only requests for an IP address or `localhost`, from its own page. That keeps out
other web pages, which browsers let connect to any address, including pages whose domain
is pointed at 127.0.0.1 (DNS rebinding). Programs that aren't browsers send no `Origin`
and get through.

A session's WebSocket, its ssh and its lane start together and end together; the page
shows why a session ended, and clicking it starts a new one. Nothing reconnects by
itself, but a lane the transport closes is opened again, and its viewer goes on from the
frame its page holds: a keyframe only if frames went on without it.

### `voxl attach` and `voxl files`

The relay runs these over ssh; you don't run them yourself. `voxl attach [--runtime
DIR]` joins its stdin and stdout to the daemon's control socket for one session. `voxl
files list|get|put` serves one request of the Files panel, confined to the folder the
daemon's hello names, so a transfer never queues behind the stream or in front of it.

### For agents and scripts

Screenshots, recordings and the daemon's numbers come over ssh too, through the daemon's
control socket, each command one run:

```sh
# the screen as a PNG
ssh desktop voxl screenshot > shot.png
# an MP4 of the next 10 s
ssh desktop voxl record --seconds 10 > clip.mp4
# or until its stdin ends (Ctrl-D in a terminal, or the end of a script's input)
ssh desktop voxl record > clip.mp4
# what the daemon is doing, as JSON
ssh desktop voxl stats
```

A recording is the AV1 voxl already encodes, saved without encoding it again,
so it costs almost no CPU. With `--audio-enabled`, it carries the desktop's sound as an
Opus track beside the video, with the silences kept as gaps. It doesn't include the cursor (viewers draw it), and a restart
of the stream (a resize) or five minutes ends it. Each command
takes `--runtime DIR`, as `voxl attach` does, for a daemon whose runtime folder is not
`/run/voxl`.

## FAQ

**Why one stream, and no code of its own against loss?** voxl tried datagrams with
forward error correction, and on a real path most loss turned out to be its own
congestion: repair added load, which added loss. A stream through a QUIC or TCP transport
leaves loss and pacing to the transport, which is built for them, and voxl keeps lag down
at the source instead: it sends a page a screen only while the page has taken what was
sent it before, as fast as its link has been delivering, so a slow link lowers that
page's frame rate rather than queueing old screens. The page still talks to one WebSocket, on 127.0.0.1,
takes one update at a time and decodes it the moment it arrives with WebCodecs.

**How is it different from Moonlight and Sunshine?** They're for playing games on your
own GPU machine through the Moonlight app, and for that they're the better fit. voxl was
built for things like software factories that use headless VMs and sandboxes (CI,
computer-use agents), usually on Xvfb with no GPU, reached over ssh. It streams them to
a plain browser tab, through a proxy.

**How is it different from RustDesk?** RustDesk is an open-source alternative to
TeamViewer and AnyDesk, for reaching your own computers or helping someone with theirs:
you install its app on both ends, desktop or phone, and the two find each other through
an ID server, then connect directly or through a relay. For that it's the better fit,
and it also shares Wayland desktops (experimentally) and Android phones, which voxl
doesn't. voxl is for desktops on servers and VMs reached over ssh: there is no ID server,
and the viewer's side is one binary that serves its own page.

**Is it safe to expose?** The daemon listens on no network port: its sockets are unix
sockets, the control socket takes only the daemon's own user, and a lane counts only
with the session's capability, which only that session's ssh channel ever carried. ssh is
the door, so whoever can ssh in as that user gets the desktop. See
[SECURITY.md](SECURITY.md).

**Which browsers and devices work?** A current Chrome or Edge on a desktop, with a mouse
or trackpad. The page needs WebCodecs video and audio, MediaStreamTrackGenerator,
ClipboardItem and the Keyboard Lock API, and a browser without one of them is told which
it lacks, rather than run without it. Firefox, Safari and phones are not supported.

**Touchscreens?** A touchscreen works: tap to click, drag, long press to right click, two
fingers to scroll, pinch to zoom the view (the desktop is unchanged), and an on-screen
keyboard that only the toolbar's keyboard button raises. The toolbar's trackpad button
switches the touchscreen to a laptop trackpad: slide to move the pointer, tap to click
where it is, hold still to grab and slide to drag, two fingers lifted together to right
click.

**Wayland?** Not yet; on Linux voxl streams X11 displays, Xvfb included. The virtual
desktops voxl is built for run on Xvfb (E2B's and Daytona's do too), and X11 lets voxl
capture them without a permission prompt, with damage events that keep a still screen at
no CPU.

**Does it use the GPU?** Not to encode: voxl encodes AV1 on the CPU with libaom, in its
realtime mode with the tools made for screens (palette coding), at one quantiser. The
browser decodes it in hardware where the machine has an AV1 decoder (Apple M3 and later)
and in software elsewhere, about a core while the whole screen moves. (The CPU table above
predates the AV1 encoder.)

**What's the license?** voxl's code is MIT. It links libaom, libyuv and Opus, each under a
BSD license.
