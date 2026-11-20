// The worker's presenter: decoded pictures written to the page's video track, off the
// page's thread. The page makes the track (a MediaStreamTrackGenerator, whose <video> the
// compositor scans YUV out of) and hands its writable stream to the worker in `init`: the
// stream is transferable, so a picture goes from the decoder to the track without a hop
// through the page's thread, which input, layout and the files panel keep busy.
//
// At most one write is in flight; a newer picture replaces one waiting behind it, which is
// let go. So the presenter holds at most two pictures whatever the track does, and the
// newest is what is shown next. The page is told the desktop's size, the size of the
// pictures, only when it changes: it lays out from that and never sees a picture.
//
// Events go in as calls; what the presenter does goes out through a `World`, which in the
// worker is the track's writer and the page, and in track.test.ts a fake of them.

/** What the presenter does. `P` is a picture. */
export interface World<P> {
	/** Writes `picture` to the track, which owns it from here. The write ends, taken or
	 * refused, with a call of `written`. */
	write(picture: P): void;
	/** Lets go of a picture that is not written. */
	letGo(picture: P): void;
	/** The desktop is now `w` x `h`: the page lays out again. */
	size(w: number, h: number): void;
}

/** What waits behind the write in flight: no write in flight, a write with nothing behind
 * it, or a picture. */
type Writing<P> =
	| { readonly s: 'idle' }
	| { readonly s: 'writing' }
	| { readonly s: 'next'; readonly picture: P };

/** The desktop as the page was last told: nothing yet this session, or its size. */
type Desk =
	| { readonly s: 'none' }
	| { readonly s: 'shown'; readonly w: number; readonly h: number };

// The states that hold nothing are one object each: a picture written with nothing behind
// it makes none.
const IDLE = { s: 'idle' } as const;
const WRITING = { s: 'writing' } as const;
const NONE = { s: 'none' } as const;

export class Track<P> {
	readonly #world: World<P>;
	#writing: Writing<P> = IDLE;
	#desk: Desk = NONE;

	constructor(world: World<P>) {
		this.#world = world;
	}

	/** A picture of `w` x `h` came out of the decoder: the page is told its size if that
	 * is news, and the picture is written, or waits for the write in flight in place of
	 * any that waited. */
	present(picture: P, w: number, h: number) {
		const desk = this.#desk;
		if (desk.s === 'none' || desk.w !== w || desk.h !== h) {
			this.#desk = { s: 'shown', w, h };
			this.#world.size(w, h);
		}
		this.#writing = this.#came(this.#writing, picture);
	}

	/** The write in flight ended: the picture that waited behind it, if any, is written. */
	written() {
		this.#writing = this.#ended(this.#writing);
	}

	/** The session closed: the page has no desktop, so the next session's first picture
	 * tells its size, whatever it is. A write in flight ends as it would have. */
	closed() {
		this.#desk = NONE;
	}

	#came(w: Writing<P>, picture: P): Writing<P> {
		switch (w.s) {
			case 'idle':
				this.#world.write(picture);
				return WRITING;
			case 'writing':
				return { s: 'next', picture };
			case 'next':
				this.#world.letGo(w.picture);
				return { s: 'next', picture };
		}
	}

	#ended(w: Writing<P>): Writing<P> {
		switch (w.s) {
			// No write was in flight: nothing ended.
			case 'idle':
				return IDLE;
			case 'writing':
				return IDLE;
			case 'next':
				this.#world.write(w.picture);
				return WRITING;
		}
	}
}
