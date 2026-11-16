"""The link the bench's viewers sit behind, one model for both tools: linkviewer.py emulates
it inside a headless viewer, and shaper.py imposes it on a real TCP connection.

- Down (server to viewer): a rate through an unbounded queue served in order, then half the
  round trip, then a one-sided jitter that keeps the order (nothing arrives before what
  went ahead of it).
- Up (viewer to server): what the page answers leaves it late by a lateness drawn from the
  page's own measured answers (`page-late.txt`), or by none, in the order it was answered,
  then crosses in half the round trip.
- Stalls, both ways: every `every` seconds the link carries nothing for `length`.

Times are seconds on the caller's clock, always passed in. Run as a script, this file
checks the model against an oracle that serves the queue a tick at a time (seeded; no
clock is read): `python3 -I bench/link.py`.
"""

import bisect
import math
import os
import random
import sys

# A page's measured lateness: `page-late.txt`, beside this file.
PAGE_LATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "page-late.txt")


def page_late(share):
    """The page's measured lateness in seconds, smallest first: its lowest `share` of it
    (1: all of it)."""
    with open(PAGE_LATE) as f:
        values = [int(line) / 1e6 for line in f if not line.startswith("#")]
    return values[: round(len(values) * share)]


class OnTime:
    """No lateness: an answer leaves as soon as it is given."""

    def draw(self):
        return 0.0


class Measured:
    """A lateness drawn from `values` (seconds), each as likely as any other."""

    def __init__(self, values, rng):
        self.values, self.rng = values, rng

    def draw(self):
        return self.rng.choice(self.values)


# The page's lateness models, by name: none, all of its measured answers, or all but their
# top 1 %; each made from the random stream it draws from.
LATE = {
    "none": lambda rng: OnTime(),
    "page": lambda rng: Measured(page_late(1.0), rng),
    "page-99": lambda rng: Measured(page_late(0.99), rng),
}


class Jitter:
    """A one-sided delay of up to `most` seconds, uniform."""

    def __init__(self, most, rng):
        self.most, self.rng = most, rng

    def draw(self):
        return self.rng.uniform(0, self.most)


class Unlimited:
    """A link with no bottleneck."""

    def busy(self, nbytes):
        return 0.0

    def carries(self, seconds):
        return math.inf


class Rate:
    """A bottleneck of `bps` bits a second."""

    def __init__(self, bps):
        self.bps = bps

    def busy(self, nbytes):
        """Seconds it takes to carry `nbytes`."""
        return nbytes * 8 / self.bps

    def carries(self, seconds):
        """Bytes it carries in `seconds`."""
        return self.bps / 8 * seconds


def rate(bps):
    """A link of `bps` bits a second, as the tools are told it (0: no bottleneck)."""
    return Rate(bps) if bps > 0 else Unlimited()


class Never:
    """A link that never stalls."""

    def resume(self, t):
        return t

    def carry(self, t, busy):
        return t + busy

    def begun(self, a, b):
        return 0

    def within(self, a, b):
        return 0.0


class Every:
    """The link carries nothing for `length` seconds every `every` seconds, the first stall
    beginning at `every`."""

    def __init__(self, every, length):
        self.every, self.length = every, length

    def resume(self, t):
        """`t`, or the end of the stall `t` falls in."""
        start = (t // self.every) * self.every
        return start + self.length if start > 0 and t < start + self.length else t

    def carry(self, t, busy):
        """When `busy` seconds of carrying, begun at `t`, end; a stall stops the carrying."""
        t = self.resume(t)
        while True:
            stall = (t // self.every + 1) * self.every
            if t + busy <= stall:
                return t + busy
            busy -= stall - t
            t = stall + self.length

    def begun(self, a, b):
        """How many stalls begin in [a, b)."""
        first = max(1, -(-a // self.every))  # the first stall at or after a
        return max(0, int(-(-b // self.every) - first))

    def within(self, a, b):
        """Seconds of stall in [a, b)."""
        total, k = 0.0, max(1, int(a // self.every))
        while k * self.every < b:
            total += max(0.0, min(b, k * self.every + self.length) - max(a, k * self.every))
            k += 1
        return total


class Down:
    """Server to viewer: `rate` (`Rate` or `Unlimited`), half the round trip `owd`, the
    link's `stalls` (`Every` or `Never`), and a `Jitter` after the bottleneck."""

    def __init__(self, rate, owd, stalls, jitter):
        self.rate, self.owd, self.stalls, self.jitter = rate, owd, stalls, jitter
        self.free = 0.0  # when the queue is next empty
        self.last = 0.0  # the latest arrival so far

    def send(self, now, nbytes):
        """`(arrival, wait)` of a message handed to the link at `now`: when its last byte
        reaches the viewer, and how long it waited behind what was ahead of it in the
        queue (a stall is not counted as waiting)."""
        start = self.stalls.resume(max(now, self.free))
        wait = start - now - self.stalls.within(now, start)
        self.free = self.stalls.carry(start, self.rate.busy(nbytes))
        self.last = max(self.last, self.free + self.owd + self.jitter.draw())
        return self.last, wait


class Up:
    """Viewer to server: the page's answers (acks, pongs), in the order it gave them, each
    late by its `late` (`OnTime` or `Measured`)."""

    def __init__(self, owd, stalls, late):
        self.owd, self.stalls, self.late = owd, stalls, late
        self.leaving = 0.0  # when the last answer left the page

    def send(self, now):
        """`(leaves, arrival)` of an answer the page gives at `now`: when it leaves the
        page (at its own lateness, or with the answer before it if that one is later
        still), and when it reaches the server."""
        self.leaving = max(now + self.late.draw(), self.leaving)
        return self.leaving, self.stalls.resume(self.leaving) + self.owd


def stall_schedule(text):
    """`--stall`'s EVERY_S,LENGTH_MS."""
    every, length = (float(v) for v in text.split(","))
    return Every(every, length / 1000)


def add_options(ap):
    """The options both tools take for the link, parsed once, here."""
    ap.add_argument("--jitter", type=float, default=0, help="one-sided jitter after the bottleneck, at most this many ms")
    ap.add_argument("--stall", type=stall_schedule, default=Never(),
                    help="EVERY_S,LENGTH_MS: the link carries nothing for LENGTH_MS every EVERY_S seconds")
    ap.add_argument("--late", choices=sorted(LATE), default="none", help="answers leave late as a page's measured ones did")
    ap.add_argument("--seed", type=int, default=0, help="seed of every kind of draw, each its own stream")


class Path:
    """One connection's link, as `add_options` parsed it: `down`, `up`, the `rate` and
    the `stalls` both directions share; and `holds`, the stream a viewer draws its decode
    holds from. Every stream a run draws from is made here, each under its own key, so
    that no two kinds of draw share numbers."""

    def __init__(self, a, rate, owd):
        self.rate, self.stalls = rate, a.stall
        self.down = Down(rate, owd, a.stall, Jitter(a.jitter / 1000, random.Random(f"{a.seed}:jitter")))
        self.up = Up(owd, a.stall, LATE[a.late](random.Random(f"{a.seed}:late")))
        self.holds = random.Random(f"{a.seed}:hold")


# How far the oracle may differ from the model: a message is handed over within one tick
# of the oracle's grid, and served to within one more.
TICK = 1e-4
TOLERANCE = 2 * TICK


def _stalled(every, length):
    """When a link that stalls for `length` every `every` is stalled, by its definition."""
    return lambda t: t >= every - 1e-9 and (t + 1e-9) % every < length


# The stall schedules the check draws, as the tools are given them, and the oracle's own
# reading of each: seconds between stalls and milliseconds of stall, by the option's words.
SCHEDULES = [([], lambda t: False, None), (["--stall", "0.5,50"], _stalled(0.5, 0.05), (0.5, 0.05)),
             (["--stall", "0.3,120"], _stalled(0.3, 0.12), (0.3, 0.12))]


def _path(argv, bps, owd):
    """The link a tool builds from these command-line words, as linkviewer.py and shaper.py
    build it."""
    import argparse

    ap = argparse.ArgumentParser()
    add_options(ap)
    return Path(ap.parse_args(argv), rate(bps), owd)


def _check(seed=0x5EED):
    """The link the tools build from their options, against independent accounts of it.
    Prints what it checked and how often each clause was reached; exits on the first miss
    or on a clause never reached. Every part goes through `add_options`, `Path` and `rate`,
    as the tools do.

    - `Down` against an oracle that serves the queue a tick at a time, in bits. It kills: a
      carry that ignores stalls, a wait that counts stall time, a link that never stalls,
      an arrival without half the round trip, a jitter twice as wide, a jitter that
      reorders, `--stall`'s length or `--jitter` read as seconds, and `rate` taking bytes
      for bits. It does not kill an arrival late by half a tick, inside `TOLERANCE`.
    - `begun` against a count of the schedule's stalls one by one, over spans that start at
      0 and elsewhere. It kills a stall counted at 0, one too many, and a count that ignores
      where the span began. It does not kill one that miscounts only past 10,000 stalls.
    - The lateness `--late` names, drawn through `Path`: every draw is a measured value,
      each decile's share is right within four standard deviations, and the top 1 % is
      drawn under `page` and never under `page-99`. It kills a draw from an interpolated
      table, `page` without its top 1 % and `page-99` with it. It does not kill a cut of
      the top 0.1 % (four values).
    - One stream for each kind of draw: the same answers, with and without `--jitter`,
      leave at the same times; and the jitter, the lateness and the holds start with
      different numbers. It kills jitter and lateness drawn from one stream, and any two
      kinds seeded under one key. It does not kill a kind seeded under another's key but
      one draw ahead of it.
    - `Up` against the contract of a page that answers in order: no answer leaves before
      its own lateness, none overtakes the one before, each leaves at its own time or with
      the one before, and it crosses at the first moment out of a stall. It kills answers
      that may overtake, a lateness added twice, an answer held when it needn't be, and an
      up path that never stalls. It does not kill a crossing later by less than the
      nanosecond it allows for rounding.
    """
    rng = random.Random(seed)
    reach = {"waited": 0, "began in a stall": 0, "carried across a stall": 0, "jittered": 0,
             "held by the one ahead": 0, "a span from 0": 0, "a span with stalls": 0,
             "left at its own time": 0, "left with the one before": 0, "crossed after a stall": 0}
    for case in range(300):
        bps = rng.choice([1e6, 5e6, 20e6, 1e9])
        # The link's stalls as the tool is told them, and the oracle's own account.
        argv, stalled, _ = rng.choice(SCHEDULES)
        jitter_ms = rng.choice([0, 5, 20])
        down = _path(argv + ["--jitter", str(jitter_ms), "--seed", str(case)], bps, rng.uniform(0, 0.1)).down
        jitter = jitter_ms / 1000
        now, sent = 0.0, []
        for _ in range(30):
            now += rng.expovariate(1 / rng.choice([0.001, 0.016, 0.1]))
            nbytes = rng.choice([40, 225, 1460, 9000, 30_000])
            sent.append((now, nbytes, *down.send(now, nbytes)))
        # The oracle: the link a tick at a time. In a tick that is not stalled it serves
        # the queue as a fluid, from where it stood, each message once the one before it is
        # done and once it has been handed over; a stalled tick serves nothing.
        k, i, left, pos, began, before = 0, 0, None, 0.0, None, 0.0
        while i < len(sent):
            t = k * TICK
            if left is None and sent[i][0] >= t + TICK:  # an idle link: on to the next hand-over
                k = int(sent[i][0] // TICK)
                continue
            if stalled(t):
                if left is not None and began is not None:
                    reach["carried across a stall"] += 1
                k += 1
                continue
            pos = max(pos, t)
            while i < len(sent) and pos < t + TICK:
                at, nbytes, arrival, wait = sent[i]
                if left is None:
                    if at >= t + TICK:
                        break
                    left, began = nbytes * 8, max(pos, at)
                    stall_time = sum(TICK for q in range(int(at // TICK), int(began // TICK)) if stalled(q * TICK))
                    reach["began in a stall"] += stall_time > 0
                    reach["waited"] += began - at - stall_time > TOLERANCE
                    pos = began
                served = min(left, (t + TICK - pos) * bps)
                left -= served
                pos += served / bps
                if left > 1e-6:
                    break
                # The jitter only delays, by at most `jitter`, and never reorders.
                want = pos + down.owd
                if not (want - TOLERANCE <= arrival <= max(want + jitter, before) + TOLERANCE) or arrival < before:
                    sys.exit(f"case {case}: message {i} arrives at {arrival:.6f}, the oracle says {want:.6f}"
                             f" to {want + jitter:.6f}, after {before:.6f}")
                reach["jittered"] += arrival > want + TOLERANCE and arrival > before
                reach["held by the one ahead"] += arrival == before
                owed = began - at - stall_time
                if abs(wait - owed) > TOLERANCE:
                    sys.exit(f"case {case}: message {i} waits {wait:.6f}, the oracle says {owed:.6f}")
                before = arrival
                i, left = i + 1, None
            k += 1
    # Stalls begun in a span, against the schedule's stalls counted one by one.
    for argv, _, schedule in SCHEDULES[1:]:
        stalls = _path(argv, 1e6, 0.0).stalls
        every, length = schedule
        for _ in range(5_000):
            a = rng.choice([0.0, rng.uniform(0, 30)])
            b = a + rng.uniform(0, 10)
            want = sum(1 for k in range(1, int(b / every) + 2) if a <= k * every < b)
            if stalls.begun(a, b) != want:
                sys.exit(f"--stall {argv[1]}: {stalls.begun(a, b)} stalls begin in [{a:.4f}, {b:.4f}), not {want}")
            reach["a span from 0"] += a == 0.0
            reach["a span with stalls"] += want > 0
    # The lateness each model draws, through the path the tools build: each draw one of the
    # measured values; below each decile of them, the decile's share of the draws, within
    # four standard deviations; the top 1 % drawn under `page` and never under `page-99`.
    n = 40_000
    whole = page_late(1.0)
    top = set(whole[round(len(whole) * 0.99):])
    for model in ("page", "page-99"):
        late = _path(["--late", model, "--seed", str(seed)], 1e6, 0.0).up.late
        draws = sorted(late.draw() for _ in range(n))
        values = [v for v in whole if model == "page" or v not in top]
        if not set(draws) <= set(values):
            sys.exit(f"--late {model}: a lateness that was never measured, or one from the top 1 %")
        for d in range(1, 10):
            at = values[len(values) * d // 10]
            share = bisect.bisect_left(draws, at) / n
            want = bisect.bisect_left(values, at) / len(values)
            if abs(share - want) > 4 * (want * (1 - want) / n) ** 0.5:
                sys.exit(f"--late {model}: {share:.4f} of draws fall below {at * 1000:.1f} ms; the values say {want:.4f}")
        if (model == "page") != bool(top & set(draws)):
            sys.exit(f"--late {model}: drew from the top 1 % {len(top & set(draws))} times")
    # One stream for each kind of draw: with and without a jitter, the same answers leave
    # at the same times.
    quiet = _path(["--late", "page", "--seed", str(seed)], 20e6, 0.055)
    jittery = _path(["--late", "page", "--seed", str(seed), "--jitter", "20"], 20e6, 0.055)
    now = 0.0
    for _ in range(2_000):
        now += rng.expovariate(1 / 0.01)
        quiet.down.send(now, 9000)
        jittery.down.send(now, 9000)
        if quiet.up.send(now) != jittery.up.send(now):
            sys.exit("an answer leaves at another time when the link jitters: one stream for two kinds of draw")
    # No two kinds of draw are seeded alike: each stream's first numbers differ from every
    # other's (drawn from copies, so the path's own are untouched).
    path = _path(["--late", "page", "--jitter", "5", "--seed", str(seed)], 1e6, 0.0)
    firsts = []
    for stream in (path.down.jitter.rng, path.up.late.rng, path.holds):
        twin_stream = random.Random()
        twin_stream.setstate(stream.getstate())
        firsts.append(tuple(twin_stream.getrandbits(64) for _ in range(4)))
    if len(set(firsts)) != len(firsts):
        sys.exit("two kinds of draw are seeded alike: their streams start with the same numbers")
    # Answers given in order, each with its own lateness, through a path that stalls. The
    # oracle draws the same lateness from the measured values in its own stream, seeded as
    # the tools seed theirs.
    twin = random.Random(f"{seed}:late")
    every, length = 1.0, 0.25
    up = _path(["--late", "page", "--seed", str(seed), "--stall", "1,250"], 1e6, 0.055).up
    now, last = 0.0, 0.0
    for _ in range(20_000):
        now += rng.expovariate(1 / 0.01)
        own = now + twin.choice(whole)
        leaves, arrival = up.send(now)
        if leaves < own or leaves < last:
            sys.exit(f"an answer due at {own:.6f} leaves at {leaves:.6f}, after one at {last:.6f}")
        if leaves == own:
            reach["left at its own time"] += 1
        elif leaves == last:
            reach["left with the one before"] += 1
        else:
            sys.exit(f"an answer due at {own:.6f} leaves at {leaves:.6f}: neither its time nor {last:.6f}")
        # When it starts across, to within rounding.
        crosses = arrival - up.owd
        held = crosses - leaves > 1e-9
        in_stall = every <= leaves and leaves % every < length
        if crosses < leaves - 1e-9 or held != in_stall:
            sys.exit(f"an answer leaving at {leaves:.6f} crosses at {crosses:.6f}; in a stall: {in_stall}")
        if held:
            reach["crossed after a stall"] += 1
            if abs(crosses % every - length) > 1e-9:
                sys.exit(f"an answer held by a stall crosses at {crosses:.6f}, not at its end")
        last = leaves
    if not all(reach.values()):
        sys.exit(f"a clause never reached: {reach}")
    print("link, as the tools build it from their options: 300 queues as a tick-by-tick oracle serves"
          " them; stalls begun, counted one by one; the lateness of page and page-99 against the"
          " measured values; one stream a kind of draw; 20,000 answers in order. Reached: "
          + ", ".join(f"{k} {v:,}" for k, v in reach.items()))


if __name__ == "__main__":
    _check()
