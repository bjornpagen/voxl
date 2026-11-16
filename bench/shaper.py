"""TCP proxy emulating a bottleneck link (bench/link.py): a rate (Mbit/s) on server->client
through an unbounded queue (like a bufferbloated path), half the round trip each way, and
with options a one-sided jitter after the bottleneck, a page's measured lateness on everything
the viewer sends (its acks and pongs) and periodic stalls both ways.
Usage: shaper.py <listen_port> <target_host:port> <mbps> <rtt_ms>
       [--jitter MS] [--late page|page-99] [--stall EVERY_S,LENGTH_MS] [--seed N]"""
import argparse, asyncio, os, random, sys, time

sys.dont_write_bytecode = True  # nothing written beside the bench
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import link  # noqa: E402  (beside this file; python3 -I leaves the script's directory off the path)

ap = argparse.ArgumentParser()
ap.add_argument("listen", type=int)
ap.add_argument("target")
ap.add_argument("mbps", type=float)
ap.add_argument("rtt", type=float, help="round trip (ms)")
ap.add_argument("--jitter", type=float, default=0, help="one-sided jitter after the bottleneck, at most this many ms")
ap.add_argument("--late", choices=sorted(link.LATE), default="none", help="what the viewer sends leaves late as a page's measured answers did")
ap.add_argument("--stall", help="EVERY_S,LENGTH_MS: the link carries nothing for LENGTH_MS every EVERY_S seconds")
ap.add_argument("--seed", type=int, default=0, help="seed of the jitter and the lateness, each its own stream")
a = ap.parse_args()
listen = a.listen
th, tp = a.target.split(':')
every, length = (float(v) for v in a.stall.split(",")) if a.stall else (0.0, 0.0)
stalls = link.Stalls(every, length / 1000)
t0 = time.monotonic()

async def pipe(reader, writer, crossing):
    """Copies `reader` to `writer`, each chunk delivered when `crossing(now, bytes)` says."""
    q = asyncio.Queue()
    async def sender():
        while True:
            t, data = await q.get()
            if data is None:
                break
            dt = t - (time.monotonic() - t0)
            if dt > 0:
                await asyncio.sleep(dt)
            writer.write(data)
            await writer.drain()
        writer.close()
    task = asyncio.create_task(sender())
    while True:
        data = await reader.read(65536)
        if not data:
            await q.put((0, None)); break
        await q.put((crossing(time.monotonic() - t0, len(data)), data))
    await task

async def handle(cr, cw):
    sr, sw = await asyncio.open_connection(th, int(tp))
    owd = a.rtt / 2000
    down = link.Down(a.mbps * 1e6, owd, stalls, a.jitter / 1000, random.Random(f"{a.seed}:jitter"))
    up = link.Up(owd, stalls, link.lateness(a.late), random.Random(f"{a.seed}:late"))
    await asyncio.gather(pipe(cr, sw, lambda now, n: up.send(now)[1]),
                         pipe(sr, cw, lambda now, n: down.send(now, n)[0]), return_exceptions=True)

async def main():
    srv = await asyncio.start_server(handle, '127.0.0.1', listen)
    async with srv:
        await srv.serve_forever()
asyncio.run(main())
