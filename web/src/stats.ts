// The counters the worker keeps and the page reads when asked (`window.voxlStats`). The
// page's meter makes them, in memory it shares with the worker (handed over in `init`),
// and reads them as rates: each reading is a copy of the totals, and the rates are the
// differences between two readings, computed from the two alone.

/** The counters, by their index in the shared array: each a running total, wrapping at
 * 2^32. */
export const Count = {
	/** Updates shown. */
	Frames: 0,
	/** Updates arrived. */
	Updates: 1,
	/** Bytes of updates arrived. */
	Bytes: 2,
	/** Arrival to shown of the updates shown, in µs. */
	DecodeUs: 3,
	/** The server's capture to send of the updates arrived, in tenths of a ms. */
	PipeTenths: 4,
	/** Deltas passed undecoded while a datagram lane's stream waited for its keyframe. */
	Passed: 5,
} as const;
type Counter = (typeof Count)[keyof typeof Count];

/** The counters' rates and means between two readings. */
export interface Rates {
	fps: number;
	kbps: number;
	/** Mean arrival to shown, and the server's capture to send; null when none came. */
	decodeMs: number | null;
	pipeMs: number | null;
	/** Deltas passed undecoded, waiting for a keyframe. */
	passed: number;
}

/** What `window.voxlStats` reads: the rates since the last read. */
export interface Stats extends Rates {
	/** What the decoder decodes; null while there is none (before a keyframe). */
	codec: string | null;
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
		passed: moved(Count.Passed),
	};
}

/** The page's side of the counters: it makes them, shared, for the worker to add to
 * (`counts`, handed over in `init`), and reads them. */
export class Meter {
	readonly counts = new Uint32Array(
		new SharedArrayBuffer(
			Object.keys(Count).length * Uint32Array.BYTES_PER_ELEMENT
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
}
