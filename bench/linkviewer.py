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
  (seeded by `--seed`) from the page's own measured answers (`page-late.txt`, 4,091 of
  them: median 13.2 ms, p90 18.9, p99 41.8, at most 424), in the order the page gave
  them; `--late page-99` draws from all but their top 1 %.
- The ack then takes the other half of the round trip, on the same monotonic clock.

All of it runs on one asyncio loop, so a busy machine only ever makes messages arrive
and acks leave later than the model says, never earlier.

It can also drive the desktop while it watches: trackpad-like scroll gestures (`sp`
commands at 60 Hz) and shell commands written to a FIFO that a terminal on the
benchmark display reads (`--fifo`; a terminal running `while read -r l < FIFO; do eval
"$l"; done`). Scenarios name what to do; `{page}` in their commands is `--page`. A
scenario that prints to the terminal does not start without `--fifo`.

The `hold` scenario holds a key (`--keysym`) for `--key-hold` seconds as a browser does:
a down, a repeat after the first `--key-repeat` delay and then every interval, an up. Key
messages leave `--key-delay` ms late, and the up `--key-stall` ms later still, as when the
segment carrying it is lost and sent again. What lands is counted on the desktop (xev);
OUT.marks.tsv says how many characters were sent (`sent`: the down and its repeats).

The `cold` scenario is a first load: the terminal is filled with dense text before the
viewer connects, and everything is measured from the connect, with no input.

At the end, still connected, it reads the server's /stats once (`--stats`): this viewer's
lowest `carries` since it connected (`lowest_carries_bytes`: what its window said the path
carries in `LATENESS`, at its least), and the window's own `LATENESS`, `START` and `FALL`.
The run fails (exit status 1) when:
- the window went below what it may: `START`, or what the emulated link carries in
  `LATENESS` if less, divided by `FALL` once for each stall begun since the connect (a
  stall is the one thing that may cost the window a fall it cannot win back at rest);
- a frame waited behind the link's queue more than `3 x LATENESS` (a stall is not a wait);
- or it was not measured: no /stats, a /stats without those fields, or this viewer not in it.

Writes OUT.msgs.tsv (each video message: when received, when it arrived on the link,
frame, key, paint-over, bytes, server pipeline ms, ms it waited behind the queue),
OUT.acks.tsv (frame, when it arrived, when its ack reached the server, the hold, the
lateness in ms), OUT.marks.tsv (the scenario's phases),
OUT.scroll.tsv (scroll input), OUT.keys.tsv (key messages, when each was due and when it
was sent) and OUT.stats.json (the /stats it read at the end).

usage: linkviewer.py HOST:PORT SCENARIO OUT [--kbps N] [--rtt MS] [--jitter MS]
       [--stall EVERY_S,LENGTH_MS] [--late page|page-99] [--hold]
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
    link.add_options(ap)
    ap.add_argument("--hold", action="store_true", help="hold frames as a software decoder would")
    ap.add_argument("--fifo", help="FIFO a terminal on the benchmark display reads commands from")
    ap.add_argument("--page", default="/etc/services", help="file the scenarios print")
    ap.add_argument("--settle", type=float, default=3.0, help="seconds to watch before the scenario")
    ap.add_argument("--passive", type=float, default=0, help="only watch, for this many seconds")
    ap.add_argument("--stats", help="the server's /stats (default: http://HOST:PORT/stats)")
    ap.add_argument("--keysym", type=lambda v: int(v, 0), default=0x61, help="key the hold scenario holds")
    ap.add_argument("--key-hold", type=float, default=3.0, help="seconds the key is held")
    # Not X's own schedule (500 ms then 50 ms under xfsettingsd, 660 then 40 bare), so the
    # count of characters says whose repeats landed.
    ap.add_argument("--key-repeat", default="300,33", help="the browser's repeat: delay,interval in ms")
    ap.add_argument("--key-delay", type=float, default=0, help="one-way delay of every key message (ms)")
    ap.add_argument("--key-stall", type=float, default=0, help="extra delay of the key-up alone (ms)")
    a = ap.parse_args()
    # A scenario that writes to the terminal measures nothing it meant to without one: the
    # screen would be whatever was on it.
    if a.fifo is None and any(kind == "term" for kind, _ in PREPARE.get(a.scenario, []) + SCENARIOS[a.scenario]):
        ap.error(f"{a.scenario} prints to a terminal on the benchmark display: give --fifo")
    holds = random.Random(f"{a.seed}:hold")
    path = link.Path(a, link.rate(a.kbps * 1000.0), a.rtt / 2000.0)
    down, up = path.down, path.up
    stats_url = a.stats or f"http://{a.hostport}/stats"
    t0 = time.monotonic()
    clock = lambda: time.monotonic() - t0
    sendq = []
    seq = 0
    frames = {}
    got_key = asyncio.Event()
    msgs = open(f"{a.out}.msgs.tsv", "w")
    msgs.write("t_recv\tt_arrive\tframe\tkey\tpaintover\tbytes\tpipe_ms\twait_ms\n")
    acks = open(f"{a.out}.acks.tsv", "w")
    marks = open(f"{a.out}.marks.tsv", "w")
    scrolls = open(f"{a.out}.scroll.tsv", "w")
    keys = open(f"{a.out}.keys.tsv", "w")
    keys.write("t_intended\tt_sent\tmsg\n")
    waits = []  # (from the connect, ms behind the queue) of each video message

    def schedule(at, msg):
        nonlocal seq
        seq += 1
        heapq.heappush(sendq, (at, seq, msg))

    def term(cmd):
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
    except OSError as e:
        sys.exit(f"no /stats at {stats_url} ({e}): nothing could be measured")
    async with websockets.connect(f"ws://{a.hostport}/ws", max_size=None, compression=None) as ws:
        connected = clock()
        marks.write(f"connect\t{connected:.4f}\n")

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
                hold = 0.002 + f["bytes"] / 1e7 + holds.uniform(0, 0.008) if a.hold else 0.003
                leaves, ack_at = up.send(at + hold)
                acks.write(f"{fid}\t{at:.4f}\t{ack_at:.4f}\t{hold * 1000:.1f}\t{(leaves - at - hold) * 1000:.1f}\n")
                schedule(ack_at, f"a,{fid},{hold * 1000:.1f}")

        async def watch():
            """The scenario, then the /stats read that ends it, while the viewer is still
            connected so /stats still lists it: (when it ended, that read)."""
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
            ended = clock()
            try:
                return ended, await asyncio.to_thread(fetch_stats, stats_url)
            except OSError as e:
                raise NotMeasured(f"no /stats at {stats_url} at the end ({e})") from None

        # One group: the viewer's sending and receiving last as long as the scenario, and
        # whichever of the three ends first ends the run; only the scenario may.
        watching = asyncio.create_task(watch())
        group = [watching, asyncio.create_task(sender()), asyncio.create_task(receiver())]
        done, _ = await asyncio.wait(group, return_when=asyncio.FIRST_COMPLETED)
        for t in group:
            t.cancel()
        first = watching if watching in done else done.pop()
        stopped = clock()
    for f in (msgs, acks, marks, scrolls, keys):
        f.close()
    if first is not watching or first.exception():
        why = first.exception() or "the server closed the connection"
        print(f"the run ended early, {stopped - connected:.1f} s from the connect: {why!r}: FAIL")
        return False
    ended, final = first.result()
    with open(f"{a.out}.stats.json", "w") as f:
        json.dump(final, f)
    return verdict(final, before, waits, path.rate, path.stalls.begun(connected, ended))


class NotMeasured(Exception):
    """What a run needed /stats to say, and it did not."""


def window_of(final, before):
    """What /stats said of this viewer's window at the end: its `LATENESS` in seconds,
    `START`, `FALL` and its lowest `carries` since it connected."""
    mine = [c for c in final["clients"] if c["id"] not in before]
    if len(mine) != 1:
        raise NotMeasured(f"/stats lists {len(mine)} viewers that came after the run's start, not one")
    try:
        return final["lateness_us"] / 1e6, final["start_bytes"], final["fall"], mine[0]["lowest_carries_bytes"]
    except KeyError as e:
        raise NotMeasured(f"/stats has no {e}") from None


def verdict(final, before, waits, rate, stalls):
    """Prints the run's bounds and whether it kept them; True when it did. `final` is the
    /stats read at the end, `stalls` how many stalls began since the viewer connected."""
    try:
        lateness, start, fall, lowest = window_of(final, before)
    except NotMeasured as e:
        print(f"not measured: {e}: FAIL")
        return False
    floor = min(start, rate.carries(lateness)) / fall**stalls
    kept_floor = lowest >= floor
    print(f"carries: lowest {lowest} B since the connect; bound {floor:.0f} B (START {start} B, or what the link"
          f" carries in LATENESS if less, / FALL {fall} for each of {stalls} stalls): {'pass' if kept_floor else 'FAIL'}")
    t, longest = max(waits, key=lambda tw: tw[1])
    bound = 3 * lateness * 1000
    kept_wait = longest <= bound
    print(f"wait behind the queue: longest {longest:.1f} ms, {t:.1f} s from the connect;"
          f" bound {bound:.0f} ms (3 x LATENESS): {'pass' if kept_wait else 'FAIL'}")
    return kept_floor and kept_wait


if __name__ == "__main__":
    if sys.argv[1:] == ["--check-header"]:
        check_header()
    else:
        sys.exit(0 if asyncio.run(main()) else 1)
