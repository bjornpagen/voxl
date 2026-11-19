// Checks the worker's record of its session with the relay (session.ts) against a fake
// socket, page, screen and audio decoder, over many seeded runs:
//   node web/src/session.test.ts
// A run draws, step by step: the page asking for a session; the socket opening; the relay
// sending a message (an update, a packet, a cursor's or a clipboard's image, a hello, a
// clipboard's text, a microphone word, its own close, or something that is none of its
// messages), also while the socket is closing; the screen decoding an update it was given,
// or failing; the socket closing; the page saying something to the daemon, turning the
// picture on or off, or asking for the sound.
//
// The oracle is what the fake knows: whether its socket is none, connecting, open or
// closing, what the page last asked for, and which updates the screen holds. After every
// step, what the record did is exactly this:
// - it dials only with no socket, and at the socket's opening tells the page and sends
//   the daemon the page's wants, the picture's then the sound's;
// - it sends only on an open socket, the one it dialed: the page's own words as they are and in order, the
//   picture's want when it changes, the sound's when it is asked for, and its word that
//   it took a frame: once for each packet, at once, whether or not the sound is wanted,
//   and once for each update, when the screen has decoded it;
// - an update goes to the screen with its arrival and its header, a packet to the audio
//   decoder the page gave, a hello to the screen and the page, every other message of
//   the relay's to the page as it is (an image's own buffer);
// - the relay's close, a message that is none of its own (told as the reader of the
//   relay's messages words it), and a screen that fails each close the socket, once, and
//   nothing that arrives while it closes is acted on;
// - when the socket has closed the page is told the first cause, or, with none, that the
//   connection closed, and the screen's session is closed, once.
import {
	Bin,
	cmd,
	CODINGS,
	readRelay,
	VIDEO_HEADER_BYTES,
} from './protocol.ts';
import { Session, type World } from './session.ts';

const RUNS = 20_000;
const STEPS = 120;

/** A small seeded generator (mulberry32): runs repeat from their seed alone. */
function random(seed: number) {
	let a = seed >>> 0;
	return (n: number) => {
		a = (a + 0x6d2b79f5) >>> 0;
		let t = a;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) % n;
	};
}

/** A socket the fake opened, and where the sound goes: each an object of its own, so
 * what the record does names the one it was given. */
interface Socket {
	n: number;
}
interface Audio {
	n: number;
}

/** One thing the record did, as the fake world saw it. */
type Did = readonly [string, ...unknown[]];

/** Whether two things the record did are the same: a buffer is itself, bytes are their
 * values, anything else its parts by their names. */
function same(a: unknown, b: unknown): boolean {
	if (a instanceof ArrayBuffer || b instanceof ArrayBuffer) return a === b;
	if (a instanceof Uint8Array && b instanceof Uint8Array)
		return a.join() === b.join();
	if (typeof a !== 'object' || typeof b !== 'object' || !a || !b)
		return a === b;
	const [pa, pb] = [
		Object.entries(a),
		new Map<string, unknown>(Object.entries(b)),
	];
	return (
		pa.length === pb.size &&
		pa.every(([k, v]) => pb.has(k) && same(v, pb.get(k)))
	);
}

/** How often each clause was reached, over every run. */
const reached = {
	dialed: 0,
	opened: 0,
	update: 0,
	packetHeard: 0,
	packetUnheard: 0,
	image: 0,
	hello: 0,
	word: 0,
	relayClosed: 0,
	bad: 0,
	shown: 0,
	screenFailed: 0,
	whileClosing: 0,
	secondCause: 0,
	closedForCause: 0,
	closedItself: 0,
	saidOpen: 0,
	saidUnheard: 0,
	videoChanged: 0,
};

function run(seed: number) {
	const draw = random(seed);
	let step = 0;
	const fail = (why: string): never => {
		throw new Error(`seed ${seed}, step ${step}: ${why}`);
	};
	const bytes = (n: number) => Uint8Array.from({ length: n }, () => draw(256));
	const text = () => `${draw(1000)}`;

	let did: Did[] = [];
	/** The socket the fake opened last. */
	let socket: Socket = { n: 0 };
	const world: World<Socket, Audio> = {
		dial() {
			socket = { n: step };
			did.push(['dial']);
			return socket;
		},
		send: (on, d) => did.push(['send', on, d]),
		hangUp: (on) => did.push(['hangUp', on]),
		tell: (m) => did.push(['tell', m]),
		hello: (codec) => did.push(['hello', codec]),
		update: (at, header, picture) => did.push(['update', at, header, picture]),
		screenClosed: () => did.push(['screenClosed']),
		sound: (audio, packet) => did.push(['sound', audio, packet]),
	};
	const session = new Session(world);
	const took = new Uint8Array([Bin.took]);

	/** The fake's facts: its socket, what the page last asked for, the updates the screen
	 * holds unshown, and why the socket is closing. */
	let sock: 'none' | 'connecting' | 'open' | 'closing' = 'none';
	let wantsVideo = true;
	let audio: Audio | null = null;
	let unshown = 0;
	let cause: string | null = null;

	/** The record must have done exactly `expected`, in order. What it sent the relay in
	 * binary is compared by its bytes: its own word is a buffer of its own. */
	const must = (what: string, expected: Did[]) => {
		const got = did.map(
			(d): Did =>
				d[0] === 'send' && d[2] instanceof ArrayBuffer
					? ['send', d[1], new Uint8Array(d[2])]
					: d
		);
		did = [];
		if (
			got.length !== expected.length ||
			!got.every((d, i) => same(d, expected[i]))
		)
			fail(
				`${what} with the socket ${sock}: did ${JSON.stringify(got)}, not ${JSON.stringify(expected)}`
			);
	};
	/** The record's word that it took a frame: a message of the one byte. */
	const tookSent = (): Did => ['send', socket, took];

	/** The relay sends a drawn message. */
	const relay = () => {
		const live = sock === 'open';
		if (!live) reached.whileClosing++;
		const at = step + draw(1000) / 1000;
		const kind = draw(9);
		if (kind <= 1) {
			const picture = bytes(1 + draw(30));
			const header = {
				coding: CODINGS[draw(CODINGS.length)],
				version: draw(2 ** 32),
				pipe: draw(2 ** 32),
			};
			const m = new Uint8Array(VIDEO_HEADER_BYTES + picture.length);
			const view = new DataView(m.buffer);
			view.setUint8(0, Bin.video);
			view.setUint8(1, CODINGS.indexOf(header.coding));
			view.setUint32(2, header.version);
			view.setUint32(6, header.pipe);
			m.set(picture, VIDEO_HEADER_BYTES);
			session.message(at, m.buffer);
			if (!live) return must('an update', []);
			unshown++;
			reached.update++;
			return must('an update', [['update', at, header, picture]]);
		}
		if (kind <= 3) {
			const packet = bytes(1 + draw(30));
			session.message(at, Uint8Array.from([Bin.audio, ...packet]).buffer);
			if (!live) return must('a packet', []);
			if (audio) reached.packetHeard++;
			else reached.packetUnheard++;
			return must('a packet', [
				...(audio ? [['sound', audio, packet] as const] : []),
				tookSent(),
			]);
		}
		if (kind === 4) {
			const t = draw(2) ? 'cursor' : 'clipImage';
			const buf = Uint8Array.from([Bin[t], ...bytes(draw(30))]).buffer;
			session.message(at, buf);
			if (live) reached.image++;
			return must('an image', live ? [['tell', { t, buf }]] : []);
		}
		if (kind === 5) {
			const hello = {
				t: 'hello',
				codec: `av01.0.${String(draw(20)).padStart(2, '0')}M.08`,
				files: { dir: text(), root: text() },
				resize: { scale: (1 + draw(300)) / 100 },
			} as const;
			session.message(at, JSON.stringify(hello));
			if (live) reached.hello++;
			return must(
				'a hello',
				live
					? [
							['hello', hello.codec],
							['tell', hello],
						]
					: []
			);
		}
		if (kind === 6) {
			const word = draw(2)
				? ({ t: 'clip', d: text() } as const)
				: ({ t: 'mic', on: draw(2) === 1 } as const);
			session.message(at, JSON.stringify(word));
			if (live) reached.word++;
			return must('a word', live ? [['tell', word]] : []);
		}
		// The relay's close, or none of its messages: the session ends for it.
		if (kind === 7) {
			const why = text();
			session.message(at, JSON.stringify({ t: 'close', why }));
			if (live) {
				cause = why;
				reached.relayClosed++;
			}
		} else {
			const none = [
				'not json',
				JSON.stringify({ t: 'lane' }),
				new ArrayBuffer(0),
				Uint8Array.from([Bin.took]).buffer,
				Uint8Array.from([Bin.video, 4, 0]).buffer,
				new Uint8Array(VIDEO_HEADER_BYTES).fill(Bin.video, 0, 1).buffer,
			][draw(6)];
			const read = readRelay(none);
			if (read.k !== 'bad') return fail('the fake drew a message of the relay');
			session.message(at, none);
			if (live) {
				cause = `the relay sent ${read.why}`;
				reached.bad++;
			}
		}
		must('a message that ends the session', live ? [['hangUp', socket]] : []);
		if (live) sock = 'closing';
	};

	for (step = 1; step <= STEPS; step++) {
		const roll = draw(100);
		if (roll < 12) {
			session.connect();
			if (sock === 'none') {
				sock = 'connecting';
				reached.dialed++;
				must('the page asking', [['dial']]);
			} else must('the page asking', []);
		} else if (roll < 24) {
			if (sock !== 'connecting') continue;
			session.opened();
			sock = 'open';
			reached.opened++;
			must('the socket opening', [
				['tell', { t: 'status', s: 'open' }],
				['send', socket, cmd.video(wantsVideo)],
				['send', socket, cmd.audio(audio !== null)],
			]);
		} else if (roll < 60) {
			if (sock === 'open' || sock === 'closing') relay();
		} else if (roll < 72) {
			if (!unshown) continue;
			unshown--;
			session.decoded();
			if (sock === 'open') reached.shown++;
			must('the screen decoding', sock === 'open' ? [tookSent()] : []);
		} else if (roll < 74) {
			const why = text();
			const live = sock === 'open';
			if (sock === 'closing') reached.secondCause++;
			session.screenEnded(why);
			must('the screen failing', [
				['tell', { t: 'log', s: `video: ${why}` }],
				...(live ? [['hangUp', socket] as const] : []),
			]);
			if (live) {
				cause = `the screen cannot be shown: ${why}`;
				sock = 'closing';
				reached.screenFailed++;
			}
		} else if (roll < 80) {
			if (sock === 'none') continue;
			session.closed();
			if (cause === null) reached.closedItself++;
			else reached.closedForCause++;
			const why = cause ?? 'the connection to the relay closed';
			sock = 'none';
			cause = null;
			unshown = 0;
			must('the socket closing', [
				['tell', { t: 'status', s: 'closed', why }],
				['screenClosed'],
			]);
		} else if (roll < 90) {
			const d = draw(2)
				? cmd.key(draw(1000), 'down')
				: bytes(1 + draw(8)).buffer;
			session.say(d);
			if (sock === 'open') reached.saidOpen++;
			else reached.saidUnheard++;
			must(
				'the page saying',
				sock === 'open'
					? [['send', socket, typeof d === 'string' ? d : new Uint8Array(d)]]
					: []
			);
		} else if (roll < 96) {
			const on = draw(2) === 1;
			const changed = on !== wantsVideo;
			wantsVideo = on;
			session.video(on);
			if (changed && sock === 'open') reached.videoChanged++;
			must(
				'the page wanting the picture',
				changed && sock === 'open' ? [['send', socket, cmd.video(on)]] : []
			);
		} else {
			audio = { n: step };
			session.sound(audio);
			must(
				'the page wanting the sound',
				sock === 'open' ? [['send', socket, cmd.audio(true)]] : []
			);
		}
	}
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
