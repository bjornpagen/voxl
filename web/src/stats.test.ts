// Checks the page's meter (stats.ts) against a fake worker, over many seeded runs:
//   node web/src/stats.test.ts
// The meter is made at a drawn time and its counters handed to the fake worker as the page
// hands them in `init` (a structured clone, which shares the memory). Between reads, at
// drawn times, the worker counts what it did as the real one does (Atomics.add): updates
// arrived (each with its bytes and the server's capture-to-send time), updates decoded (each
// with its time from arrival to out of the decoder). The first interval adds amounts up to
// 2^32 - 1, so later intervals wrap the totals.
//
// The worker also says when it gives the decoder an update (the update's arrival, in
// fractional ms on the shared clock, which crosses multiples of 2^32 ms) and when that
// update is out of it.
//
// What it checks are the fake's own facts: each read reports what the worker did since the
// read before it (or the meter's making), and nothing else: the counts it made, over the
// time it took, and the means of what it timed, none where it timed nothing; and the
// decoder reads as idle, or as decoding since the whole ms the update inside it arrived.
import { Count, holds, Meter, type Decoder, type Rates } from './stats.ts';

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

/** What the fake worker does, one event at a time. */
const EVENTS = ['arrived', 'shown', 'decoder'] as const;

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
	decoderIdle: 0,
	decoding: 0,
	decodingAcrossAWrap: 0,
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
	/** The wall clock (ms), which often starts just under a multiple of 2^32, and when the
	 * update inside the decoder went in: null while it holds none. */
	let wall =
		(400 + draw(8)) * 2 ** 32 - (draw(2) ? draw(20_000) : draw(2 ** 32));
	let inside: number | null = null;
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
				wall += draw(50);
				const event = EVENTS[draw(EVENTS.length)];
				// What it returns says only that each event has its case.
				((): 'done' => {
					switch (event) {
						case 'arrived': {
							const bytes = 15 + draw(400_000);
							const pipe = draw(2000);
							Atomics.add(worker, Count.Bytes, bytes);
							Atomics.add(worker, Count.Updates, 1);
							Atomics.add(worker, Count.PipeTenths, pipe);
							did.arrived++;
							did.bytes += bytes;
							did.pipeTenths += pipe;
							return 'done';
						}
						case 'shown': {
							const us = draw(200_000);
							Atomics.add(worker, Count.Frames, 1);
							Atomics.add(worker, Count.DecodeUs, us);
							did.shown++;
							did.decodeUs += us;
							return 'done';
						}
						// The decoder is given an update, or the one inside it comes out.
						case 'decoder':
							if (inside === null) {
								inside = wall + draw(1000) / 1000;
								holds(worker, { s: 'decoding', since: inside });
							} else {
								holds(worker, { s: 'idle' });
								inside = null;
							}
							return 'done';
					}
				})();
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
		// The decoder's state is what is so at the read, not a rate: idle, or decoding
		// since the update inside arrived.
		wall += 1 + draw(100_000);
		const decoder: Decoder =
			inside === null
				? { s: 'idle' }
				: { s: 'decoding', since: Math.floor(inside) };
		if (inside === null) reached.decoderIdle++;
		else {
			reached.decoding++;
			if (Math.floor(inside / 2 ** 32) !== Math.floor(wall / 2 ** 32))
				reached.decodingAcrossAWrap++;
		}
		const read = JSON.stringify(meter.decoder(wall + draw(1000) / 1000));
		if (read !== JSON.stringify(decoder))
			fail(
				`read ${r}: the decoder reads ${read}, not ${JSON.stringify(decoder)}`
			);
	}
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
