"""A key held by an emulated browser behind a link: the down, the browser's own repeats,
and the up, each sent late by --delay ms and the up --stall ms later still (a lost
segment, retransmitted). A repeat is sent as `k,<keysym>,<REPEAT>`: 1 (a down, as
upstream's page sends it) or 2 (a repeat). Prints how many presses it sent: the down and
the repeats.

usage: keyhold.py HOST:PORT KEYSYM REPEAT [--hold S] [--repeat-ms DELAY,INTERVAL] [--delay MS] [--stall MS]
"""

import argparse
import asyncio
import time

import websockets


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("hostport")
    ap.add_argument("keysym", type=lambda v: int(v, 0))
    ap.add_argument("repeat", choices=["1", "2"])
    ap.add_argument("--hold", type=float, default=3.0, help="seconds the key is held")
    ap.add_argument("--repeat-ms", default="300,33", help="the browser's repeat delay and interval (ms)")
    ap.add_argument("--delay", type=float, default=0, help="ms added to every key message")
    ap.add_argument("--stall", type=float, default=0, help="ms added to the key-up alone")
    a = ap.parse_args()
    first, every = (float(v) / 1000 for v in a.repeat_ms.split(","))
    d = a.delay / 1000
    msgs = [(d, f"k,{a.keysym},1")]
    r = first
    while r < a.hold:
        msgs.append((r + d, f"k,{a.keysym},{a.repeat}"))
        r += every
    msgs.append((a.hold + d + a.stall / 1000, f"k,{a.keysym},0"))
    async with websockets.connect(f"ws://{a.hostport}/ws", max_size=None, compression=None) as ws:
        t0 = time.monotonic()
        for at, m in msgs:
            await asyncio.sleep(max(0, t0 + at - time.monotonic()))
            await ws.send(m)
        await asyncio.sleep(1.5)
    print(len(msgs) - 1)


asyncio.run(main())
