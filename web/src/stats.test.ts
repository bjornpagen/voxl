// Checks the page's meter (stats.ts) against a fake worker, over many seeded runs:
//   node web/src/stats.test.ts
// The meter is made at a drawn time and its counters handed to the fake worker as the page
// hands them in `init` (a structured clone, which shares the memory). Between reads, at
// drawn times, the worker counts what it did as the real one does (Atomics.add): updates
// arrived (each with its bytes and the server's capture-to-send time), updates shown (each
// with its arrival-to-shown time). The first interval adds amounts up to
// 2^32 - 1, so later intervals wrap the totals.
//
// What it checks are the fake's own facts: each read reports what the worker did since the
// read before it (or the meter's making), and nothing else: the counts it made, over the
// time it took, and the means of what it timed, none where it timed nothing.
import { Count, Meter, type Rates } from './stats.ts';

const RUNS = 20_000;
/** Reads in a run. */
const READS = 12;

/** A small seeded generator (mulberry32): runs repeat from their seed alone. */
function random(seed: number) {
	let a = seed >>> 0;
	return (n: number) => {
		a = (a + 0x6d2b79f5) >>> 0;
		let t = a;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) % n;
	};
}

/** What the fake worker did in an interval, summed as plain numbers. */
interface Did {
	shown: number;
	arrived: number;
	bytes: number;
	decodeUs: number;
	pipeTenths: number;
}

/** Clauses some run must reach. */
const reached = {
	idle: 0,
	shownAndArrived: 0,
	arrivedNotShown: 0,
	wrapped: 0,
};

function run(seed: number) {
	const draw = random(seed);
	const fail = (why: string) => {
		throw new Error(`seed ${seed}: ${why}`);
	};
	let at = draw(1_000_000) + draw(1000) / 1000;
	const meter = new Meter(at);
	const worker = structuredClone(meter.counts);
	/** The totals as the oracle keeps them, unbounded. */
	let bytesTotal = 0;
	for (let r = 0; r < READS; r++) {
		const did: Did = {
			shown: 0,
			arrived: 0,
			bytes: 0,
			decodeUs: 0,
			pipeTenths: 0,
		};
		if (r === 0) {
			did.shown = draw(2 ** 32);
			did.arrived = draw(2 ** 32);
			did.bytes = draw(2 ** 32);
			did.decodeUs = draw(2 ** 32);
			did.pipeTenths = draw(2 ** 32);
			Atomics.add(worker, Count.Frames, did.shown);
			Atomics.add(worker, Count.Updates, did.arrived);
			Atomics.add(worker, Count.Bytes, did.bytes);
			Atomics.add(worker, Count.DecodeUs, did.decodeUs);
			Atomics.add(worker, Count.PipeTenths, did.pipeTenths);
		} else {
			// Events one at a time, interleaved as the worker meets them.
			const events = draw(4) === 0 ? 0 : draw(300);
			for (let e = 0; e < events; e++) {
				switch (draw(2)) {
					case 0: {
						const bytes = 15 + draw(400_000);
						const pipe = draw(2000);
						Atomics.add(worker, Count.Bytes, bytes);
						Atomics.add(worker, Count.Updates, 1);
						Atomics.add(worker, Count.PipeTenths, pipe);
						did.arrived++;
						did.bytes += bytes;
						did.pipeTenths += pipe;
						break;
					}
					case 1: {
						const us = draw(200_000);
						Atomics.add(worker, Count.Frames, 1);
						Atomics.add(worker, Count.DecodeUs, us);
						did.shown++;
						did.decodeUs += us;
						break;
					}
				}
			}
		}
		const before = bytesTotal;
		bytesTotal += did.bytes;
		if (
			r > 0 &&
			Math.floor(before / 2 ** 32) !== Math.floor(bytesTotal / 2 ** 32)
		)
			reached.wrapped++;
		if (!did.shown && !did.arrived) reached.idle++;
		else if (did.shown && did.arrived) reached.shownAndArrived++;
		else if (did.arrived) reached.arrivedNotShown++;

		const from = at;
		at += 1 + draw(5000) + draw(1000) / 1000;
		const seconds = (at - from) / 1000;
		const expected: Rates = {
			fps: did.shown / seconds,
			kbps: (did.bytes * 8) / 1000 / seconds,
			decodeMs: did.shown ? did.decodeUs / 1000 / did.shown : null,
			pipeMs: did.arrived ? did.pipeTenths / 10 / did.arrived : null,
		};
		// Exact: JSON writes each number so that it reads back as the same double.
		const got = JSON.stringify(meter.read(at));
		if (got !== JSON.stringify(expected))
			fail(
				`read ${r}: ${got}, not ${JSON.stringify(expected)} (did ${JSON.stringify(did)} in ${seconds} s)`
			);
	}
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
