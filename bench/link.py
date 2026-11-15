"""The link the bench's viewers sit behind, one model for both tools: linkviewer.py emulates
it inside a headless viewer, and shaper.py imposes it on a real TCP connection.

- Down (server to viewer): a rate through an unbounded queue served in order, then half the
  round trip, then a one-sided jitter that keeps the order (nothing arrives before what
  went ahead of it).
- Up (viewer to server): what the page answers leaves it late by the page's own lateness
  (`PAGE_LATE`, or none), in the order it was answered, then crosses in half the round trip.
- Stalls, both ways: every `every` seconds the link carries nothing for `length`.

Times are seconds on the caller's clock, always passed in. Run as a script, this file
checks the model against an oracle that serves the queue a tick at a time (seeded; no
clock is read): `python3 -I bench/link.py`.
"""

import bisect
import random
import sys

# How late the page that fell to the window's floor answered, past the path's fastest trip
# and past the hold it said it kept the frame: ms, quantiles of 4,091 of its acks at rest
# on a 20 Mbit/s, 110 ms path (its /stats/trace). Between two knots, a straight line. The
# least is 0: a page that answers at once is rare, not impossible.
PAGE_LATE = (
    (0.0, 0.0),
    (0.05, 9.106),
    (0.10, 10.235),
    (0.15, 10.788),
    (0.20, 11.128),
    (0.25, 11.478),
    (0.30, 11.778),
    (0.35, 12.131),
    (0.40, 12.434),
    (0.45, 12.824),
    (0.50, 13.218),
    (0.55, 13.629),
    (0.60, 14.052),
    (0.65, 14.542),
    (0.70, 15.048),
    (0.75, 15.647),
    (0.80, 16.567),
    (0.85, 17.432),
    (0.90, 18.947),
    (0.95, 22.749),
    (0.99, 41.789),
    (0.999, 327.014),
    (1.0, 423.745),
)
_PROBS = [p for p, _ in PAGE_LATE]


def page_late(rng):
    """One answer's lateness, in seconds, drawn from `PAGE_LATE`."""
    u = rng.random()
    i = bisect.bisect_right(_PROBS, u)
    (p0, l0), (p1, l1) = PAGE_LATE[i - 1], PAGE_LATE[i]
    return (l0 + (l1 - l0) * (u - p0) / (p1 - p0)) / 1000


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
        """`(leaves, arrival, held_back)` of an answer the page gives at `now`: when it
        leaves the page, when it reaches the server, and whether it left with the answer
        before it (its own lateness ended first, and it waited for that one)."""
        late = self.late(self.rng) if self.late else 0.0
        held_back = now + late < self.leaving
        self.leaving = max(now + late, self.leaving)
        return self.leaving, self.stalls.resume(self.leaving) + self.owd, held_back


def _check(seed=0x5EED):
    """The model against an oracle that serves the queue a tick at a time, and the lateness
    against the table it is drawn from. Prints what it checked; exits on the first miss."""
    rng = random.Random(seed)
    tick = 1e-4
    for case in range(300):
        bps = rng.choice([1e6, 5e6, 20e6, 1e9])
        every, length = rng.choice([(0.0, 0.0), (0.5, 0.05), (0.3, 0.12)])
        jitter = rng.choice([0.0, 0.005, 0.02])
        down = Down(bps, rng.uniform(0, 0.1), Stalls(every, length), jitter, random.Random(case))
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
            t = k * tick
            if left is None and sent[i][0] >= t + tick:  # an idle link: on to the next hand-over
                k = int(sent[i][0] // tick)
                continue
            if stalled(t):
                k += 1
                continue
            pos = max(pos, t)
            while i < len(sent) and pos < t + tick:
                at, nbytes, arrival, wait = sent[i]
                if left is None:
                    if at >= t + tick:
                        break
                    left, began = nbytes * 8, max(pos, at)
                    stall_time = sum(tick for q in range(int(at // tick), int(began // tick)) if stalled(q * tick))
                    pos = began
                served = min(left, (t + tick - pos) * bps)
                left -= served
                pos += served / bps
                if left > 1e-6:
                    break
                # The jitter only delays, by at most `jitter`, and never reorders.
                want = pos + down.owd
                if not (want - 2 * tick <= arrival <= max(want + jitter, before) + 2 * tick) or arrival < before:
                    sys.exit(f"case {case}: message {i} arrives at {arrival:.6f}, the oracle says {want:.6f}"
                             f" to {want + jitter:.6f}, after {before:.6f}")
                before = arrival
                owed = began - at - stall_time
                if abs(wait - owed) > 2 * tick:
                    sys.exit(f"case {case}: message {i} waits {wait:.6f}, the oracle says {owed:.6f}")
                i, left = i + 1, None
            k += 1
    # The page's lateness: below each knot's lateness, the knot's share of draws, within
    # four standard deviations; never below 0.
    n = 40_000
    draws = sorted(page_late(rng) for _ in range(n))
    if draws[0] < 0:
        sys.exit("a lateness below zero")
    for p, ms in PAGE_LATE[1:-1]:
        share = bisect.bisect_left(draws, ms / 1000) / n
        if abs(share - p) > 4 * (p * (1 - p) / n) ** 0.5 + 1 / n:
            sys.exit(f"{share:.4f} of draws fall below {ms} ms, the table says {p}")
    # Answers: each leaves at its own lateness or with the one before it, whichever is
    # later; `held_back` says which. The oracle draws the same lateness from a twin stream.
    twin = random.Random(seed)
    up = Up(0.055, Stalls(1.0, 0.25), page_late, random.Random(seed))
    now, before, held = 0.0, 0.0, 0
    for _ in range(20_000):
        now += rng.expovariate(1 / 0.01)
        own = now + page_late(twin)
        leaves, arrival, held_back = up.send(now)
        if leaves != max(own, before) or held_back != (own < before):
            sys.exit(f"an answer due at {own:.6f} after one at {before:.6f} leaves at {leaves:.6f}, held back {held_back}")
        if arrival < leaves + up.owd:
            sys.exit(f"an answer leaving at {leaves:.6f} arrives at {arrival:.6f}")
        before, held = leaves, held + held_back
    if not 0 < held < 20_000:
        sys.exit(f"{held} of 20,000 answers held back: the check never saw both sides")
    print(f"link: 300 queues as a tick-by-tick oracle serves them; PAGE_LATE's {len(PAGE_LATE) - 2} inner"
          f" quantiles over {n:,} draws; 20,000 answers in order, {held:,} of them held back")


if __name__ == "__main__":
    _check()
