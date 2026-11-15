"""A headless voxl viewer behind an emulated link, for measuring rate control.

Speaks voxl's protocol (web/src/protocol.ts): turns video on, answers pings and acks
every frame. It does not decode. Instead it models what a viewer behind a bottleneck sees:

- The link is bench/link.py's, the one shaper.py imposes on a real connection: a rate of
  `--kbps` through an unbounded queue served in order, half the round trip each way, and
  with options a one-sided jitter after the bottleneck (`--jitter`) and periodic stalls
  both ways (`--stall`). Pings and every other message take their turn on it.
- A frame is acknowledged once its last part has arrived (parts = the header's
  "parts in frame"), never at its first.
- Before the ack the viewer holds the frame for its decode and draw: 3 ms, or with
  `--hold` a software decoder's time (2 ms + 0.1 ms per KB + 0 to 8 ms at random). The
  ack carries that hold (`a,<id>,<ms>`), as the page's does: the same number the ack was
  delayed by, on the viewer's own monotonic clock, measured from the moment the frame's
  last part arrived on the emulated link.
- With `--late page` every ack and pong then leaves later still, by a lateness drawn
  (seeded by `--seed`) from the page's own (`link.PAGE_LATE`: median 13.2 ms, p90 18.9,
  p99 41.8, rarely none), in the order the page gave them. With `--cumulative`, acks that
  leave together because one waited for the other leave as one, naming the newest frame:
  the frames before it are covered and never acked by their own id.
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

The `cold` scenario is a first load: the terminal is filled with dense text before the
viewer connects, and everything is measured from the connect, with no input.

While it watches it reads the server's /stats every `STATS_EVERY` (`--stats`), and keeps
this viewer's `carries_bytes` (what its window says the path carries in `LATENESS`). The
run fails (exit status 1) when, from the connect on:
- the window says the path carries less than `START` or than the emulated link does in
  `LATENESS`, whichever is less (not measured against a server whose /stats has no
  `carries_bytes`);
- or a frame waits behind the link's queue more than `3 x LATENESS` (a stall is not a
  wait).

Writes OUT.msgs.tsv (each video message: when received, when it arrived on the link,
frame, key, paint-over, bytes, server pipeline ms, ms it waited behind the queue),
OUT.acks.tsv (frame, when it arrived, when its ack reached the server, the hold, the
lateness in ms, and the frame whose ack covered it), OUT.marks.tsv (the scenario's phases),
OUT.scroll.tsv (scroll input), OUT.keys.tsv (key messages, when each was due and when it
was sent) and OUT.stats.tsv (from the connect: seconds, carries, flying and fastest).

usage: linkviewer.py HOST:PORT SCENARIO OUT [--kbps N] [--rtt MS] [--jitter MS]
       [--stall EVERY_S,LENGTH_MS] [--late page] [--cumulative] [--hold]
       [--seed N] [--fifo PATH] [--page FILE] [--passive SECS] [--stats URL]
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
import urllib.request

sys.dont_write_bytecode = True  # nothing written beside the bench
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import link  # noqa: E402  (beside this file; python3 -I leaves the script's directory off the path)

# Messages, and the fields they mean, that the server's and the web client's header
# readers are checked against too.
HEADER_FIXTURE = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "web", "src", "video-header.json"))
VIDEO = 0x01  # a video message's type byte
HEADER_LEN = 16
# src/window.rs: how late a frame may be behind the path (LATENESS), and what a window
# starts with and must never be talked below on a path that carries it (START, ten packets).
LATENESS = 0.060
START = 14_600
# A window moves at most once a round, and a round is at least LATENESS: two reads a round.
STATS_EVERY = LATENESS / 2


def read_header(m):
    """A video message's header (src/video.rs `Header`), None for any other message."""
    if len(m) <= HEADER_LEN or m[0] != VIDEO:
        return None
    return {
        "key": m[1] & 1 == 1,
        "sharpens": m[1] & 2 == 2,
        "frame": int.from_bytes(m[2:6], "big"),
        "pipe": int.from_bytes(m[6:10], "big"),
        "stripe": m[10],
        "stripes": m[11],
        "parts": m[12],
        "y": int.from_bytes(m[14:16], "big"),
    }


def check_header():
    with open(HEADER_FIXTURE) as f:
        cases = json.load(f)
    for c in cases:
        h = read_header(bytes.fromhex(c["header"] + c["picture"]))
        # Every field the fixture names, and nothing else.
        if h != {field: v for field, v in c.items() if field not in ("header", "picture")}:
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


# What a scenario prepares on the desktop before the viewer connects.
PREPARE = {
    # A terminal filled to its last row with dense text, then left to settle.
    "cold": [
        ("term", "clear; tr -s '\\n\\t' '  ' < {page} | fold -w $(tput cols) | head -n $(( $(tput lines) - 1 ))"),
        ("wait", 3.0),
    ],
}

SCENARIOS = {
    # A first load, measured from the connect: the screen PREPARE left, watched for a minute.
    "cold": [
        ("mark", "watch"),
        ("wait", 60.0),
        ("mark", "end"),
    ],
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


def fetch_stats(url):
    with urllib.request.urlopen(url, timeout=2) as r:
        return json.loads(r.read())


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("hostport")
    ap.add_argument("scenario", choices=sorted(SCENARIOS))
    ap.add_argument("out")
    ap.add_argument("--kbps", type=float, default=0, help="bottleneck rate (0: none)")
    ap.add_argument("--rtt", type=float, default=0, help="round trip (ms)")
    ap.add_argument("--jitter", type=float, default=0, help="one-sided jitter after the bottleneck, at most this many ms")
    ap.add_argument("--stall", help="EVERY_S,LENGTH_MS: the link carries nothing for LENGTH_MS every EVERY_S seconds")
    ap.add_argument("--late", choices=["none", "page"], default="none", help="acks and pongs leave late as the page's do")
    ap.add_argument("--cumulative", action="store_true", help="acks that leave together leave as one, naming the newest")
    ap.add_argument("--hold", action="store_true", help="hold frames as a software decoder would")
    ap.add_argument("--fifo", help="FIFO a terminal on the benchmark display reads commands from")
    ap.add_argument("--page", default="/etc/services", help="file the scenarios print")
    ap.add_argument("--settle", type=float, default=3.0, help="seconds to watch before the scenario")
    ap.add_argument("--passive", type=float, default=0, help="only watch, for this many seconds")
    ap.add_argument("--stats", help="the server's /stats (default: http://HOST:PORT/stats)")
    ap.add_argument("--seed", type=int, help="seed of the holds, the lateness and the jitter, so a run can be repeated")
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
    every, length = (float(v) for v in a.stall.split(",")) if a.stall else (0.0, 0.0)
    stalls = link.Stalls(every, length / 1000)
    down = link.Down(rate, owd, stalls, a.jitter / 1000, rng)
    up = link.Up(owd, stalls, link.page_late if a.late == "page" else None, rng)
    stats_url = a.stats or f"http://{a.hostport}/stats"
    t0 = time.monotonic()
    clock = lambda: time.monotonic() - t0
    sendq = []
    seq = 0
    frames = {}
    got_key = asyncio.Event()
    msgs = open(f"{a.out}.msgs.tsv", "w")
    msgs.write("t_recv\tt_arrive\tframe\tkey\tpaintover\tbytes\tpipe_ms\twait_ms\n")
    marks = open(f"{a.out}.marks.tsv", "w")
    scrolls = open(f"{a.out}.scroll.tsv", "w")
    keys = open(f"{a.out}.keys.tsv", "w")
    keys.write("t_intended\tt_sent\tmsg\n")
    acked = {}  # frame: [arrived, ack at the server, hold ms, late ms, covered by]
    last_ack = None  # (frame, when it left the page, its entry in sendq)
    waits = []  # (from the connect, ms behind the queue) of each video message
    carried = []  # (from the connect, carries, flying, fastest) of this viewer

    def schedule(at, msg):
        nonlocal seq
        seq += 1
        entry = [at, seq, msg]
        heapq.heappush(sendq, entry)
        return entry

    def term(cmd):
        if a.fifo:
            with open(a.fifo, "w") as f:
                f.write(cmd.replace("{page}", a.page) + "\n")

    async def run(steps):
        for kind, arg in steps:
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
                up_at = t + a.key_hold + d + a.key_stall / 1000
                schedule(up_at, f"k,{a.keysym},0")
                marks.write(f"sent\t{n + 1}\n")
                # Stay until the up has landed: closing the socket releases every key.
                await asyncio.sleep(up_at - clock() + 1.0)
            elif kind == "scroll":
                start = time.monotonic()
                for i, d in enumerate(arg):
                    await ws.send(f"sp,0.0,{d:.1f}")
                    scrolls.write(f"{clock():.4f}\t{d:.1f}\n")
                    await asyncio.sleep(max(0, start + (i + 1) / 60 - time.monotonic()))

    import websockets

    await run(PREPARE.get(a.scenario, []))
    try:
        before = {c["id"] for c in (await asyncio.to_thread(fetch_stats, stats_url))["clients"]}
    except OSError:
        before = None  # no /stats to read: nothing to say of the window
    async with websockets.connect(f"ws://{a.hostport}/ws", max_size=None, compression=None) as ws:
        connected = clock()
        marks.write(f"connect\t{connected:.4f}\n")

        async def sender():
            # Sends each scheduled message once its time has come (at most 2 ms late); an
            # ack another one covered (its message gone) is not sent.
            while True:
                if sendq and sendq[0][0] <= clock():
                    at, _, msg = heapq.heappop(sendq)
                    if msg is None:
                        continue
                    await ws.send(msg)
                    if msg.startswith("k,"):
                        keys.write(f"{at:.4f}\t{clock():.4f}\t{msg}\n")
                    continue
                wait = sendq[0][0] - clock() if sendq else 0.002
                await asyncio.sleep(min(max(wait, 0), 0.002))

        async def receiver():
            nonlocal last_ack
            async for m in ws:
                if isinstance(m, str):
                    j = json.loads(m)
                    at, _ = down.send(clock(), len(m))
                    if j.get("t") == "ping":
                        schedule(up.send(at)[1], f"po,{j['s']}")
                    continue
                now = clock()
                at, wait = down.send(now, len(m))
                h = read_header(m)
                if h is None:
                    continue
                waits.append((now - connected, wait * 1000))
                fid, key, parts = h["frame"], h["key"], h["parts"]
                msgs.write(f"{now:.4f}\t{at:.4f}\t{fid}\t{key:d}\t{h['sharpens']:d}\t{len(m)}\t{h['pipe'] / 10:.1f}\t{wait * 1000:.1f}\n")
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
                leaves, ack_at, held_back = up.send(at + hold)
                entry = schedule(ack_at, f"a,{fid},{hold * 1000:.1f}")
                acked[fid] = [at, ack_at, hold * 1000, (leaves - at - hold) * 1000, ""]
                if a.cumulative and held_back and last_ack and last_ack[1] == leaves and last_ack[2][2]:
                    # It left with the ack before it: one ack, naming the newest, covers both.
                    last_ack[2][2] = None
                    acked[last_ack[0]][4] = fid
                last_ack = (fid, leaves, entry)

        async def sampler():
            # This viewer's window: the client /stats lists that was not there before.
            while before is not None:
                t = clock()
                try:
                    st = await asyncio.to_thread(fetch_stats, stats_url)
                except OSError:
                    st = {"clients": []}
                mine = [c for c in st["clients"] if c["id"] not in before]
                if mine and "carries_bytes" in mine[-1]:
                    c = mine[-1]
                    carried.append((t - connected, c["carries_bytes"], c.get("flying_bytes"), c.get("fastest_ms")))
                await asyncio.sleep(max(0, t + STATS_EVERY - clock()))

        tasks = [asyncio.create_task(sender()), asyncio.create_task(receiver()), asyncio.create_task(sampler())]
        await ws.send("v,1")
        await asyncio.wait_for(got_key.wait(), 30)
        if a.passive:
            marks.write(f"watch\t{clock():.4f}\n")
            await asyncio.sleep(a.passive)
            marks.write(f"end\t{clock():.4f}\n")
        elif a.scenario in PREPARE:
            # Measured from the connect: no pointer, no settling.
            await run(SCENARIOS[a.scenario])
        else:
            await ws.send("m,1512,1000")
            await asyncio.sleep(a.settle)
            await run(SCENARIOS[a.scenario])
        for t in tasks:
            t.cancel()
    with open(f"{a.out}.acks.tsv", "w") as f:
        for fid, (arrived, ack_at, hold, late, covered_by) in acked.items():
            f.write(f"{fid}\t{arrived:.4f}\t{ack_at:.4f}\t{hold:.1f}\t{late:.1f}\t{covered_by}\n")
    with open(f"{a.out}.stats.tsv", "w") as f:
        f.write("t\tcarries_bytes\tflying_bytes\tfastest_ms\n")
        for row in carried:
            f.write("\t".join(str(v) for v in row) + "\n")
    for f in (msgs, marks, scrolls, keys):
        f.close()
    return verdict(carried, waits, rate)


def verdict(carried, waits, rate):
    """Prints the run's two bounds and whether it kept them; True when it did."""
    ok = True
    floor = min(START, rate / 8 * LATENESS) if rate else START
    if carried:
        t, low = min(((t, c) for t, c, _, _ in carried), key=lambda tc: tc[1])
        kept = low >= floor
        ok &= kept
        print(f"carries_bytes: lowest {low} B, {t:.1f} s from the connect, over {len(carried)} reads;"
              f" bound {floor:.0f} B (START, or what the link carries in LATENESS if less): {'pass' if kept else 'FAIL'}")
    else:
        print("carries_bytes: not in /stats, not measured")
    if waits:
        t, longest = max(waits, key=lambda tw: tw[1])
        kept = longest <= 3 * LATENESS * 1000
        ok &= kept
        print(f"wait behind the queue: longest {longest:.1f} ms, {t:.1f} s from the connect;"
              f" bound {3 * LATENESS * 1000:.0f} ms (3 x LATENESS): {'pass' if kept else 'FAIL'}")
    return ok


if __name__ == "__main__":
    if sys.argv[1:] == ["--check-header"]:
        check_header()
    else:
        sys.exit(0 if asyncio.run(main()) else 1)
