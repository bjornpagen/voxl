# Benchmarks

Tools used to compare voxl with selkies, KasmVNC, Neko, noVNC and Sunshine. Everything runs on a separate Xvfb display
(`:2` by default, `BENCH_DISPLAY` to change it), so a live desktop isn't disturbed.

## Setup

```sh
cc -O2 -o loadgen loadgen.c -lX11 -lXext      # 60 Hz animation with a frame/time marker
cc -O2 -o rootlat rootlat.c -lX11             # marker latency on the root window
env/xvfb.sh & sleep 1; env/openbox.sh &       # 2560x1600 @ 144 DPI, like production
env/voxl.sh                                   # the voxl daemon (VOXL_BIN to override); its relay serves :6902
env/selkies.sh                                # selkies (own Xvfb) :3, port 6912 (SELKIES_IMAGE to override)
BENCH_DISPLAY=:3 env/openbox.sh &
KASMVNC=/path/to/kasmvncserver XKB_DIR=/path/to/xkb env/kasmvnc.sh &  # KasmVNC (own X server) :5, port 6922
sleep 1; BENCH_DISPLAY=:5 env/openbox.sh &
```

`env/selkies.sh` runs selkies and the XLibre Xvfb from selkies' base image, since
selkies reads the screen in place only from its own X server; `docker logs selkies-bench`
is its log. CBR from 30 Mbps, with congestion control down to 2 Mbps.

`KASMVNC` is the monorepo's `nix/packages/kasmvnc` build; `XKB_DIR` is
`xkeyboard_config`'s `share/X11/xkb`. `env/kasmvnc.sh` passes the flags Tembo's
`kasmvnc.yaml` expands to.

`*.mjs` need Playwright (`PLAYWRIGHT_NODE_MODULES`) and Chromium (`CHROMIUM`).
`py.sh` runs Python with PyAV and websockets through nix-shell.

## Measuring

- `nclient.py voxl|selkies host:port secs [x y]`: native client (FFmpeg decode)
  that reads the marker at (x, y): for voxl, through the relay's page socket. Reports
  unique FPS, latency and Mbps.
- `link.py`: the link model `shaper.py` imposes: a rate through an unbounded queue served in order,
  half the round trip each way, one-sided jitter, a page's lateness, periodic stalls.
  `python3 -I link.py` checks it against an oracle that serves the queue a tick at a time,
  the lateness against the measured values, and the answers against a page's order.
- `keyhold.py host:port keysym repeat`: holds a key as a browser does, behind a link: the
  down, the browser's repeats (each sent as `k,<keysym>,<repeat>`, `1` as upstream's page
  sends a repeat or `2` as this one does), the up; every key message late by `--delay` ms
  and the up `--stall` ms later still. Prints how many presses it sent; what landed is
  counted on the desktop (`xev`).
- `kclient.py host:port secs [x y]`: native KasmVNC client. Speaks RFB over its
  WebSocket the way KasmVNC's web client does (same pixel format and encodings,
  including JPEG, WebP and CopyRect, continuous updates and fences), keeps a full
  framebuffer, decodes images in parallel and reads the marker after each update.
  Same JSON as `nclient.py`.
- `nkclient.py host:port secs [x y]`: native Neko client. Logs in and signals over
  Neko's WebSocket API like its web client, receives the WebRTC video with aiortc and
  reads the marker from every decoded frame. Needs aiortc (`pip install aiortc`). Same JSON.
- `suite2.sh`: noVNC and Neko through the same workloads and links as
  `suite.sh`; `cpusuite2.sh`: their server CPU. Start the servers with
  `env/novnc.sh` (x11vnc + websockify + noVNC, as E2B ships them; `X11VNC`, `NOVNC`,
  `WEBSOCKIFY`), `env/neko.sh` (Neko's official image against the benchmark display;
  `NEKO_SHAPE=mbps/rtt` shapes its UDP media with `tc netem` in its own container
  network).
- `mlclient.py host secs [x y]`: native Moonlight client for Sunshine. Streams with
  moonlight-embedded built by `moonlight/` (`nix-build moonlight`; its patch makes
  `-platform fake` hand each frame over as it arrives), decodes with FFmpeg and reads the
  marker. `MOONLIGHT` (the binary), `ML_HOME` (its pairing keys), `ML_BITRATE`. Same JSON.
- `suite3.sh`: Sunshine through the same workloads and links (`CONFIGS`: `sunshine`,
  `sunshine_tuned`; `REPS`), restarting it per run with `env/sunshine.sh` (`SUNSHINE`:
  the binary; `SUNSHINE_SHAPE=mbps/rtt` runs it in its own container network shaped
  with `tc netem`, like Neko). Pair once as `env/sunshine.sh` describes. `cpusuite2.sh`
  measures its CPU with `SERVERS="sunshine sunshine_tuned"`.
- `daytona_bench.py up|run|down`: a Daytona sandbox (default size, 2560x1600 desktop
  with x11vnc + noVNC) with the workloads in it, measured with `kclient.py` through
  Daytona's signed preview proxy (`KC_URL`, `KC_HEADERS`; `KC_CONNECT` to go through
  `shaper.py`). Needs the `daytona` SDK and `DAYTONA_API_KEY`.
- `suite.sh`: every config (`CONFIGS`: selkies with and without picom, voxl and
  KasmVNC with and without picom) x workload
  (Chromium scroll, 1080p and full-screen animation)
  x link (`NETS`: LAN, 10 Mbps/40 ms, 5 Mbps/60 ms, and 50 Mbps/20 ms with arrivals
  jittered by up to 10 ms) via `shaper.py`, a userspace bottleneck proxy (`--jitter`,
  `--late page`, `--stall`).
- `ab.sh <workload> <reps>`: interleaved voxl vs selkies runs.
- `cpusuite.sh`: server CPU (X server + streamer) per workload with one client connected.
- `scroll.html`: the long page `scrolltarget.mjs` smooth-scrolls, with a marker drawn every frame.
- `cpu.sh <pid> <secs>`, `srcfps.py` (true
  source frame rate from the Xvfb framebuffer), `measure.mjs` (in-browser unique FPS; `VIEWPORT=WxH`).
- `inputtest.mjs`, `keytest.sh`: input and clipboard checks through the web client.

## What is here

Everything above runs against `voxl relay` on port 6902 (`env/voxl.sh` starts the daemon;
the relay reaches it over ssh and the lanes, as it reaches a real desktop):
`nclient.py voxl` reads the relay's page socket, `measure.mjs`, `shot.mjs`,
`inputtest.mjs` and `keytest.mjs` drive its page in Chromium, `keyhold.py` and `smoke.py`
talk to its socket as the page does. The other clients (`kclient.py`, `nkclient.py`,
`mlclient.py`, `daytona_bench.py`) and `env/` scripts are the compared servers'. The
Playwright feature tests and the macOS tests that drove voxl's old built-in server are
gone. What they drove end to end (files, clipboard, the microphone, resizing) has no
end-to-end test against the relay yet; `smoke.py` covers the hello, video, the cursor,
input and the clipboard.
