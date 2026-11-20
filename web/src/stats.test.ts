// Checks the page's meter (stats.ts) against a fake worker, over many seeded runs:
//   node web/src/stats.test.ts
// The meter is made at a drawn time and its counters handed to the fake worker as the page
// hands them in `init` (a structured clone, which shares the memory). Between reads, at
// drawn times, the worker counts what it did as the real one does (Atomics.add, `took`):
// updates arrived (each with its bytes and the server's time for it), updates decoded (each
// with its time from arrival to out of the decoder). The first interval adds amounts up to
// 2^32 - 1, to the counters and to buckets of the times, so later intervals wrap the totals.
//
// The worker also says when it gives the decoder an update (the update's arrival, in
// fractional ms on the shared clock, which crosses multiples of 2^32 ms) and when that
// update is out of it.
//
// What it checks are the fake's own facts: each read reports what the worker did since the
// read before it (or the meter's making), and nothing else: the counts it made, over the
// time it took, and the spread of what it timed, none where it timed nothing: the median
// and the 99th percentile are those of the times themselves, each cut to its five leading
// bits (a cut written here apart from the meter's buckets); and the decoder reads as idle,
// or as decoding since the whole ms the update inside it arrived.
import {
	BUCKETS,
	bucketsOf,
	Count,
	holds,
	Meter,
	Timed,
	took,
	type Decoder,
	type Rates,
	type Spread,
} from './stats.ts';

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

/** Times counted, as the oracle keeps them: each cut to its leading bits, with how many
 * came so. */
type Times = Map<number, number>;

/** What the fake worker did in an interval, as plain numbers. */
interface Did {
	shown: number;
	bytes: number;
	decodeUs: Times;
	pipeTenths: Times;
}

/** `time` with only its five leading bits kept: halved until it is under 32, and doubled
 * back. */
function leading(time: number): number {
	let v = time;
	let halved = 0;
	while (v >= 32) {
		v = Math.floor(v / 2);
		halved++;
	}
	return v * 2 ** halved;
}

/** Every time under 2^32 that is its own leading bits, in order: the 16 under 16, and 16
 * to 31 doubled up to 27 times. A time's place in it is its bucket. */
const CUTS: number[] = [];
for (let v = 0; v < 16; v++) CUTS.push(v);
for (let doubled = 0; doubled < 28; doubled++)
	for (let v = 16; v < 32; v++) CUTS.push(v * 2 ** doubled);
if (CUTS.length !== BUCKETS)
	throw new Error(`${CUTS.length} cuts, ${BUCKETS} buckets`);

const count = (times: Times, cut: number, n: number) =>
	times.set(cut, (times.get(cut) ?? 0) + n);

/** The spread of `times`, whose unit is `perMs` to the ms: of the `n` in order, those at
 * places `ceil(n / 2)` and `ceil(n * 99 / 100)`, counted from 1. */
function spreadOf(times: Times, perMs: number): Spread | null {
	let n = 0;
	for (const c of times.values()) n += c;
	if (n === 0) return null;
	const placed = (place: number) => {
		let before = 0;
		for (const cut of CUTS) {
			before += times.get(cut) ?? 0;
			if (before >= place) return cut / perMs;
		}
		throw new Error('a place past the times');
	};
	return {
		p50: placed(Math.ceil(n / 2)),
		p99: placed(Math.ceil((n * 99) / 100)),
	};
}

/** Clauses some run must reach. */
const reached = {
	idle: 0,
	shownAndArrived: 0,
	arrivedNotShown: 0,
	wrapped: 0,
	bucketWrapped: 0,
	tailApart: 0,
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
	/** The buckets the first interval filled, by their place in the array: how many each
	 * holds, unbounded. */
	const seeded = new Map<number, number>();
	const counted = (time: (typeof Timed)[keyof typeof Timed], value: number) => {
		const place = bucketsOf(time) + CUTS.indexOf(leading(value));
		const held = seeded.get(place);
		if (held === undefined) return;
		if (held < 2 ** 32 && held + 1 >= 2 ** 32) reached.bucketWrapped++;
		seeded.set(place, held + 1);
	};
	for (let r = 0; r < READS; r++) {
		const did: Did = {
			shown: 0,
			bytes: 0,
			decodeUs: new Map(),
			pipeTenths: new Map(),
		};
		if (r === 0) {
			did.shown = draw(2 ** 32);
			did.bytes = draw(2 ** 32);
			Atomics.add(worker, Count.Frames, did.shown);
			Atomics.add(worker, Count.Bytes, did.bytes);
			// A few buckets of each time, each nearly full or anywhere: a later time in
			// one wraps it.
			for (const [time, times] of [
				[Timed.Decode, did.decodeUs],
				[Timed.Pipe, did.pipeTenths],
			] as const)
				for (let k = draw(4); k > 0; k--) {
					const b = draw(2) ? draw(BUCKETS) : 100 + draw(120);
					const n = draw(2) ? 2 ** 32 - 1 - draw(3) : draw(2 ** 32);
					if (times.has(CUTS[b])) continue;
					Atomics.add(worker, bucketsOf(time) + b, n);
					count(times, CUTS[b], n);
					seeded.set(bucketsOf(time) + b, n);
				}
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
							// Most a few ms, some a stall's seconds.
							const pipe = draw(20) ? draw(2000) : draw(2 ** 32);
							Atomics.add(worker, Count.Bytes, bytes);
							took(worker, Timed.Pipe, pipe);
							counted(Timed.Pipe, pipe);
							did.bytes += bytes;
							count(did.pipeTenths, leading(pipe), 1);
							return 'done';
						}
						case 'shown': {
							const us = draw(20) ? draw(20_000) : draw(2_000_000);
							Atomics.add(worker, Count.Frames, 1);
							took(worker, Timed.Decode, us);
							counted(Timed.Decode, us);
							did.shown++;
							count(did.decodeUs, leading(us), 1);
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
		if (!did.decodeUs.size && !did.pipeTenths.size) reached.idle++;
		else if (did.decodeUs.size && did.pipeTenths.size)
			reached.shownAndArrived++;
		else if (did.pipeTenths.size) reached.arrivedNotShown++;

		const from = at;
		at += 1 + draw(5000) + draw(1000) / 1000;
		const seconds = (at - from) / 1000;
		const expected: Rates = {
			fps: did.shown / seconds,
			kbps: (did.bytes * 8) / 1000 / seconds,
			decodeMs: spreadOf(did.decodeUs, 1000),
			pipeMs: spreadOf(did.pipeTenths, 10),
		};
		if (expected.pipeMs && expected.pipeMs.p99 > 4 * expected.pipeMs.p50)
			reached.tailApart++;
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
