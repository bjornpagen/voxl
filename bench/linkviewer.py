"""A headless voxl viewer behind an emulated link, for measuring rate control.

Speaks voxl's protocol (web/src/protocol.ts): turns video on, answers pings and acks
every frame. It does not decode. Instead it models what a viewer behind a bottleneck sees:

- Each message arrives at `max(received, link free) + its bytes / rate`, plus half the
  round trip: a link of `--kbps` with an unbounded queue that serves in order, like
  `shaper.py`. Pings and every other message take their turn on it.
- A frame is acknowledged once its last part has arrived (parts = the header's
  "parts in frame"), never at its first.
- Before the ack the viewer holds the frame for its decode and draw: 3 ms, or with
  `--hold` a software decoder's time (2 ms + 0.1 ms per KB + 0 to 8 ms at random).
  `--report-hold` sends that hold in the ack (`a,<id>,<ms>`). It is the same number the
  ack was delayed by, on the viewer's own monotonic clock, measured from the moment the
  frame's last part arrived on the emulated link.
- The ack then takes the other half of the round trip, on the same monotonic clock.

All of it runs on one asyncio loop, so a busy machine only ever makes messages arrive
and acks leave later than the model says, never earlier.

It can also drive the desktop while it watches: trackpad-like scroll gestures (`sp`
commands at 60 Hz) and shell commands written to a FIFO that a terminal on the
benchmark display reads (`--fifo`; a terminal running `while read -r l < FIFO; do eval
"$l"; done`). Scenarios name what to do; `{page}` in their commands is `--page`.

The `hold` scenario holds a key (`--keysym`) for `--key-hold` seconds as a browser does:
a down, a repeat after the first `--key-repeat` delay and then every interval, an up. Key
messages leave `--key-delay` ms late, and the up `--key-stall` ms later still, as when the
segment carrying it is lost and sent again. What lands is counted on the desktop (xev);
OUT.marks.tsv says how many characters were sent (`sent`: the down and its repeats).

Writes OUT.msgs.tsv (each video message: when received, when it arrived on the link,
frame, key, paint-over, bytes, server pipeline ms), OUT.acks.tsv, OUT.marks.tsv (the
scenario's phases), OUT.scroll.tsv (scroll input) and OUT.keys.tsv (key messages, when
each was due and when it was sent).

usage: linkviewer.py HOST:PORT SCENARIO OUT [--kbps N] [--rtt MS] [--hold] [--report-hold]
       [--seed N] [--fifo PATH] [--page FILE] [--passive SECS]
       [--keysym K] [--key-hold S] [--key-repeat MS,MS] [--key-delay MS] [--key-stall MS]
       linkviewer.py --check-header   (the standard library alone; python3 -I runs it)
"""

import argparse
import asyncio
import heapq
import json
import math
import os
import random
import sys
import time

# Messages, and the fields they mean, that the server's and the web client's header
# readers are checked against too.
HEADER_FIXTURE = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "web", "src", "video-header.json"))
VIDEO = 0x01  # a video message's type byte
HEADER_LEN = 16


def read_header(m):
    """The fields this viewer reads from a video message's header (src/video.rs `Header`),
    None for any other message."""
    if len(m) <= HEADER_LEN or m[0] != VIDEO:
        return None
    return {
        "key": m[1] & 1 == 1,
        "sharpens": m[1] & 2 == 2,
        "frame": int.from_bytes(m[2:6], "big"),
        "pipe": int.from_bytes(m[6:10], "big"),
        "parts": m[12],
    }


def check_header():
    with open(HEADER_FIXTURE) as f:
        cases = json.load(f)
    for c in cases:
        h = read_header(bytes.fromhex(c["header"] + c["picture"]))
        if h != {field: c[field] for field in h}:
            sys.exit(f"{c['header']} reads as {h}")
    print(f"read_header: {len(cases)} messages as {HEADER_FIXTURE} says")


def flick(peak_px_s, tau_s, sign):
    """Per-frame deltas of a trackpad flick: 100 ms of push, then momentum decaying with tau."""
    out = [sign * peak_px_s * (i + 1) / 6 / 60 for i in range(6)]
    v = peak_px_s
    while v > 30:
        out.append(sign * v / 60)
        v *= math.exp(-1 / 60 / tau_s)
    return out


def steady(px_s, secs, sign):
    return [sign * px_s / 60] * int(secs * 60)


SCENARIOS = {
    # Fill the scrollback, let it settle, then scroll back through it as a person does.
    "scroll": [
        ("term", "clear; cat {page}"),
        ("wait", 4.0),
        ("mark", "flick1"),
        ("scroll", flick(6000, 0.35, -1)),
        ("wait", 1.5),
        ("mark", "flick2"),
        ("scroll", flick(3000, 0.35, -1)),
        ("wait", 1.5),
        ("mark", "read"),
        ("scroll", steady(600, 3.0, -1)),
        ("wait", 2.0),
        ("mark", "end"),
    ],
    # Output streaming in: one line every 40 ms.
    "stream": [
        ("term", "clear; head -400 {page}"),
        ("wait", 3.0),
        ("mark", "stream"),
        ("term", "head -150 {page} | while IFS= read -r l; do printf '%s\\n' \"$l\"; sleep 0.04; done"),
        ("wait", 8.0),
        ("mark", "end"),
    ],
    # Motion, then 4 s of stillness each time: how sharp it gets, how soon, for how much.
    "sharpen": [
        ("mark", "page"),
        ("term", "clear; cat {page}"),
        ("wait", 4.5),
        ("mark", "flick"),
        ("scroll", flick(6000, 0.35, -1)),
        ("wait", 4.0),
        ("mark", "read"),
        ("scroll", steady(600, 2.0, -1)),
        ("wait", 4.0),
        ("mark", "end"),
    ],
    # A key held for --key-hold seconds, repeated as a browser does, behind the link.
    "hold": [
        ("wait", 1.0),
        ("mark", "down"),
        ("key", None),
        ("mark", "end"),
    ],
    # A burst of output, then stillness, then a flick.
    "idle": [
        ("term", "clear; head -300 {page}"),
        ("wait", 1.5),
        ("mark", "still"),
        ("wait", 10.0),
        ("mark", "after"),
        ("scroll", flick(6000, 0.35, -1)),
        ("wait", 1.5),
        ("mark", "end"),
    ],
}


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("hostport")
    ap.add_argument("scenario", choices=sorted(SCENARIOS))
    ap.add_argument("out")
    ap.add_argument("--kbps", type=float, default=0, help="bottleneck rate (0: none)")
    ap.add_argument("--rtt", type=float, default=0, help="round trip (ms)")
    ap.add_argument("--hold", action="store_true", help="hold frames as a software decoder would")
    ap.add_argument("--report-hold", action="store_true", help="send the hold in the ack")
    ap.add_argument("--fifo", help="FIFO a terminal on the benchmark display reads commands from")
    ap.add_argument("--page", default="/etc/services", help="file the scenarios print")
    ap.add_argument("--settle", type=float, default=3.0, help="seconds to watch before the scenario")
    ap.add_argument("--passive", type=float, default=0, help="only watch, for this many seconds")
    ap.add_argument("--seed", type=int, help="seed for --hold's random part, so a run can be repeated")
    ap.add_argument("--keysym", type=lambda v: int(v, 0), default=0x61, help="key the hold scenario holds")
    ap.add_argument("--key-hold", type=float, default=3.0, help="seconds the key is held")
    # Not X's own schedule (500 ms then 50 ms under xfsettingsd, 660 then 40 bare), so the
    # count of characters says whose repeats landed.
    ap.add_argument("--key-repeat", default="300,33", help="the browser's repeat: delay,interval in ms")
    ap.add_argument("--key-delay", type=float, default=0, help="one-way delay of every key message (ms)")
    ap.add_argument("--key-stall", type=float, default=0, help="extra delay of the key-up alone (ms)")
    a = ap.parse_args()
    rng = random.Random(a.seed)
    owd = a.rtt / 2000.0
    rate = a.kbps * 1000.0
    t0 = time.monotonic()
    clock = lambda: time.monotonic() - t0
    link_free = 0.0
    sendq = []
    seq = 0
    frames = {}
    got_key = asyncio.Event()
    msgs = open(f"{a.out}.msgs.tsv", "w")
    msgs.write("t_recv\tt_arrive\tframe\tkey\tpaintover\tbytes\tpipe_ms\n")
    acks = open(f"{a.out}.acks.tsv", "w")
    marks = open(f"{a.out}.marks.tsv", "w")
    scrolls = open(f"{a.out}.scroll.tsv", "w")
    keys = open(f"{a.out}.keys.tsv", "w")
    keys.write("t_intended\tt_sent\tmsg\n")

    def arrive(nbytes):
        """When a message received now has crossed the emulated link."""
        nonlocal link_free
        now = clock()
        depart = max(now, link_free) + nbytes * 8 / rate if rate else now
        link_free = depart
        return depart + owd

    def schedule(at, msg):
        nonlocal seq
        seq += 1
        heapq.heappush(sendq, (at, seq, msg))

    def term(cmd):
        if a.fifo:
            with open(a.fifo, "w") as f:
                f.write(cmd.replace("{page}", a.page) + "\n")

    import websockets

    async with websockets.connect(f"ws://{a.hostport}/ws", max_size=None, compression=None) as ws:

        async def sender():
            # Sends each scheduled message once its time has come (at most 2 ms late).
            while True:
                if sendq and sendq[0][0] <= clock():
                    at, _, msg = heapq.heappop(sendq)
                    await ws.send(msg)
                    if msg.startswith("k,"):
                        keys.write(f"{at:.4f}\t{clock():.4f}\t{msg}\n")
                    continue
                wait = sendq[0][0] - clock() if sendq else 0.002
                await asyncio.sleep(min(max(wait, 0), 0.002))

        async def receiver():
            async for m in ws:
                if isinstance(m, str):
                    j = json.loads(m)
                    at = arrive(len(m))
                    if j.get("t") == "ping":
                        schedule(at + owd, f"po,{j['s']}")
                    continue
                at = arrive(len(m))
                h = read_header(m)
                if h is None:
                    continue
                fid, key, parts = h["frame"], h["key"], h["parts"]
                msgs.write(f"{clock():.4f}\t{at:.4f}\t{fid}\t{key:d}\t{h['sharpens']:d}\t{len(m)}\t{h['pipe'] / 10:.1f}\n")
                f = frames.setdefault(fid, {"need": parts, "got": 0, "bytes": 0})
                f["got"] += 1
                f["bytes"] += len(m)
                if key:
                    got_key.set()
                if f["got"] < f["need"]:
                    continue
                # The frame's last part is in: hold it for decode and draw, then ack.
                del frames[fid]
                hold = 0.002 + f["bytes"] / 1e7 + rng.uniform(0, 0.008) if a.hold else 0.003
                ack_at = at + hold + owd
                acks.write(f"{fid}\t{at:.4f}\t{ack_at:.4f}\t{hold * 1000:.1f}\n")
                schedule(ack_at, f"a,{fid},{hold * 1000:.1f}" if a.report_hold else f"a,{fid}")

        tasks = [asyncio.create_task(sender()), asyncio.create_task(receiver())]
        await ws.send("v,1")
        await asyncio.wait_for(got_key.wait(), 30)
        if a.passive:
            marks.write(f"watch\t{clock():.4f}\n")
            await asyncio.sleep(a.passive)
            marks.write(f"end\t{clock():.4f}\n")
        else:
            await ws.send("m,1512,1000")
            await asyncio.sleep(a.settle)
            for kind, arg in SCENARIOS[a.scenario]:
                if kind == "term":
                    term(arg)
                elif kind == "wait":
                    await asyncio.sleep(arg)
                elif kind == "mark":
                    marks.write(f"{arg}\t{clock():.4f}\n")
                elif kind == "key":
                    # Down, the browser's repeats, up: each leaves after --key-delay, and
                    # the up --key-stall later still (a lost segment, retransmitted).
                    delay, interval = (float(v) / 1000 for v in a.key_repeat.split(","))
                    t = clock()
                    d = a.key_delay / 1000
                    schedule(t + d, f"k,{a.keysym},1")
                    n = 0
                    r = t + delay
                    while r < t + a.key_hold:
                        schedule(r + d, f"k,{a.keysym},2")
                        n += 1
                        r += interval
                    up = t + a.key_hold + d + a.key_stall / 1000
                    schedule(up, f"k,{a.keysym},0")
                    marks.write(f"sent\t{n + 1}\n")
                    # Stay until the up has landed: closing the socket releases every key.
                    await asyncio.sleep(up - clock() + 1.0)
                elif kind == "scroll":
                    start = time.monotonic()
                    for i, d in enumerate(arg):
                        await ws.send(f"sp,0.0,{d:.1f}")
                        scrolls.write(f"{clock():.4f}\t{d:.1f}\n")
                        await asyncio.sleep(max(0, start + (i + 1) / 60 - time.monotonic()))
        for t in tasks:
            t.cancel()
    for f in (msgs, acks, marks, scrolls, keys):
        f.close()


if __name__ == "__main__":
    if sys.argv[1:] == ["--check-header"]:
        check_header()
    else:
        asyncio.run(main())
