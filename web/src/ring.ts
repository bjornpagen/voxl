// The sound waiting to be played: the newest samples a stream brought, in order, and at
// most a fixed number of them. A stream brings sound late and then all at once. So what
// comes in over the most the ring holds drops the oldest (dropping beats lag: the sound
// played is never older than what the ring holds), and when the ring holds less than is
// asked for it plays nothing and keeps what it has, so sound that is late is silence for as
// long as it is late and then plays at once. There is no level to fill to before playing
// and none to drop to: the ring's most is its one number.

export class Ring {
	readonly #most: number;
	readonly #l: Float32Array;
	readonly #r: Float32Array;
	/** Where the next sample goes; the oldest held is `#fill` before it, around the ring. */
	#wr = 0;
	#fill = 0;

	/** A ring that holds at most `most` samples of each channel. */
	constructor(most: number) {
		this.#most = most;
		this.#l = new Float32Array(most);
		this.#r = new Float32Array(most);
	}

	/** The samples waiting. */
	get fill(): number {
		return this.#fill;
	}

	/** Sound arrived: `l` and `r`, of one length. What the ring then holds over its most,
	 * the oldest, is dropped. */
	push(l: Float32Array, r: Float32Array) {
		const most = this.#most;
		let wr = this.#wr;
		for (let i = 0; i < l.length; i++) {
			this.#l[wr] = l[i];
			this.#r[wr] = r[i];
			wr = wr + 1 === most ? 0 : wr + 1;
		}
		this.#wr = wr;
		this.#fill = Math.min(this.#fill + l.length, most);
	}

	/** Plays the oldest samples into `l` and `r`, of one length, if it holds that many; if
	 * not it leaves them as they are and keeps what it holds. */
	play(l: Float32Array, r: Float32Array) {
		const n = l.length;
		if (this.#fill < n) return;
		const most = this.#most;
		let rd = this.#wr - this.#fill;
		if (rd < 0) rd += most;
		for (let i = 0; i < n; i++) {
			l[i] = this.#l[rd];
			r[i] = this.#r[rd];
			rd = rd + 1 === most ? 0 : rd + 1;
		}
		this.#fill -= n;
	}
}
