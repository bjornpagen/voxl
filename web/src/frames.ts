// The worker's record of the screen: what its one decoder is. An update is shown as soon as
// the decoder puts it out: the relay gives the page updates in order and drops those that
// can no longer be decoded (one referring to an update that missed its deadline), so an
// update may follow a gap in the frame numbers, which is no failure. Events go in as calls;
// what the record does goes out through a `World`, which in the worker is WebCodecs and the
// canvas, and in frames.test.ts a fake of them.

/** What the record does. `D` is a decoder, `P` a picture, `B` an update's bytes. */
export interface World<D, P, B> {
	/** The codec a keyframe's SPS names; throws if it names none. */
	codec(key: B): string;
	/** A decoder for `codec`; throws if the codec cannot be configured. Its pictures go to
	 * `output`, and its error, after which it is closed, to `error`. */
	open(codec: string): D;
	/** Gives `dec` an update that arrived at `at`, which names the picture it puts out.
	 * Throws if the decoder refuses it. */
	decode(dec: D, key: boolean, at: number, data: B): void;
	/** Closes a decoder the record gives up on. */
	close(dec: D): void;
	/** Asks the server for a keyframe. */
	ask(): void;
	/** Says why the screen stopped decoding. */
	log(why: string): void;
	/** Lets go of a picture that will not be shown. */
	letGo(picture: P): void;
	/** Shows a picture. */
	show(picture: P): void;
}

/**
 * What the screen's updates go to. Before the stream starts (a hello) and after the
 * socket closes there is no stream, and an update is dropped. In a stream, updates decode
 * with a decoder opened on a keyframe, for the codec that keyframe names. One whose decoder
 * failed waits for a keyframe, and asked for it as it failed; so does every stream as it
 * starts, which the server answers with one unasked. One whose codec this browser cannot
 * decode waits for nothing until the stream starts again: asking would bring a keyframe
 * that fails the same way.
 */
type Screen<D> =
	| { readonly s: 'none' }
	| { readonly s: 'decoding'; readonly dec: D; readonly codec: string }
	| { readonly s: 'waiting' }
	| { readonly s: 'unsupported' };

const NONE = { s: 'none' } as const;
const WAITING = { s: 'waiting' } as const;
const UNSUPPORTED = { s: 'unsupported' } as const;

export class Frames<D, P, B> {
	readonly #world: World<D, P, B>;
	#screen: Screen<D> = NONE;
	/** Updates in the live decoder, given and not yet put out. */
	#held = 0;
	#dropped = 0;

	constructor(world: World<D, P, B>) {
		this.#world = world;
	}

	/** Updates that came out of no decoder: dropped as they arrived, or lost in a decoder
	 * given up on. */
	get dropped(): number {
		return this.#dropped;
	}

	/** What the decoder decodes; null while there is none. */
	get codec(): string | null {
		return this.#screen.s === 'decoding' ? this.#screen.codec : null;
	}

	/** The stream starts (a hello): the decoder is given up on, so no picture put out before
	 * is shown, since the screen may have changed. */
	hello() {
		this.#end();
		this.#screen = WAITING;
	}

	/** The socket closed: the decoder is given up on. */
	closed() {
		this.#end();
		this.#screen = NONE;
	}

	/** An update arrived at `at`. */
	part(at: number, key: boolean, data: B) {
		const s = key ? this.#keyframe(data) : this.#screen;
		if (s.s !== 'decoding') {
			this.#dropped++;
			return;
		}
		try {
			this.#world.decode(s.dec, key, at, data);
			this.#held++;
		} catch (e) {
			this.#close(s.dec);
			this.#failed(`decode threw ${e}`);
			this.#dropped++;
		}
	}

	/** Decoder `dec` put out `picture`: shown if the record still decodes with it, let go
	 * otherwise. */
	output(dec: D, picture: P) {
		const s = this.#screen;
		if (s.s !== 'decoding' || s.dec !== dec) {
			this.#world.letGo(picture);
			return;
		}
		this.#held--;
		this.#world.show(picture);
	}

	/** Decoder `dec` failed with an error named `name`, and is closed. One the record had
	 * given up on changes nothing. */
	error(dec: D, name: string, message: string) {
		const s = this.#screen;
		if (s.s !== 'decoding' || s.dec !== dec) return;
		this.#lost();
		if (name === 'NotSupportedError') this.#unsupported(message);
		else this.#failed(`decoder error: ${message}`);
	}

	/** What the screen is once keyframe `data` arrives: a keyframe of the codec its decoder
	 * decodes goes to that decoder; any other opens one for its own. */
	#keyframe(data: B): Screen<D> {
		const s = this.#screen;
		if (s.s === 'none' || s.s === 'unsupported') return s;
		let codec: string;
		try {
			codec = this.#world.codec(data);
		} catch (e) {
			if (s.s === 'decoding') this.#close(s.dec);
			return this.#unsupported(`${e}`);
		}
		if (s.s === 'decoding') {
			if (s.codec === codec) return s;
			this.#close(s.dec);
		}
		try {
			const dec = this.#world.open(codec);
			return (this.#screen = { s: 'decoding', dec, codec });
		} catch (e) {
			return this.#unsupported(`configure ${codec}: ${e}`);
		}
	}

	/** The decoder, if any, is given up on. */
	#end() {
		const s = this.#screen;
		if (s.s === 'decoding') this.#close(s.dec);
	}

	/** Decoder `dec` is given up on while open. */
	#close(dec: D) {
		this.#world.close(dec);
		this.#lost();
	}

	/** The decoder is gone: what it held never comes out. */
	#lost() {
		this.#dropped += this.#held;
		this.#held = 0;
		this.#screen = WAITING;
	}

	#failed(why: string) {
		this.#world.log(why);
		this.#world.ask();
	}

	#unsupported(why: string): Screen<D> {
		this.#world.log(`cannot decode: ${why}`);
		return (this.#screen = UNSUPPORTED);
	}
}
