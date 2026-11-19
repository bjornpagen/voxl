// The worker's record of the screen: its one decoder. The session's hello names the codec,
// which stays the session's (the daemon declares the level of the largest desktop). The lane
// is one stream that carries every update in order, and a viewer's lane begins with a
// keyframe: a keyframe starts the decoder, and every update after it decodes, in the order
// it came. An update before the hello or a delta before the first keyframe is a fault, as is
// a second hello, a decoder error or a codec the browser cannot decode: each ends the session
// for its cause. Nothing asks for a keyframe, and nothing is reconfigured.
//
// The page holds at most one update whose picture has not come out of the decoder. One,
// because an update decodes from the picture before it: a second could only wait behind the
// first, and what waits in the page is lag the daemon cannot see. The relay gives one update
// at a time, and the next only once the page says it took the one before (`took`, said by
// session.ts). Took means exactly this: the page is done with that update, which is decoded
// and its picture handed to the track (`World.present`, which the worker posts to the
// page's thread, whose presenter writes it to the video track). It does not mean the picture
// was seen: the presenter keeps the newest picture it was handed, and one still waiting
// behind a write is replaced. So an update is never behind another in the worker, its time
// from arrival to handed over is its own decode, and one that arrives while another is
// inside the decoder is a fault like the others.
//
// So the record relies on one property of the decoder: it puts out one picture for each
// update given it, before any further input. The daemon's part holds: its encoder gives
// each update one shown frame, with nothing reordered (src/vp9.rs's test). The decoder's
// part is the browser's, and WebCodecs does not promise it (worker.ts quotes what it says).
// Measured on 2026-11-19 in Chrome 157 on an M2 Max, with the decoder configured as
// worker.ts configures it (`optimizeForLatency: true`, `hardwareAcceleration:
// 'no-preference'`): 60 of 60 pictures came out before the next input was given, none held
// back, for hardware VP9 and for software AV1 (Chromium's dav1d wrapper sets max_frame_delay
// to 1 in low-delay mode).
//
// A decoder that keeps a picture in all the same leaves the record decoding, and the session
// waiting, for good: the record neither ends the session nor takes the update. It is not
// silent: the record tells the world what is inside the decoder at every change
// (`World.decoder`), and the page's meter shows it as decoding since that update's arrival
// (stats.ts), for as long as it lasts, which a harness reads. Nothing here keeps a timer.
//
// Events go in as calls; what the record does goes out through a `World`, which in the
// worker is WebCodecs, the page and the relay, and in frames.test.ts a fake of them. Each
// event is one step from the screen as it is to the screen as it is next: what the step
// tells the world it tells before the record is in the next state, and nothing the world
// is told calls the record back.

import type { Coding } from './protocol.ts';
import type { Decoder } from './stats.ts';

/** What an update says of itself (its video header). */
export interface Update {
	coding: Coding;
}

/** A session of the screen: its codec. Each is an object of its own, so that what is asked
 * for one session (whether the browser can decode its codec) is answered for that session
 * and lands nowhere once the record has left it. */
export interface Session {
	readonly codec: string;
}

/** What the record does. `D` is a decoder, `P` a picture, `B` an update's bytes. */
export interface World<D, P, B> {
	/** `session` began: whether the browser can decode its codec is to be checked, once,
	 * and `unsupported(session)` called if it cannot. */
	check(session: Session): void;
	/** The record now decodes `codec`: its first keyframe opened the decoder. */
	decodes(codec: string): void;
	/** The record decodes nothing any more: its decoder closed or failed. */
	decodesNothing(): void;
	/** A decoder for `codec`; throws if the codec cannot be configured. Its pictures go to
	 * `output`, and its error, after which it is closed, to `error`. */
	open(codec: string): D;
	/** Gives `dec` an update that arrived at `at`, which names the picture it puts out.
	 * Throws if the decoder refuses it. */
	decode(dec: D, coding: Coding, at: number, data: B): void;
	/** What is inside the decoder now: the update it was given, since that update's
	 * arrival, or nothing. Told at every change. */
	decoder(state: Decoder): void;
	/** Closes a decoder the record is done with. */
	close(dec: D): void;
	/** Lets go of a picture that is not handed to the page. */
	letGo(picture: P): void;
	/** Hands the page a picture to present: its update is out of the decoder, and the
	 * worker holds none undecoded. */
	present(picture: P): void;
	/** The screen cannot go on: the session ends, for `why`. */
	end(why: string): void;
}

/** A session with the decoder its first keyframe opened. */
interface Opened<D> {
	readonly session: Session;
	readonly dec: D;
}

/**
 * The screen: no session (before its hello, after it ended or failed); a session with no
 * keyframe yet; or one with its decoder, which holds nothing (idle) or the one update whose
 * picture it has yet to put out (decoding).
 */
type Screen<D> =
	| { readonly s: 'none' }
	| { readonly s: 'ready'; readonly session: Session }
	| ({ readonly s: 'idle' } & Opened<D>)
	| ({ readonly s: 'decoding' } & Opened<D>);

type None = Extract<Screen<never>, { s: 'none' }>;
const NONE: None = { s: 'none' };

export class Frames<D, P, B> {
	readonly #world: World<D, P, B>;
	#screen: Screen<D> = NONE;

	constructor(world: World<D, P, B>) {
		this.#world = world;
	}

	/** A hello naming `codec`: it begins a session, whose codec is checked (`World.check`).
	 * A second ends the session. */
	hello(codec: string) {
		this.#screen = this.#greeted(this.#screen, { codec });
	}

	/** `session`'s codec cannot be decoded here: the session ends, if the record is still
	 * in it. An answer for a session the record has left is no state's, and changes none. */
	unsupported(session: Session) {
		this.#screen = this.#refused(this.#screen, session);
	}

	/** The session closed: the decoder, if any, is closed. */
	closed() {
		this.#screen = this.#shut(this.#screen);
	}

	/** An update arrived at `at`. */
	part(at: number, u: Update, data: B) {
		this.#screen = this.#given(this.#screen, at, u, data);
	}

	/** Decoder `dec` put out `picture`: handed on if it is the picture of the update inside
	 * the record's decoder, which then holds nothing; let go otherwise. */
	output(dec: D, picture: P) {
		this.#screen = this.#out(this.#screen, dec, picture);
	}

	/** Decoder `dec` failed, and is closed. One the record is done with changes nothing. */
	error(dec: D, name: string, message: string) {
		this.#screen = this.#broke(this.#screen, dec, `${name}: ${message}`);
	}

	#greeted(s: Screen<D>, session: Session): Screen<D> {
		switch (s.s) {
			case 'none':
				this.#world.check(session);
				return { s: 'ready', session };
			case 'ready':
			case 'idle':
			case 'decoding':
				return this.#fail(s, 'a second hello');
		}
	}

	#refused(s: Screen<D>, session: Session): Screen<D> {
		switch (s.s) {
			case 'none':
				return s;
			case 'ready':
			case 'idle':
			case 'decoding':
				return s.session === session
					? this.#fail(s, `this browser cannot decode ${session.codec}`)
					: s;
		}
	}

	#given(s: Screen<D>, at: number, u: Update, data: B): Screen<D> {
		switch (s.s) {
			case 'none':
				return this.#fail(s, "an update before the session's hello");
			case 'ready':
				return this.#first(s.session, at, u, data);
			case 'idle':
				return this.#decode({ ...s, s: 'decoding' }, at, u, data);
			case 'decoding':
				return this.#fail(s, 'an update before the one before it was taken');
		}
	}

	/** A session's first update: a keyframe, which opens its decoder. */
	#first(session: Session, at: number, u: Update, data: B): Screen<D> {
		const ready = { s: 'ready', session } as const;
		switch (u.coding) {
			case 'delta':
				return this.#fail(ready, 'a delta before its keyframe');
			case 'key': {
				let dec: D;
				try {
					dec = this.#world.open(session.codec);
				} catch (e) {
					return this.#fail(ready, `cannot decode ${session.codec}: ${e}`);
				}
				this.#world.decodes(session.codec);
				return this.#decode({ s: 'decoding', session, dec }, at, u, data);
			}
		}
	}

	/** The decoder of `next` is given an update: `next` is the screen if it takes it. */
	#decode(
		next: Extract<Screen<D>, { s: 'decoding' }>,
		at: number,
		u: Update,
		data: B
	): Screen<D> {
		try {
			this.#world.decode(next.dec, u.coding, at, data);
		} catch (e) {
			// Refused, the update is not inside the decoder.
			return this.#fail(
				{ ...next, s: 'idle' },
				`the decoder refused an update: ${e}`
			);
		}
		this.#world.decoder({ s: 'decoding', since: at });
		return next;
	}

	/** A picture came out of `dec`. WebCodecs has no way to take back what a decoder has
	 * queued, so a decoder closed with its session may still put a picture out: the picture
	 * names the decoder it came from, and only the screen's own decoder, holding an update,
	 * has a picture to hand on. */
	#out(s: Screen<D>, dec: D, picture: P): Screen<D> {
		switch (s.s) {
			case 'none':
			case 'ready':
			case 'idle':
				this.#world.letGo(picture);
				return s;
			case 'decoding':
				if (s.dec !== dec) {
					this.#world.letGo(picture);
					return s;
				}
				this.#world.decoder({ s: 'idle' });
				this.#world.present(picture);
				return { ...s, s: 'idle' };
		}
	}

	/** `dec` failed, for `why`: as with its pictures, only the screen's own decoder's error
	 * is the screen's. */
	#broke(s: Screen<D>, dec: D, why: string): Screen<D> {
		switch (s.s) {
			case 'none':
			case 'ready':
				return s;
			case 'idle':
			case 'decoding':
				if (s.dec !== dec) return s;
				this.#emptied(s);
				this.#world.decodesNothing();
				this.#world.end(`the decoder failed: ${why}`);
				return NONE;
		}
	}

	/** The decoder of `s` is done with: what was inside it is gone. */
	#emptied(s: Extract<Screen<D>, { s: 'idle' | 'decoding' }>): 'emptied' {
		switch (s.s) {
			case 'idle':
				return 'emptied';
			case 'decoding':
				this.#world.decoder({ s: 'idle' });
				return 'emptied';
		}
	}

	/** The decoder, if any, is closed. */
	#shut(s: Screen<D>): None {
		switch (s.s) {
			case 'none':
			case 'ready':
				return NONE;
			case 'idle':
			case 'decoding':
				this.#world.close(s.dec);
				this.#emptied(s);
				this.#world.decodesNothing();
				return NONE;
		}
	}

	/** The screen cannot go on. */
	#fail(s: Screen<D>, why: string): None {
		const none = this.#shut(s);
		this.#world.end(why);
		return none;
	}
}
