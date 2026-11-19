// The worker's record of the screen: its one decoder. The session's hello names the codec,
// which stays the session's (the daemon declares the level of the largest desktop). The lane
// is one stream that carries every update in order, and a viewer's lane begins with a
// keyframe: a keyframe starts the decoder, and every update after it decodes, in the order
// it came. An update before the hello or a delta before the first keyframe is a fault, as is
// a second hello, a decoder error or a codec the browser cannot decode: each ends the session
// for its cause. Nothing asks for a keyframe, and nothing is reconfigured. Events go in as
// calls; what the record does goes out through a `World`, which in the worker is WebCodecs
// and the page, and in frames.test.ts a fake of them.

/** What an update says of itself (its video header). */
export interface Update {
	/** It is a keyframe. */
	key: boolean;
}

/** What the record does. `D` is a decoder, `P` a picture, `B` an update's bytes. */
export interface World<D, P, B> {
	/** A decoder for `codec`; throws if the codec cannot be configured. Its pictures go to
	 * `output`, and its error, after which it is closed, to `error`. */
	open(codec: string): D;
	/** Gives `dec` an update that arrived at `at`, which names the picture it puts out.
	 * Throws if the decoder refuses it. */
	decode(dec: D, key: boolean, at: number, data: B): void;
	/** Closes a decoder the record is done with. */
	close(dec: D): void;
	/** Lets go of a picture that will not be shown. */
	letGo(picture: P): void;
	/** Shows a picture. */
	show(picture: P): void;
	/** The screen cannot go on: the session ends, for `why`. */
	end(why: string): void;
}

/**
 * The screen: no session (before its hello, after it ended or failed); a session of a codec,
 * no keyframe decoded yet; or decoding, with the decoder opened on the session's first
 * keyframe.
 */
type Screen<D> =
	| { readonly s: 'none' }
	| { readonly s: 'ready'; readonly codec: string }
	| { readonly s: 'decoding'; readonly codec: string; readonly dec: D };

const NONE = { s: 'none' } as const;

export class Frames<D, P, B> {
	readonly #world: World<D, P, B>;
	#screen: Screen<D> = NONE;
	/** The sessions begun so far: the newest is the one the record is in, if any. */
	#sessions = 0;

	constructor(world: World<D, P, B>) {
		this.#world = world;
	}

	/** What the decoder decodes; null while there is none. */
	get codec(): string | null {
		return this.#screen.s === 'decoding' ? this.#screen.codec : null;
	}

	/** A hello naming `codec`. The session's begins it and returns its number, for
	 * `unsupported` (its codec is checked once); a second ends the session (null). */
	hello(codec: string): number | null {
		if (this.#screen.s !== 'none') {
			this.#fail('a second hello');
			return null;
		}
		this.#screen = { s: 'ready', codec };
		return ++this.#sessions;
	}

	/** Session `session`'s codec cannot be decoded here: if the record is still in it, the
	 * session ends. */
	unsupported(session: number) {
		const s = this.#screen;
		if (s.s === 'none' || session !== this.#sessions) return;
		this.#fail(`this browser cannot decode ${s.codec}`);
	}

	/** The session closed: the decoder, if any, is closed. */
	closed() {
		this.#close();
		this.#screen = NONE;
	}

	/** An update arrived at `at`. */
	part(at: number, u: Update, data: B) {
		const s = this.#screen;
		switch (s.s) {
			case 'none':
				return this.#fail("an update before the session's hello");
			case 'ready': {
				if (!u.key) return this.#fail('a delta before its keyframe');
				let dec: D;
				try {
					dec = this.#world.open(s.codec);
				} catch (e) {
					return this.#fail(`cannot decode ${s.codec}: ${e}`);
				}
				this.#screen = { s: 'decoding', codec: s.codec, dec };
				return this.#decode(dec, u, at, data);
			}
			case 'decoding':
				return this.#decode(s.dec, u, at, data);
		}
	}

	/** Decoder `dec` put out `picture`: shown if the record still decodes with it, let go
	 * otherwise (a decoder closed with its session may still put out what it held). */
	output(dec: D, picture: P) {
		const s = this.#screen;
		if (s.s === 'decoding' && s.dec === dec) this.#world.show(picture);
		else this.#world.letGo(picture);
	}

	/** Decoder `dec` failed, and is closed. One the record is done with changes nothing. */
	error(dec: D, name: string, message: string) {
		const s = this.#screen;
		if (s.s !== 'decoding' || s.dec !== dec) return;
		this.#screen = NONE;
		this.#world.end(`the decoder failed: ${name}: ${message}`);
	}

	#decode(dec: D, u: Update, at: number, data: B) {
		try {
			this.#world.decode(dec, u.key, at, data);
		} catch (e) {
			this.#fail(`the decoder refused an update: ${e}`);
		}
	}

	/** The decoder, if any, is closed. */
	#close() {
		const s = this.#screen;
		if (s.s === 'decoding') this.#world.close(s.dec);
	}

	/** The screen cannot go on. */
	#fail(why: string) {
		this.#close();
		this.#screen = NONE;
		this.#world.end(why);
	}
}
