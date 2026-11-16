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


# The page's lateness models: none, all of its measured answers, or all but their top 1 %.
LATE = {"none": None, "page": 1.0, "page-99": 0.99}


def lateness(model):
    """The lateness `model` (a key of `LATE`) draws from."""
    return drawn_from(page_late(LATE[model])) if LATE[model] else None


def drawn_from(values):
    """A lateness drawn from `values`, each as likely as any other."""
    return lambda rng: rng.choice(values)


class Stalls:
    """The link carries nothing for `length` seconds every `every` seconds, the first stall
    beginning at `every`. With `every` 0 it never stalls."""

    def __init__(self, every=0.0, length=0.0):
        self.every, self.length = every, length

    def resume(self, t):
        """`t`, or the end of the stall `t` falls in."""
        if not self.every:
            return t
        start = (t // self.every) * self.every
        return start + self.length if start > 0 and t < start + self.length else t

    def carry(self, t, busy):
        """When `busy` seconds of carrying, begun at `t`, end; a stall stops the carrying."""
        t = self.resume(t)
        if not self.every:
            return t + busy
        while True:
            stall = (t // self.every + 1) * self.every
            if t + busy <= stall:
                return t + busy
            busy -= stall - t
            t = stall + self.length

    def begun(self, a, b):
        """How many stalls begin in [a, b)."""
        if not self.every:
            return 0
        first = max(1, -(-a // self.every))  # the first stall at or after a
        return max(0, int(-(-b // self.every) - first))

    def within(self, a, b):
        """Seconds of stall in [a, b)."""
        if not self.every or b <= a:
            return 0.0
        total, k = 0.0, max(1, int(a // self.every))
        while k * self.every < b:
            total += max(0.0, min(b, k * self.every + self.length) - max(a, k * self.every))
            k += 1
        return total


class Down:
    """Server to viewer. `bps` 0: no bottleneck."""

    def __init__(self, bps, owd, stalls, jitter=0.0, rng=None):
        self.bps, self.owd, self.stalls, self.jitter, self.rng = bps, owd, stalls, jitter, rng
        self.free = 0.0  # when the queue is next empty
        self.last = 0.0  # the latest arrival so far

    def send(self, now, nbytes):
        """`(arrival, wait)` of a message handed to the link at `now`: when its last byte
        reaches the viewer, and how long it waited behind what was ahead of it in the
        queue (a stall is not counted as waiting)."""
        start = self.stalls.resume(max(now, self.free))
        wait = start - now - self.stalls.within(now, start)
        self.free = self.stalls.carry(start, nbytes * 8 / self.bps) if self.bps else start
        jitter = self.rng.uniform(0, self.jitter) if self.jitter else 0.0
        self.last = max(self.last, self.free + self.owd + jitter)
        return self.last, wait


class Up:
    """Viewer to server: the page's answers (acks, pongs), in the order it gave them."""

    def __init__(self, owd, stalls, late=None, rng=None):
        self.owd, self.stalls, self.late, self.rng = owd, stalls, late, rng
        self.leaving = 0.0  # when the last answer left the page

    def send(self, now):
        """`(leaves, arrival)` of an answer the page gives at `now`: when it leaves the
        page (at its own lateness, or with the answer before it if that one is later
        still), and when it reaches the server."""
        late = self.late(self.rng) if self.late else 0.0
        self.leaving = max(now + late, self.leaving)
        return self.leaving, self.stalls.resume(self.leaving) + self.owd


# How far the oracle may differ from the model: a message is handed over within one tick
# of the oracle's grid, and served to within one more.
TICK = 1e-4
TOLERANCE = 2 * TICK


def _check(seed=0x5EED):
    """The model against independent accounts of it. Prints what it checked and how often
    each clause was reached; exits on the first miss or on a clause never reached.

    - `Down` against an oracle that serves the queue a tick at a time. It kills: a carry
      that ignores stalls, a wait that counts stall time, a link that never stalls, an
      arrival without half the round trip, a jitter twice as wide, and a jitter that
      reorders. It does not kill an arrival late by half a tick, inside `TOLERANCE`.
    - The lateness against the measured values: every draw is one of them, each decile's
      share is right within four standard deviations, and the top 1 % is absent exactly
      when asked. It kills a draw from an interpolated table and a cut that keeps the top.
      It does not kill a cut of the top 0.1 % (four values).
    - `Up` against the contract of a page that answers in order: no answer leaves before
      its own lateness, none overtakes the one before, each leaves at its own time or with
      the one before, and it crosses at the first moment out of a stall. It kills answers
      that may overtake, a lateness added twice, and an answer held when it needn't be. It
      does not kill a crossing later by less than the nanosecond it allows for rounding.
    """
    rng = random.Random(seed)
    reach = {"waited": 0, "began in a stall": 0, "carried across a stall": 0, "jittered": 0,
             "held by the one ahead": 0, "left at its own time": 0, "left with the one before": 0,
             "crossed after a stall": 0}
    for case in range(300):
        bps = rng.choice([1e6, 5e6, 20e6, 1e9])
        every, length = rng.choice([(0.0, 0.0), (0.5, 0.05), (0.3, 0.12)])
        jitter = rng.choice([0.0, 0.005, 0.02])
        stalls = Stalls(every, length)
        down = Down(bps, rng.uniform(0, 0.1), stalls, jitter, random.Random(case))
        now, sent = 0.0, []
        for _ in range(30):
            now += rng.expovariate(1 / rng.choice([0.001, 0.016, 0.1]))
            nbytes = rng.choice([40, 225, 1460, 9000, 30_000])
            sent.append((now, nbytes, *down.send(now, nbytes)))
        # The oracle: the link a tick at a time. In a tick that is not stalled it serves
        # the queue as a fluid, from where it stood, each message once the one before it is
        # done and once it has been handed over; a stalled tick serves nothing.
        stalled = lambda t: every > 0 and t >= every - 1e-9 and (t + 1e-9) % every < length
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
    # The lateness: each draw is one of the measured values; below each decile of them, the
    # decile's share of the draws, within four standard deviations; the cut model never
    # draws from the top 1 % and the whole one does.
    n = 40_000
    whole, cut = page_late(1.0), page_late(0.99)
    top = set(whole[len(cut):])
    for name, values in (("all", whole), ("without the top 1 %", cut)):
        draw = drawn_from(values)
        draws = sorted(draw(rng) for _ in range(n))
        if not set(draws) <= set(values):
            sys.exit(f"{name}: a lateness that was never measured")
        for d in range(1, 10):
            at = values[len(values) * d // 10]
            share = bisect.bisect_left(draws, at) / n
            want = bisect.bisect_left(values, at) / len(values)
            if abs(share - want) > 4 * (want * (1 - want) / n) ** 0.5:
                sys.exit(f"{name}: {share:.4f} of draws fall below {at * 1000:.1f} ms; the values say {want:.4f}")
        if (values is cut) == bool(top & set(draws)):
            sys.exit(f"{name}: drew from the top 1 % {len(top & set(draws))} times")
    # Answers given in order, each with its own lateness, and a stall on the way up.
    twin = random.Random(seed)
    stalls = Stalls(1.0, 0.25)
    up = Up(0.055, stalls, drawn_from(whole), random.Random(seed))
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
        in_stall = stalls.every <= leaves and leaves % stalls.every < stalls.length
        if crosses < leaves - 1e-9 or held != in_stall:
            sys.exit(f"an answer leaving at {leaves:.6f} crosses at {crosses:.6f}; in a stall: {in_stall}")
        if held:
            reach["crossed after a stall"] += 1
            if abs(crosses % stalls.every - stalls.length) > 1e-9:
                sys.exit(f"an answer held by a stall crosses at {crosses:.6f}, not at its end")
        last = leaves
    if not all(reach.values()):
        sys.exit(f"a clause never reached: {reach}")
    print("link: 300 queues as a tick-by-tick oracle serves them; the page's lateness, all of it and"
          " without its top 1 %, against its measured values; 20,000 answers in order. Reached: "
          + ", ".join(f"{k} {v:,}" for k, v in reach.items()))


if __name__ == "__main__":
    _check()
