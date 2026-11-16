// The worker's record of the screen: what each stripe's decoder is, and the frames the
// server has sent and the page has not yet acknowledged. The server counts a frame as on
// its way until an ack names it or a later frame (src/hub.rs), so every frame is
// acknowledged once, in the order sent, as soon as all of its parts have arrived and
// every part given to a decoder has come out of it or never will. Events go in as calls;
// what the record does goes out through a `World`, which in the worker is WebCodecs, the
// socket and the canvas, and in frames.test.ts a fake of them.
import type { VideoHeader } from './protocol';

/** What the record does. `D` is a decoder, `P` a picture, `B` a part's bytes. */
export interface World<D, P, B> {
	/** The codec a keyframe's SPS names; throws if it names none. */
	codec(key: B): string;
	/** A decoder for stripe `stripe`'s `codec`; throws if the codec cannot be configured.
	 * Its pictures go to `output`, and its error, after which it is closed, to `error`. */
	open(stripe: number, codec: string): D;
	/** Gives `dec` a part of frame `frame`, which names the picture it puts out. Throws if
	 * the decoder refuses it. */
	decode(dec: D, key: boolean, frame: number, data: B): void;
	/** Closes a decoder the record gives up on. */
	close(dec: D): void;
	/** Asks the server for a keyframe. */
	ask(): void;
	/** Says why a stripe stopped decoding. */
	log(stripe: number, why: string): void;
	/** Lets go of a picture that will not be shown. */
	letGo(picture: P): void;
	/** Shows a frame with nothing more to come, and acknowledges it. */
	show(frame: Frame<P>): void;
}

/**
 * What a stripe's parts go to. A stripe decodes with a decoder opened on its keyframe, for
 * the codec that keyframe names. One whose decoder failed waits for a keyframe, and asked
 * for it as it failed; so does every stripe when the stream starts (a hello), which the
 * server answers with one unasked. One whose codec this browser cannot decode waits for
 * nothing until the stream starts again: asking would bring a keyframe that fails the same
 * way.
 */
type Stripe<D> =
	| { readonly s: 'decoding'; readonly dec: D; readonly codec: string }
	| { readonly s: 'waiting' }
	| { readonly s: 'unsupported' };

const WAITING = { s: 'waiting' } as const;
const UNSUPPORTED = { s: 'unsupported' } as const;

/** A frame not yet acknowledged. */
export interface Frame<P> {
	readonly id: number;
	/** Its parts that have not arrived. */
	coming: number;
	/** The stripes whose decoder holds one of its parts, a bit each: a frame has at most
	 * one part a stripe, and a screen at most 16 stripes (src/video.rs). */
	decoding: number;
	/** What its parts came out of their decoders as. */
	readonly pictures: P[];
	/** When its first part arrived. */
	readonly first: number;
	/** When its last part arrived. */
	last: number;
}

/** What the record reads of a part's header. */
export type Head = Pick<VideoHeader, 'key' | 'frame' | 'stripe' | 'parts'>;

export class Frames<D, P, B> {
	readonly #world: World<D, P, B>;
	#stripes: Stripe<D>[] = [];
	/** Oldest first. */
	readonly #frames: Frame<P>[] = [];
	#dropped = 0;

	constructor(world: World<D, P, B>) {
		this.#world = world;
	}

	/** How many frames are not yet acknowledged. */
	get waiting(): number {
		return this.#frames.length;
	}

	/** Parts that came out of no decoder: dropped as they arrived, or lost in a decoder
	 * given up on. */
	get dropped(): number {
		return this.#dropped;
	}

	/** What the first stripe decodes. */
	get codec(): string | null {
		const s = this.#stripes[0];
		return s?.s === 'decoding' ? s.codec : null;
	}

	/** The stream starts, cut into `count` stripes (a hello): every decoder is given up on,
	 * and no picture put out before is shown, since the screen may have changed. A frame
	 * that has not arrived whole is still acknowledged once it has. */
	hello(count: number) {
		this.#end();
		this.#stripes = Array.from({ length: count }, () => WAITING);
		this.#flush();
	}

	/** The socket closed: every decoder is given up on, and nothing pending is
	 * acknowledged. */
	closed() {
		this.#end();
		this.#stripes = [];
		this.#frames.length = 0;
	}

	/** A part arrived at `at`, of its frame's `head.parts`. Whether it was the frame's
	 * first. */
	part(at: number, head: Head, data: B): boolean {
		const { key, frame, stripe: i } = head;
		let f = this.#find(frame);
		const first = !f;
		if (f) {
			f.coming--;
			f.last = at;
		} else {
			f = {
				id: frame,
				coming: head.parts - 1,
				decoding: 0,
				pictures: [],
				first: at,
				last: at,
			};
			this.#frames.push(f);
		}
		const s = key ? this.#keyframe(i, data) : this.#stripes[i];
		if (s.s === 'decoding') {
			try {
				this.#world.decode(s.dec, key, frame, data);
				f.decoding |= 1 << i;
			} catch (e) {
				this.#close(i, s.dec);
				this.#failed(i, `decode threw ${e}`);
				this.#dropped++;
			}
		} else this.#dropped++;
		this.#flush();
		return first;
	}

	/** Stripe `i`'s decoder put out its part of frame `frame` as `picture`. Its frame keeps
	 * it unless that decoder was given up on, and the part with it. */
	output(i: number, frame: number, picture: P) {
		const f = this.#find(frame);
		const bit = 1 << i;
		if (!f || !(f.decoding & bit)) {
			this.#world.letGo(picture);
			return;
		}
		f.decoding &= ~bit;
		f.pictures.push(picture);
		this.#flush();
	}

	/** Decoder `dec` failed with an error named `name`, and is closed. One the record had
	 * given up on changes nothing. */
	error(dec: D, name: string, message: string) {
		const i = this.#stripes.findIndex(
			(s) => s.s === 'decoding' && s.dec === dec
		);
		if (i < 0) return;
		this.#lost(i);
		if (name === 'NotSupportedError') this.#unsupported(i, message);
		else this.#failed(i, `decoder error: ${message}`);
		this.#flush();
	}

	/** What stripe `i` is once its keyframe `data` arrives: a keyframe of the codec its
	 * decoder decodes goes to that decoder; any other opens one for its own. */
	#keyframe(i: number, data: B): Stripe<D> {
		const s = this.#stripes[i];
		if (s.s === 'unsupported') return s;
		let codec: string;
		try {
			codec = this.#world.codec(data);
		} catch (e) {
			if (s.s === 'decoding') this.#close(i, s.dec);
			return this.#unsupported(i, `${e}`);
		}
		if (s.s === 'decoding') {
			if (s.codec === codec) return s;
			this.#close(i, s.dec);
		}
		try {
			const dec = this.#world.open(i, codec);
			return (this.#stripes[i] = { s: 'decoding', dec, codec });
		} catch (e) {
			return this.#unsupported(i, `configure ${codec}: ${e}`);
		}
	}

	/** Every decoder is given up on, and no picture put out is shown. */
	#end() {
		this.#stripes.forEach((s, i) => {
			if (s.s === 'decoding') this.#close(i, s.dec);
		});
		for (const f of this.#frames) {
			for (const p of f.pictures) this.#world.letGo(p);
			f.pictures.length = 0;
		}
	}

	/** Stripe `i`'s decoder `dec` is given up on while open. */
	#close(i: number, dec: D) {
		this.#world.close(dec);
		this.#lost(i);
	}

	/** Stripe `i`'s decoder is gone: what it held never comes out, so the frames waiting
	 * for it go without it. */
	#lost(i: number) {
		const bit = 1 << i;
		for (const f of this.#frames) {
			if (f.decoding & bit) this.#dropped++;
			f.decoding &= ~bit;
		}
		this.#stripes[i] = WAITING;
	}

	#failed(i: number, why: string) {
		this.#world.log(i, why);
		this.#world.ask();
	}

	#unsupported(i: number, why: string): Stripe<D> {
		this.#world.log(i, `cannot decode: ${why}`);
		return (this.#stripes[i] = UNSUPPORTED);
	}

	#find(id: number): Frame<P> | undefined {
		for (const f of this.#frames) if (f.id === id) return f;
		return undefined;
	}

	#flush() {
		while (this.#frames.length) {
			const f = this.#frames[0];
			if (f.coming || f.decoding) return;
			this.#frames.shift();
			this.#world.show(f);
		}
	}
}
