// The counters the worker keeps and the page reads when asked (`window.voxlStats`). The
// page's meter makes them, in memory it shares with the worker (handed over in `init`),
// and reads them as rates: each reading is a copy of the totals, and the rates are the
// differences between two readings, computed from the two alone.

/** The counters, by their index in the shared array: each a running total, wrapping at
 * 2^32. */
export const Count = {
	/** Updates decoded: their pictures out of the decoder and handed to the page. */
	Frames: 0,
	/** Updates arrived. */
	Updates: 1,
	/** Bytes of updates arrived. */
	Bytes: 2,
	/** Arrival to out of the decoder of those updates, in µs. */
	DecodeUs: 3,
	/** The server's capture to send of the updates arrived, in tenths of a ms. */
	PipeTenths: 4,
} as const;
type Counter = (typeof Count)[keyof typeof Count];

/** The decoder's state, after the counters in the shared array: not totals, but what is
 * so now. Whether an update is inside the decoder (1) or none is (0), and when the one
 * inside arrived: the shared clock's whole ms, modulo 2^32. */
const HOLDS = Object.keys(Count).length;
const SINCE = HOLDS + 1;

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

/** The counters' rates and means between two readings. */
export interface Rates {
	fps: number;
	kbps: number;
	/** Mean arrival to out of the decoder, and the server's capture to send; null when
	 * none came. */
	decodeMs: number | null;
	pipeMs: number | null;
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

/** The rates from reading `from` to the later reading `to`. Each counter moved by the
 * difference of its totals, which is exact while it moved less than 2^32 between them. */
function between(from: Reading, to: Reading): Rates {
	const moved = (c: Counter) => (to.totals[c] - from.totals[c]) >>> 0;
	const seconds = (to.at - from.at) / 1000;
	const frames = moved(Count.Frames);
	const updates = moved(Count.Updates);
	return {
		fps: frames / seconds,
		kbps: (moved(Count.Bytes) * 8) / 1000 / seconds,
		decodeMs: frames ? moved(Count.DecodeUs) / 1000 / frames : null,
		pipeMs: updates ? moved(Count.PipeTenths) / 10 / updates : null,
	};
}

/** The page's side of the counters: it makes them, shared, for the worker to add to
 * (`counts`, handed over in `init`), and reads them. */
export class Meter {
	readonly counts = new Uint32Array(
		new SharedArrayBuffer((SINCE + 1) * Uint32Array.BYTES_PER_ELEMENT)
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
