// The worker's record of the screen: its one decoder. The session's first hello names the
// codec, which stays the session's (the daemon declares the level of the largest desktop);
// a later hello (a resize) changes the layout and nothing here. Every update says which
// keyframe it descends from, and refers only to that keyframe and pictures after it that
// the page reported decoded. So the record decodes a keyframe always, and a delta exactly
// when the keyframe it descends from is the last one it decoded, whatever order the
// keyframe, the delta and the hellos arrive in (the hello comes over the control stream,
// the updates over the lane, and nothing orders the two). A delta whose keyframe the page
// never decoded is passed over, counted, on a datagram lane, where the keyframe may have
// missed its deadline and the daemon, told of the miss, sends another; on a stream lane
// every update arrives, so such a delta is a fault. Every fault (a decoder error, a codec
// the browser cannot decode, a hello that changes the codec) ends the session for its
// cause. Nothing asks for a keyframe, and nothing is reconfigured. Events go in as calls;
// what the record does goes out through a `World`, which in the worker is WebCodecs and the
// page, and in frames.test.ts a fake of them.

/** How the session's lane carries the frames (the relay's lanes description). */
export type LaneKind = 'datagram' | 'stream';

/** What an update says of itself (its video header). */
export interface Update {
	/** It is a keyframe. */
	key: boolean;
	/** Its number: the daemon's count of the frames it sent, wrapping. */
	frame: number;
	/** The number of the keyframe it descends from (a keyframe's own). */
	from: number;
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
	/** A delta passed undecoded: its keyframe never reached the decoder (a datagram lane). */
	passed(): void;
	/** The screen cannot go on: the session ends, for `why`. */
	end(why: string): void;
}

/**
 * The screen: no session (before its hello, after it ended or failed); a session of a codec
 * on a lane of a kind, no keyframe decoded yet; or decoding, with the decoder opened on the
 * session's first keyframe and the number of the last keyframe it decoded.
 */
type Screen<D> =
	| { readonly s: 'none' }
	| { readonly s: 'ready'; readonly lane: LaneKind; readonly codec: string }
	| {
			readonly s: 'decoding';
			readonly lane: LaneKind;
			readonly codec: string;
			readonly dec: D;
			readonly key: number;
	  };

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

	/** A hello naming `codec`, on a lane of kind `lane`. The session's first begins it and
	 * returns its number, for `unsupported` (its codec is checked once); a later one names
	 * the same codec and changes nothing here (null), or ends the session. */
	hello(lane: LaneKind, codec: string): number | null {
		const s = this.#screen;
		if (s.s === 'none') {
			this.#screen = { s: 'ready', lane, codec };
			return ++this.#sessions;
		}
		if (s.codec !== codec)
			this.#fail(`a hello changed the codec from ${s.codec} to ${codec}`);
		return null;
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
				if (!u.key) return this.#unrooted(s.lane);
				let dec: D;
				try {
					dec = this.#world.open(s.codec);
				} catch (e) {
					return this.#fail(`cannot decode ${s.codec}: ${e}`);
				}
				this.#screen = {
					s: 'decoding',
					lane: s.lane,
					codec: s.codec,
					dec,
					key: u.frame,
				};
				return this.#decode(dec, u, at, data);
			}
			case 'decoding':
				if (u.key) this.#screen = { ...s, key: u.frame };
				else if (u.from !== s.key) return this.#unrooted(s.lane);
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

	/** A delta whose keyframe the decoder never had: passed over where updates can miss,
	 * a fault where every update arrives. */
	#unrooted(lane: LaneKind) {
		switch (lane) {
			case 'datagram':
				return this.#world.passed();
			case 'stream':
				return this.#fail('a delta whose keyframe never came');
		}
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
