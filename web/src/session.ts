// The worker's record of its session with the relay: one socket, opened when the page asks
// and ended once, for one cause the page is told. Nothing reconnects by itself.
//
// The relay gives one video update or Opus packet at a time, and the next only once the
// worker says it took the one before (`Bin.took`, one byte). Took means exactly: the page is
// done with this message. For an update, it is decoded and its picture handed to the track:
// out of the decoder and posted to the page's thread, whose presenter writes it to the video
// track (`decoded`, which frames.ts's record calls through the worker; frames.ts says what
// the decoder must do for that to come). For a packet, it is handed to the audio decoder, or
// dropped when the sound is not wanted, at once. It does not mean the picture was seen: the
// presenter keeps the newest picture it was handed, and one still waiting behind a write is
// replaced. The browser's socket has no way to make a sender wait, and what it lets arrive
// unread it queues where the daemon cannot see it, so the wait is said. Sound and pictures
// are one stream in the daemon's order: a packet behind an update is given once that update
// is decoded, and never plays before it.
//
// A message that is none of the relay's, the relay's own word that the session ends, or a
// screen that cannot go on ends the session: the socket is closed, what arrives on it after
// that is nothing, and when it has closed the page is told the cause. A socket that closes
// with nothing said is its own cause.
//
// Events go in as calls; what the record does goes out through a `World`, which in the
// worker is the WebSocket, the page, the screen's record and the audio decoder, and in
// session.test.ts a fake of them.
import { Bin, cmd, readRelay } from './protocol.ts';
import type {
	FromRelay,
	FromWorker,
	ServerMessage,
	VideoHeader,
} from './protocol.ts';

/** What the record tells the page: everything the worker does but the screen's own. */
export type Told = Exclude<
	FromWorker,
	{ t: 'size' } | { t: 'decodes' } | { t: 'decodesNothing' }
>;

/** What the record does. `S` is a socket to the relay, and `A` where the desktop's sound
 * goes once the page asked for it. */
export interface World<S, A> {
	/** Opens a socket to the relay: its opening, its messages and its closing come back as
	 * `opened`, `message` and `closed`. */
	dial(): S;
	/** Sends the relay a message on `socket`, which is open. */
	send(socket: S, d: string | ArrayBuffer): void;
	/** Closes `socket`: `closed` follows. */
	hangUp(socket: S): void;
	/** Tells the page. */
	tell(m: Told): void;
	/** The screen's session begins, of `codec`. */
	hello(codec: string): void;
	/** An update for the screen, arrived at `at`. */
	update(at: number, header: VideoHeader, picture: Uint8Array): void;
	/** The screen's session closed. */
	screenClosed(): void;
	/** An Opus packet for `audio`. */
	sound(audio: A, packet: Uint8Array): void;
}

/** The session: none (before the page asked, after one closed); a socket being opened;
 * an open one; or one being closed for a cause, which the page is told when it has
 * closed. */
type State<S> =
	| { readonly s: 'none' }
	| { readonly s: 'dialing'; readonly socket: S }
	| Open<S>
	| { readonly s: 'ending'; readonly why: string };

interface Open<S> {
	readonly s: 'open';
	readonly socket: S;
}

/** The desktop's sound: not asked for, or wanted at `audio`. */
type Sound<A> = { readonly s: 'off' } | { readonly s: 'on'; readonly audio: A };

const TOOK = new Uint8Array([Bin.took]).buffer;

/** Every event is one step from the session as it is to the session as it is next. */
export class Session<S, A> {
	readonly #world: World<S, A>;
	#state: State<S> = { s: 'none' };
	/** What the page wants of the daemon, said at every session's opening and when it
	 * changes: the picture (it is visible) or not, and the sound. */
	#video = true;
	#sound: Sound<A> = { s: 'off' };

	constructor(world: World<S, A>) {
		this.#world = world;
	}

	/** The page asks for a session: one is opened if there is none. */
	connect() {
		this.#state = this.#asked(this.#state);
	}

	/** The socket opened: the daemon is told what the page wants. */
	opened() {
		this.#state = this.#opened(this.#state);
	}

	/** The relay sent `data`, which arrived at `at`. */
	message(at: number, data: string | ArrayBuffer) {
		this.#state = this.#heard(this.#state, at, data);
	}

	/** The update the screen was given is decoded and its picture handed to the track: the
	 * page is done with it, and says so. */
	decoded() {
		this.say(TOOK);
	}

	/** The screen cannot go on, for `why`: the session ends. */
	screenEnded(why: string) {
		this.#world.tell({ t: 'log', s: `video: ${why}` });
		this.#state = this.#failed(this.#state, why);
	}

	/** The socket closed: the session is over, for the cause it was ended for, or for the
	 * closing itself. */
	closed() {
		this.#state = this.#closed(this.#state);
	}

	/** The page says `d` to the daemon. With no session open there is no one to tell. */
	say(d: string | ArrayBuffer) {
		this.#state = this.#said(this.#state, d);
	}

	/** The page wants the picture, or does not (nobody can see it). */
	video(on: boolean) {
		if (this.#video === on) return;
		this.#video = on;
		this.say(cmd.video(on));
	}

	/** The page wants the sound, at `audio`. */
	sound(audio: A) {
		this.#sound = { s: 'on', audio };
		this.say(cmd.audio(true));
	}

	#asked(s: State<S>): State<S> {
		switch (s.s) {
			case 'none':
				return { s: 'dialing', socket: this.#world.dial() };
			case 'dialing':
			case 'open':
			case 'ending':
				return s;
		}
	}

	#opened(s: State<S>): State<S> {
		switch (s.s) {
			case 'dialing': {
				const { socket } = s;
				this.#world.tell({ t: 'status', s: 'open' });
				this.#world.send(socket, cmd.video(this.#video));
				this.#world.send(socket, cmd.audio(this.#wantsSound()));
				return { s: 'open', socket };
			}
			case 'none':
			case 'open':
			case 'ending':
				return s;
		}
	}

	#wantsSound(): boolean {
		switch (this.#sound.s) {
			case 'on':
				return true;
			case 'off':
				return false;
		}
	}

	/** What arrives on a socket that is closing, or on none, is nothing. */
	#heard(s: State<S>, at: number, data: string | ArrayBuffer): State<S> {
		switch (s.s) {
			case 'open':
				return this.#read(s, at, readRelay(data));
			case 'none':
			case 'dialing':
			case 'ending':
				return s;
		}
	}

	#read(open: Open<S>, at: number, m: FromRelay): State<S> {
		switch (m.k) {
			case 'bad':
				return this.#end(open, `the relay sent ${m.why}`);
			case 'said':
				return this.#told(open, m.m);
			case 'video':
				this.#world.update(at, m.header, m.picture);
				return open;
			case 'audio':
				if (this.#sound.s === 'on')
					this.#world.sound(this.#sound.audio, m.packet);
				this.#world.send(open.socket, TOOK);
				return open;
			case 'cursor':
				this.#world.tell({ t: 'cursor', buf: m.buf });
				return open;
			case 'clipImage':
				this.#world.tell({ t: 'clipImage', buf: m.buf });
				return open;
		}
	}

	/** A text message: the relay's word that the session ends, or the daemon's for the
	 * page, of which the hello is also the screen's. */
	#told(open: Open<S>, m: ServerMessage): State<S> {
		switch (m.t) {
			case 'close':
				return this.#end(open, m.why);
			case 'hello':
				this.#world.hello(m.codec);
				this.#world.tell(m);
				return open;
			case 'clip':
			case 'mic':
				this.#world.tell(m);
				return open;
		}
	}

	/** A session that is ending keeps its first cause. */
	#failed(s: State<S>, why: string): State<S> {
		switch (s.s) {
			case 'open':
				return this.#end(s, `the screen cannot be shown: ${why}`);
			case 'none':
			case 'dialing':
			case 'ending':
				return s;
		}
	}

	#closed(s: State<S>): State<S> {
		switch (s.s) {
			case 'none':
				return s;
			case 'dialing':
			case 'open':
				return this.#over('the connection to the relay closed');
			case 'ending':
				return this.#over(s.why);
		}
	}

	#said(s: State<S>, d: string | ArrayBuffer): State<S> {
		switch (s.s) {
			case 'open':
				this.#world.send(s.socket, d);
				return s;
			case 'none':
			case 'dialing':
			case 'ending':
				return s;
		}
	}

	/** Ends the open session for `why`: its socket is closed. */
	#end(open: Open<S>, why: string): State<S> {
		this.#world.hangUp(open.socket);
		return { s: 'ending', why };
	}

	/** The session is over, for `why`. */
	#over(why: string): State<S> {
		this.#world.tell({ t: 'status', s: 'closed', why });
		this.#world.screenClosed();
		return { s: 'none' };
	}
}
