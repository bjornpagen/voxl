// The counters the worker keeps and the page reads when asked (`window.voxlStats`). The
// page's meter makes them, in memory it shares with the worker (handed over in `init`),
// and reads them as rates: each reading is a copy of the totals, and the rates are the
// differences between two readings, computed from the two alone.

/** The counters, by their index in the shared array: each a running total, wrapping at
 * 2^32. */
export const Count = {
	/** Updates decoded: their pictures out of the decoder and handed to the page. */
	Frames: 0,
	/** Bytes of updates arrived. */
	Bytes: 1,
} as const;
type Counter = (typeof Count)[keyof typeof Count];

/** The decoder's state, after the counters in the shared array: not totals, but what is
 * so now. Whether an update is inside the decoder (1) or none is (0), and when the one
 * inside arrived: the shared clock's whole ms, modulo 2^32. */
const HOLDS = Object.keys(Count).length;
const SINCE = HOLDS + 1;

/** The times the worker keeps, each as a distribution: how many came in each bucket, so a
 * reading says a median and a tail, which a sum hides. By their place among the
 * distributions in the shared array, after the decoder's state. */
export const Timed = {
	/** An update's arrival to its picture out of the decoder, in µs. */
	Decode: 0,
	/** The server's time for an update, from when its change could first be captured to
	 * its send (the header's `pipe`), in tenths of a ms. */
	Pipe: 1,
} as const;
type Time = (typeof Timed)[keyof typeof Timed];

/** A bucket is a time's leading bits, as the daemon's are (src/histogram.rs, which says
 * why 4): every time under 16 has its own, and above that the times of a bucket share
 * their 5 most significant bits, so a bucket is never wider than a sixteenth of its times.
 * A time here is a whole number under 2^32, so 16 buckets for the times under 16 and 16
 * for each of the 28 octaves above: every such time has one. */
const BITS = 4;
const STEPS = 1 << BITS;
export const BUCKETS = (32 - BITS + 1) << BITS;

/** Where the distributions begin in the shared array, and where `time`'s does. */
const TIMES = SINCE + 1;
export const bucketsOf = (time: Time) => TIMES + time * BUCKETS;

/** The most a time is counted as: one past it is counted as it. */
const MOST = 2 ** 32 - 1;

/** The bucket of `time`, a whole number from 0 to 2^32 - 1. */
function bucket(time: number): number {
	const shift = Math.max(31 - Math.clz32(time | 1) - BITS, 0);
	return (shift << BITS) + (time >>> shift);
}

/** The least time of bucket `index`. */
function least(index: number): number {
	const octave = index >>> BITS;
	const step = index & (STEPS - 1);
	return octave === 0 ? step : (STEPS + step) * 2 ** (octave - 1);
}

/** The worker counts one `time` of `value`, a whole number of its unit: no object is made,
 * and nothing is kept but the count. */
export function took(counts: Uint32Array, time: Time, value: number) {
	const at = bucket(Math.min(Math.max(value, 0), MOST) >>> 0);
	Atomics.add(counts, bucketsOf(time) + at, 1);
}

/** What the decoder holds: nothing, or the update that arrived at `since`, in ms on the
 * clock the worker and the page share (`performance.timeOrigin + performance.now()`, read
 * in whole ms). The page takes an update only when its picture comes out of the decoder, so
 * a decoder that keeps one in reads here as decoding since that update's arrival, for as
 * long as it keeps it, while nothing else arrives: a session waiting on the decoder is never
 * silent. */
export type Decoder =
	| { readonly s: 'idle' }
	| { readonly s: 'decoding'; readonly since: number };

/** The worker says what is inside the decoder now. What it returns says only that each
 * state has its case. */
export function holds(counts: Uint32Array, decoder: Decoder): 'held' {
	switch (decoder.s) {
		case 'idle':
			Atomics.store(counts, HOLDS, 0);
			return 'held';
		case 'decoding':
			Atomics.store(counts, SINCE, decoder.since >>> 0);
			Atomics.store(counts, HOLDS, 1);
			return 'held';
	}
}

/** A distribution as a reading says it, in ms: the median and the time 99 in 100 are
 * within, each the least time of its bucket (under the true one by less than a
 * sixteenth). */
export interface Spread {
	p50: number;
	p99: number;
}

/** The counters' rates and the times' spreads between two readings. */
export interface Rates {
	fps: number;
	kbps: number;
	/** Arrival to out of the decoder, and the server's time from when a change could first
	 * be captured to its send; null when none came. */
	decodeMs: Spread | null;
	pipeMs: Spread | null;
}

/** What `window.voxlStats` reads: the rates since the last read. */
export interface Stats extends Rates {
	/** What the decoder decodes. null while there is none (before a keyframe): `voxlStats`
	 * is read as JSON by the bench tools, and this is JSON's word for it. */
	codec: string | null;
	decoder: Decoder;
}

/** The counters at one time (ms): a copy of their totals, which nothing changes after. */
interface Reading {
	readonly at: number;
	readonly totals: Uint32Array;
}

/** The spread of the `time`s counted from reading `from` to `to`, whose unit is `perMs`
 * to the ms: each bucket moved by the difference of its totals, exact while it moved less
 * than 2^32. Of `n` times in order, the one at place `ceil(n * 99 / 100)` (and `ceil(n /
 * 2)`), counted from 1, is in the first bucket by which that many have come. */
function spread(
	from: Reading,
	to: Reading,
	time: Time,
	perMs: number
): Spread | null {
	const base = bucketsOf(time);
	const moved = (i: number) =>
		(to.totals[base + i] - from.totals[base + i]) >>> 0;
	let n = 0;
	for (let i = 0; i < BUCKETS; i++) n += moved(i);
	if (n === 0) return null;
	const half = Math.ceil(n / 2);
	const tail = Math.ceil((n * 99) / 100);
	let p50 = -1;
	let before = 0;
	for (let i = 0; i < BUCKETS; i++) {
		before += moved(i);
		if (p50 < 0 && before >= half) p50 = least(i) / perMs;
		if (before >= tail) return { p50, p99: least(i) / perMs };
	}
	throw new Error('voxl stats: a spread whose times are in no bucket');
}

/** The rates from reading `from` to the later reading `to`. Each counter moved by the
 * difference of its totals, which is exact while it moved less than 2^32 between them. */
function between(from: Reading, to: Reading): Rates {
	const moved = (c: Counter) => (to.totals[c] - from.totals[c]) >>> 0;
	const seconds = (to.at - from.at) / 1000;
	return {
		fps: moved(Count.Frames) / seconds,
		kbps: (moved(Count.Bytes) * 8) / 1000 / seconds,
		decodeMs: spread(from, to, Timed.Decode, 1000),
		pipeMs: spread(from, to, Timed.Pipe, 10),
	};
}

/** The page's side of the counters: it makes them, shared, for the worker to add to
 * (`counts`, handed over in `init`), and reads them. */
export class Meter {
	readonly counts = new Uint32Array(
		new SharedArrayBuffer(
			(TIMES + Object.keys(Timed).length * BUCKETS) *
				Uint32Array.BYTES_PER_ELEMENT
		)
	);
	#last: Reading;

	/** A meter made at time `at`, which its first read counts from. */
	constructor(at: number) {
		this.#last = { at, totals: this.counts.slice() };
	}

	/** The rates from the last read (or the meter's making) to time `at`. */
	read(at: number): Rates {
		const from = this.#last;
		const to = { at, totals: this.counts.slice() };
		this.#last = to;
		return between(from, to);
	}

	/** What the decoder holds, read at `now` (ms on the shared clock). */
	decoder(now: number): Decoder {
		if (Atomics.load(this.counts, HOLDS) === 0) return { s: 'idle' };
		// The arrival is kept modulo 2^32, and so is its distance back from now: exact
		// while the update has been inside less than 2^32 ms (49 days).
		const at = Math.floor(now);
		const back = ((at >>> 0) - Atomics.load(this.counts, SINCE)) >>> 0;
		return { s: 'decoding', since: at - back };
	}
}
