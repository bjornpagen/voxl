// Checks the worker's record of the screen (frames.ts) against a fake of the server, the
// socket and WebCodecs, over many seeded runs:
//   node web/src/frames.test.ts
// The fake server sends frames in order, each a part for some of its stripes, in any
// order: a whole keyframe at each start of the stream (a hello), after each request for
// one, and now and then unasked, now and then of another codec; a hello now and then
// between two parts of a frame, as a failed display switch does, or between frames with
// a new cut and codec, as a rebuild does. The socket closes now and then, between any two
// messages, and opens again. The fake browser runs the decoders' outputs and errors as
// tasks in the order queued, between the socket's messages in any order. A decoder puts
// out what it took in order; it fails or is reclaimed at any time, and its error closes
// it; now and then it refuses a part (`decode` throws). One opened for a codec the browser
// lacks fails with NotSupportedError; a codec it cannot configure throws from `open`, and
// a keyframe with no SPS from `codec`: a stream of one of them now and then, and a
// keyframe now and then. A task queued before the record closed a decoder may still run.
//
// What it checks are the fake's own facts: what each decoder was given, put out, told to
// close, and failed with. After every step:
// - the frames shown (acknowledged) are exactly the longest run, from the oldest, of the
//   frames sent on this socket whose parts have all arrived and none of whose parts is in
//   a decoder: given to one, and neither put out by it nor ended with it (closed by the
//   record, or its error run). Each once, in order, at the times its parts arrived; none
//   of a closed socket;
// - the requests for a keyframe are as many as the decoders that failed on the record:
//   parts they refused, and errors other than NotSupportedError;
// - a part goes to a decoder exactly when its stripe has one live, opened on a keyframe
//   since the last hello; a keyframe goes to a decoder of its codec, opened only when the
//   stripe has no live one of it, and the decoder it replaces is closed; a stripe that
//   could not decode a codec opens nothing more until the next hello;
// - the record never decodes into, or closes, a decoder that is closed.
// Once a whole keyframe has arrived, every stripe nothing has happened to since its part
// arrived decodes with the decoder its part went to. At the end of a run, once every part
// has arrived and every decoder has put out what it holds, every frame has been shown;
// once the socket closes, every decoder has been closed once or failed, every picture has
// been let go once (shown, or closed), none put out before a hello has been shown, and
// every part that arrived has either come out of a decoder into its frame or been counted
// dropped.
import { Frames, type Frame, type Head, type World } from './frames.ts';

/** The server cuts a screen into at most 16 stripes (src/video.rs). */
const STRIPES_MOST = 16;
const RUNS = 20_000;
/** Steps in a run before it is drained. */
const STEPS = 300;

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

/** Codecs the fake browser decodes; one it lacks (NotSupportedError); one it cannot
 * configure; and a keyframe that names none. */
const GOOD = ['avc1.640034', 'avc1.42e01f'];
const LACKED = 'avc1.lacked';
const INVALID = 'avc1.invalid';
const NO_SPS = 'no SPS';

interface Bytes {
	codec: string;
}

interface Dec {
	stripe: number;
	codec: string;
	/** Neither closed by the record nor by its own error. */
	open: boolean;
	closes: number;
	/** Its error ran. */
	failed: boolean;
	/** Frames of the parts it took and has not put out, oldest first. */
	held: number[];
	/** The hellos before it was opened. */
	stream: number;
}

interface Picture {
	frame: number;
	lets: number;
	shown: boolean;
	/** The hellos before the record had it. */
	hellos: number;
}

interface Sent {
	id: number;
	parts: number;
	came: number;
	first: number;
	last: number;
	/** Its parts in a decoder, by stripe. */
	held: Map<number, Dec>;
	/** A part of it was in a decoder that ended. */
	lost: boolean;
	whole: boolean;
	/** For a whole keyframe: per stripe, what had happened to it when its part arrived,
	 * and the decoder that part went to. */
	calm: Map<number, { happened: number; into: Dec | null }>;
}

type Message =
	| { t: 'part'; head: Head; bytes: Bytes; whole: boolean }
	| { t: 'hello'; count: number };

type Task =
	| { t: 'output'; dec: Dec; frame: number }
	| { t: 'error'; dec: Dec; name: string };

/** How often each clause was reached, over every run. */
const reached = {
	opened: 0,
	kept: 0,
	replaced: 0,
	keyAfterFailure: 0,
	configureThrew: 0,
	noSps: 0,
	refusedWhileDecoding: 0,
	notSupported: 0,
	refusedPart: 0,
	failed: 0,
	lateError: 0,
	lateOutput: 0,
	droppedWaiting: 0,
	droppedRefused: 0,
	helloInFrame: 0,
	closedWithFrames: 0,
	wholeChecked: 0,
	shownAfterLoss: 0,
};

function run(seed: number) {
	const draw = random(seed);
	let step = 0;
	// The record catches what the fake browser throws, so a failure found inside one of
	// its calls is kept, and thrown again after the step.
	let failure: Error | null = null;
	const fail = (what: string): never => {
		failure ??= new Error(`seed ${seed}, step ${step}: ${what}`);
		throw failure;
	};

	// The server.
	let nextId = draw(2 ** 32);
	let count = 1 + draw(STRIPES_MOST);
	let codec = GOOD[0];
	let keyOwed = true;
	let socket: Message[] = [{ t: 'hello', count }];

	// The browser, and what it has seen.
	const tasks: Task[] = [];
	const decs: Dec[] = [];
	const pictures: Picture[] = [];
	let hellos = 0;
	let asks = 0;
	let failures = 0;
	let arrived = 0;
	let accepted = 0;
	/** Frames sent on this socket, oldest first, and how many of them were shown. */
	let sent: Sent[] = [];
	let shown = 0;
	/** Per stripe since the last hello: it could not decode its codec; it failed. */
	let refused: boolean[] = [];
	let broke: boolean[] = [];
	/** Per stripe: hellos and failures. */
	let happened: number[] = [];
	/** The part being delivered, what the record did with it, and what it did to it. */
	let delivering: Extract<Message, { t: 'part' }> | null = null;
	let decodedInto: Dec | null = null;
	let opened: Dec | null = null;
	let threw = false;

	/** The decoder of `stripe` opened last since the last hello, if it is open. */
	const live = (stripe: number) => {
		for (let k = decs.length - 1; k >= 0; k--) {
			const d = decs[k];
			if (d.stripe === stripe && d.stream === hellos)
				return d.open ? d : undefined;
		}
		return undefined;
	};
	/** A decoder is gone: the parts it held never come out. */
	const gone = (dec: Dec) => {
		dec.open = false;
		dec.held.length = 0;
		for (const s of sent)
			if (s.held.get(dec.stripe) === dec) {
				s.held.delete(dec.stripe);
				s.lost = true;
			}
	};

	const world: World<Dec, Picture, Bytes> = {
		codec(key) {
			if (key.codec === NO_SPS) throw new TypeError('no SPS');
			return key.codec;
		},
		open(stripe, c) {
			const part = delivering ?? fail('a decoder opened outside a part');
			if (!part.head.key || part.head.stripe !== stripe)
				fail(`stripe ${stripe} opened a decoder on another part`);
			if (refused[stripe])
				fail(`stripe ${stripe} opened a decoder after refusing`);
			if (live(stripe)?.codec === c)
				fail(`stripe ${stripe} opened a second decoder of ${c}`);
			if (c === INVALID) throw new TypeError(`${c} is not a codec`);
			const dec: Dec = {
				stripe,
				codec: c,
				open: true,
				closes: 0,
				failed: false,
				held: [],
				stream: hellos,
			};
			decs.push(dec);
			opened = dec;
			if (c === LACKED)
				tasks.push({ t: 'error', dec, name: 'NotSupportedError' });
			return dec;
		},
		decode(dec, key, frame, bytes) {
			const part = delivering ?? fail('a decode outside a part');
			if (!dec.open) fail('a decode into a closed decoder');
			if (
				key !== part.head.key ||
				frame !== part.head.frame ||
				dec.stripe !== part.head.stripe ||
				bytes !== part.bytes
			)
				fail('a decode of another part than the one that arrived');
			decodedInto = dec;
			if (!draw(200)) {
				threw = true;
				throw new DOMException('refused', 'DataError');
			}
			dec.held.push(frame);
		},
		close(dec) {
			if (!dec.open) fail('a decoder closed twice, or after its error');
			dec.closes++;
			gone(dec);
		},
		ask() {
			asks++;
		},
		log() {},
		letGo(p) {
			p.lets++;
		},
		show(f: Frame<Picture>) {
			const s = sent[shown] ?? fail(`frame ${f.id} shown, of a closed socket`);
			if (f.id !== s.id) fail(`frame ${f.id} shown where ${s.id} was next`);
			if (f.first !== s.first || f.last !== s.last)
				fail(`frame ${f.id} arrived from ${s.first} to ${s.last}`);
			for (const p of f.pictures) {
				if (p.frame !== f.id)
					fail(`frame ${f.id} showed a picture of ${p.frame}`);
				if (p.hellos !== hellos)
					fail(`frame ${f.id} showed a picture from before a hello`);
				p.lets++;
				p.shown = true;
			}
			if (s.lost) reached.shownAfterLoss++;
			shown++;
		},
	};
	const frames = new Frames(world);

	/** What the server sends next. */
	const produce = () => {
		if (!draw(30)) {
			count = 1 + draw(STRIPES_MOST);
			const c = draw(20);
			codec = [LACKED, INVALID, NO_SPS][c] ?? GOOD[draw(2)];
			socket.push({ t: 'hello', count });
			keyOwed = true;
			return;
		}
		const whole = keyOwed || !draw(20);
		if (whole && !keyOwed && GOOD.includes(codec) && !draw(4))
			codec = GOOD[1 - GOOD.indexOf(codec)];
		keyOwed = false;
		const stripes = Array.from({ length: count }, (_, i) => i).filter(
			() => whole || !draw(2)
		);
		if (!stripes.length) stripes.push(draw(count));
		for (let i = stripes.length - 1; i > 0; i--) {
			const j = draw(i + 1);
			[stripes[i], stripes[j]] = [stripes[j], stripes[i]];
		}
		const frame = nextId;
		nextId = (nextId + 1) >>> 0;
		stripes.forEach((stripe, k) => {
			if (k && !draw(50)) {
				socket.push({ t: 'hello', count });
				keyOwed = true;
			}
			const key = whole || !draw(20);
			const head = { key, frame, stripe, parts: stripes.length };
			// Now and then a keyframe of a codec the browser cannot decode, mid-stream.
			const bad = key && !draw(100);
			const bytes = { codec: bad ? [NO_SPS, INVALID, LACKED][draw(3)] : codec };
			socket.push({ t: 'part', head, bytes, whole });
		});
	};

	const due = () => {
		let n = 0;
		while (
			n < sent.length &&
			sent[n].came === sent[n].parts &&
			!sent[n].held.size
		)
			n++;
		return n;
	};

	const check = () => {
		if (failure) throw failure;
		if (shown !== due()) fail(`${shown} frames shown where ${due()} are due`);
		if (frames.waiting !== sent.length - shown)
			fail(`${frames.waiting} frames waiting, not ${sent.length - shown}`);
		if (asks !== failures)
			fail(`${asks} keyframes asked for after ${failures} failures`);
		if (frames.codec !== (live(0)?.codec ?? null))
			fail(`the first stripe reads as decoding ${frames.codec}`);
	};

	const hello = (n: number) => {
		hellos++;
		refused = Array.from({ length: n }, () => false);
		broke = Array.from({ length: n }, () => false);
		happened = Array.from({ length: n }, (_, i) => (happened[i] ?? 0) + 1);
		frames.hello(n);
	};

	const part = (m: Extract<Message, { t: 'part' }>) => {
		const { head, bytes } = m;
		const i = head.stripe;
		let s = sent.find((x) => x.id === head.frame);
		const isFirst = !s;
		if (!s) {
			s = {
				id: head.frame,
				parts: head.parts,
				came: 0,
				first: step,
				last: step,
				held: new Map(),
				lost: false,
				whole: m.whole,
				calm: new Map(),
			};
			sent.push(s);
		}
		s.came++;
		s.last = step;
		arrived++;
		const before = live(i);
		const calm = happened[i];
		delivering = m;
		decodedInto = null;
		opened = null;
		threw = false;
		const first = frames.part(step, head, bytes);
		delivering = null;
		if (first !== isFirst)
			fail(`a part of ${head.frame} read as its first: ${first}`);
		const into = decodedInto as Dec | null;
		if (into && !threw) s.held.set(i, into);
		// Where the part should have gone.
		if (refused[i]) {
			if (into) fail(`stripe ${i} decoded after it could not`);
			reached.droppedRefused++;
		} else if (head.key) {
			if (bytes.codec === NO_SPS || bytes.codec === INVALID) {
				if (into) fail(`a keyframe of ${bytes.codec} decoded`);
				if (before?.open)
					fail(`stripe ${i} kept its decoder for a codec it cannot read`);
				reached[bytes.codec === NO_SPS ? 'noSps' : 'configureThrew']++;
				if (before) reached.refusedWhileDecoding++;
				refused[i] = true;
			} else {
				if (!into || into.codec !== bytes.codec)
					fail(`a keyframe of ${bytes.codec} went to ${into?.codec}`);
				if (before?.codec === bytes.codec) {
					if (into !== before)
						fail(`stripe ${i} left its decoder of ${bytes.codec}`);
					reached.kept++;
				} else {
					if (into !== opened)
						fail(`a keyframe of a new codec went to an old decoder`);
					if (before?.open) fail(`stripe ${i} left its old decoder open`);
					reached[before ? 'replaced' : 'opened']++;
					if (broke[i]) reached.keyAfterFailure++;
					broke[i] = false;
				}
			}
		} else if (before) {
			if (into !== before) fail(`a part on stripe ${i} missed its decoder`);
		} else {
			if (into) fail(`a part on stripe ${i} went to a decoder it has not`);
			reached.droppedWaiting++;
		}
		if (threw) {
			if (into?.open) fail(`stripe ${i} kept a decoder that refused a part`);
			failures++;
			happened[i]++;
			broke[i] = true;
			reached.refusedPart++;
		}
		if (s.whole) {
			s.calm.set(i, { happened: calm, into });
			if (s.came === s.parts)
				for (const [stripe, c] of s.calm) {
					if (happened[stripe] !== c.happened || refused[stripe]) continue;
					if (!c.into || live(stripe) !== c.into)
						fail(`stripe ${stripe} decodes nothing after a whole keyframe`);
					reached.wholeChecked++;
				}
		}
	};

	const runTask = (task: Task) => {
		const { dec } = task;
		if (task.t === 'output') {
			const p: Picture = { frame: task.frame, lets: 0, shown: false, hellos };
			pictures.push(p);
			const late = !dec.open;
			if (!late) sent.find((s) => s.id === task.frame)?.held.delete(dec.stripe);
			frames.output(dec.stripe, task.frame, p);
			if (late) {
				if (p.lets !== 1)
					fail(`a picture of ${task.frame} from a closed decoder kept`);
				reached.lateOutput++;
			} else if (p.lets && !p.shown)
				fail(`a picture of ${task.frame} let go as it came out`);
			else accepted++;
			return;
		}
		if (!dec.open) {
			// One error a decoder; and one queued before the record closed it may run.
			if (dec.failed) return;
			reached.lateError++;
			frames.error(dec, task.name, 'late');
			return;
		}
		dec.failed = true;
		gone(dec);
		if (task.name === 'NotSupportedError') {
			refused[dec.stripe] = true;
			reached.notSupported++;
		} else {
			failures++;
			broke[dec.stripe] = true;
			reached.failed++;
		}
		happened[dec.stripe]++;
		frames.error(dec, task.name, task.name);
	};

	const deliver = () => {
		if (!socket.length) produce();
		const m = socket.shift()!;
		if (m.t === 'hello') {
			const last = sent.at(-1);
			if (last && last.came < last.parts) reached.helloInFrame++;
			hello(m.count);
		} else part(m);
	};

	for (step = 1; step <= STEPS; step++) {
		const roll = draw(100);
		if (roll < 40) deliver();
		else if (roll < 65) {
			const busy = decs.filter((d) => d.open && d.held.length);
			if (busy.length) {
				const dec = busy[draw(busy.length)];
				tasks.push({ t: 'output', dec, frame: dec.held.shift()! });
			}
		} else if (roll < 95) {
			const task = tasks.shift();
			if (task) runTask(task);
		} else if (roll < 99) {
			// A decoder fails, or the browser reclaims it.
			const open = decs.filter((d) => d.open);
			if (open.length) {
				const dec = open[draw(open.length)];
				tasks.push({
					t: 'error',
					dec,
					name: draw(2) ? 'EncodingError' : 'QuotaExceededError',
				});
			}
		} else {
			// The socket closes, and another opens.
			if (sent.length > shown) reached.closedWithFrames++;
			sent = [];
			shown = 0;
			frames.closed();
			count = 1 + draw(STRIPES_MOST);
			socket = [{ t: 'hello', count }];
			keyOwed = true;
		}
		check();
	}
	// What was sent arrives, and every decoder puts out what it holds.
	while (socket.length) {
		step++;
		deliver();
		check();
	}
	for (;;) {
		for (const d of decs)
			while (d.open && d.held.length)
				tasks.push({ t: 'output', dec: d, frame: d.held.shift()! });
		const task = tasks.shift();
		if (!task) break;
		step++;
		runTask(task);
		check();
	}
	if (shown !== sent.length) fail(`${sent.length - shown} frames never shown`);
	// The socket closes.
	sent = [];
	shown = 0;
	frames.closed();
	while (tasks.length) runTask(tasks.shift()!);
	check();
	for (const [k, d] of decs.entries())
		if (d.open || d.closes + Number(d.failed) !== 1)
			fail(`decoder ${k} closed ${d.closes} times, failed: ${d.failed}`);
	for (const p of pictures)
		if (p.lets !== 1) fail(`a picture of ${p.frame} let go ${p.lets} times`);
	if (arrived !== accepted + frames.dropped)
		fail(
			`${arrived} parts arrived; ${accepted} came out, ${frames.dropped} dropped`
		);
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
