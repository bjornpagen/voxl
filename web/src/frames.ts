// The worker's record of the screen: its one decoder. Every update the relay gives the page
// after a stream's keyframe is decodable (the encoder refers only to pictures it knows the
// page has), so a stream, whose codec its hello names, decodes from its keyframe on, in
// order. Before the keyframe the lane's kind decides: on a datagram lane the keyframe may
// have missed its deadline, and the deltas after it pass undecoded (counted) until the
// daemon, told of the miss, starts the stream again with a new hello and keyframe; on a
// stream lane every frame arrives, so a delta before the keyframe is a fault. Every fault (a
// decoder error, a codec the browser cannot decode) ends the session for its cause. Nothing
// asks for a keyframe, and nothing is reconfigured. Events go in as calls; what the record
// does goes out through a `World`, which in the worker is WebCodecs and the page, and in
// frames.test.ts a fake of them.

/** How the session's lane carries the frames (the relay's lanes description). */
export type LaneKind = 'datagram' | 'stream';

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
	/** A delta passed undecoded while a datagram lane's stream waits for its keyframe. */
	passed(): void;
	/** The screen cannot go on: the session ends, for `why`. */
	end(why: string): void;
}

/**
 * The screen: no stream (before a hello, after the session ended or failed); a stream of a
 * codec, on a lane of a kind, waiting for its keyframe; or decoding with the decoder opened
 * on it.
 */
type Screen<D> =
	| { readonly s: 'none' }
	| { readonly s: 'waiting'; readonly lane: LaneKind; readonly codec: string }
	| { readonly s: 'decoding'; readonly dec: D; readonly codec: string };

const NONE = { s: 'none' } as const;

export class Frames<D, P, B> {
	readonly #world: World<D, P, B>;
	#screen: Screen<D> = NONE;
	/** The streams begun so far: the newest is the one the record is in, if any. */
	#streams = 0;

	constructor(world: World<D, P, B>) {
		this.#world = world;
	}

	/** What the decoder decodes; null while there is none. */
	get codec(): string | null {
		return this.#screen.s === 'decoding' ? this.#screen.codec : null;
	}

	/** A stream of `codec` starts on a lane of kind `lane` (a hello): the decoder, if any, is
	 * closed, so no picture of the old stream is shown. The stream's number, for
	 * `unsupported`. */
	hello(lane: LaneKind, codec: string): number {
		this.#close();
		this.#screen = { s: 'waiting', lane, codec };
		return ++this.#streams;
	}

	/** Stream `stream`'s codec cannot be decoded here: if the record is still in it, the
	 * session ends. */
	unsupported(stream: number) {
		const s = this.#screen;
		if (s.s === 'none' || stream !== this.#streams) return;
		this.#fail(`this browser cannot decode ${s.codec}`);
	}

	/** The session closed: the decoder, if any, is closed. */
	closed() {
		this.#close();
		this.#screen = NONE;
	}

	/** An update arrived at `at`. */
	part(at: number, key: boolean, data: B) {
		const s = this.#screen;
		switch (s.s) {
			case 'none':
				return this.#fail('an update outside a stream');
			case 'waiting': {
				if (!key)
					switch (s.lane) {
						// Its keyframe missed: the daemon starts the stream again.
						case 'datagram':
							return this.#world.passed();
						case 'stream':
							return this.#fail("a delta before the stream's keyframe");
					}
				let dec: D;
				try {
					dec = this.#world.open(s.codec);
				} catch (e) {
					return this.#fail(`cannot decode ${s.codec}: ${e}`);
				}
				this.#screen = { s: 'decoding', dec, codec: s.codec };
				return this.#decode(dec, key, at, data);
			}
			case 'decoding':
				return this.#decode(s.dec, key, at, data);
		}
	}

	/** Decoder `dec` put out `picture`: shown if the record still decodes with it, let go
	 * otherwise (a decoder closed by a hello may still put out what it held). */
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

	#decode(dec: D, key: boolean, at: number, data: B) {
		try {
			this.#world.decode(dec, key, at, data);
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
