// Checks the worker's record of the screen (frames.ts) against a fake of the relay and
// WebCodecs, over many seeded runs:
//   node web/src/frames.test.ts
// The fake relay sends updates: a keyframe at each start of the stream (a hello), after
// each request for one, and now and then unasked, now and then of another codec; a hello
// now and then with a new codec, as a rebuild does. Now and then it drops an update (it
// missed its deadline, or refers to one that did), so the updates come with gaps. The
// session ends now and then, between any two messages, and another starts, now and then
// with an update ahead of its hello. The fake browser runs the decoders' outputs and
// errors as tasks in the order queued, between the socket's messages in any order. A
// decoder puts out what it took in order; it fails or is reclaimed at any time, and its
// error closes it; now and then it refuses an update (`decode` throws). One opened for a
// codec the browser lacks fails with NotSupportedError; a codec it cannot configure throws
// from `open`, and a keyframe with no SPS from `codec`: a stream of one of them now and
// then, and a keyframe now and then. A task queued before the record closed a decoder may
// still run.
//
// What it checks are the fake's own facts: what each decoder was given, put out, told to
// close, and failed with. After every step:
// - a picture is shown exactly when it comes out of the decoder that is open (the live
//   one, opened since the last hello), at once; every other picture is let go at once;
// - the requests for a keyframe are as many as the decoders that failed on the record:
//   updates they refused, and errors other than NotSupportedError; a gap asks for none;
// - an update goes to a decoder exactly when a stream has one live, opened on a keyframe
//   since the last hello; a keyframe goes to a decoder of its codec, opened only when there
//   is no live one of it, and the decoder it replaces is closed; a stream that could not
//   decode a codec opens nothing more until the next hello; before the first hello and
//   after the session ends, nothing is decoded;
// - the record never decodes into, or closes, a decoder that is closed.
// Once the session ends, every decoder has been closed once or failed, every picture has
// been let go once (shown, or closed), and every update that arrived has either come out
// of a decoder or been counted dropped.
import { Frames, type World } from './frames.ts';

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
	codec: string;
	/** Neither closed by the record nor by its own error. */
	open: boolean;
	closes: number;
	/** Its error ran. */
	failed: boolean;
	/** Arrival times of the updates it took and has not put out, oldest first. */
	held: number[];
}

interface Picture {
	/** The arrival time of the update it decodes. */
	at: number;
	lets: number;
	shown: boolean;
}

type Message = { t: 'part'; key: boolean; bytes: Bytes } | { t: 'hello' };

type Task =
	| { t: 'output'; dec: Dec; at: number }
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
	droppedNoStream: 0,
	endedHolding: 0,
	shown: 0,
	gap: 0,
};

/** A decoder is gone: the updates it held never come out. */
function gone(dec: Dec) {
	dec.open = false;
	dec.held.length = 0;
}

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

	// The relay.
	let codec = GOOD[0];
	let keyOwed = true;
	let socket: Message[] = [{ t: 'hello' }];

	// The browser, and what it has seen.
	const tasks: Task[] = [];
	const decs: Dec[] = [];
	const pictures: Picture[] = [];
	let asks = 0;
	let failures = 0;
	let arrived = 0;
	let accepted = 0;
	/** Whether a hello came on this session; the stream could not decode its codec; its
	 * decoder failed since its last keyframe. */
	let streaming = false;
	let refused = false;
	let broke = false;
	/** The decoder of this stream opened last, if any. */
	let latest: Dec | null = null;
	const live = () => (latest?.open ? latest : null);
	/** The update being delivered, what the record did with it, and what it did to it. */
	let delivering: Extract<Message, { t: 'part' }> | null = null;
	let decodedInto: Dec | null = null;
	let opened: Dec | null = null;
	let threw = false;
	/** The picture coming out of a decoder now. */
	let outputting: { dec: Dec; picture: Picture } | null = null;

	const world: World<Dec, Picture, Bytes> = {
		codec(key) {
			if (key.codec === NO_SPS) throw new TypeError('no SPS');
			return key.codec;
		},
		open(c) {
			const part = delivering ?? fail('a decoder opened outside an update');
			if (!part.key) fail('a decoder opened on a delta');
			if (refused) fail('a decoder opened after the stream refused its codec');
			if (live()?.codec === c) fail(`a second decoder of ${c}`);
			if (c === INVALID) throw new TypeError(`${c} is not a codec`);
			const dec: Dec = {
				codec: c,
				open: true,
				closes: 0,
				failed: false,
				held: [],
			};
			decs.push(dec);
			opened = dec;
			latest = dec;
			if (c === LACKED)
				tasks.push({ t: 'error', dec, name: 'NotSupportedError' });
			return dec;
		},
		decode(dec, key, at, bytes) {
			const part = delivering ?? fail('a decode outside an update');
			if (!dec.open) fail('a decode into a closed decoder');
			if (key !== part.key || at !== step || bytes !== part.bytes)
				fail('a decode of another update than the one that arrived');
			decodedInto = dec;
			if (!draw(200)) {
				threw = true;
				throw new DOMException('refused', 'DataError');
			}
			dec.held.push(at);
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
		show(p) {
			const o = outputting ?? fail('a picture shown outside an output');
			if (o.picture !== p) fail('another picture shown than the one put out');
			p.lets++;
			p.shown = true;
			reached.shown++;
		},
	};
	const frames = new Frames(world);

	/** What the relay sends next. */
	const produce = () => {
		if (!draw(30)) {
			const c = draw(20);
			codec = [LACKED, INVALID, NO_SPS][c] ?? GOOD[draw(2)];
			socket.push({ t: 'hello' });
			keyOwed = true;
			return;
		}
		const key = keyOwed || !draw(20);
		if (key && !keyOwed && GOOD.includes(codec) && !draw(4))
			codec = GOOD[1 - GOOD.indexOf(codec)];
		// Now and then the relay drops a delta: the next update follows a gap in the
		// numbers, which is no failure.
		if (!key && !draw(15)) {
			reached.gap++;
			return;
		}
		keyOwed = false;
		// Now and then a keyframe of a codec the browser cannot decode, mid-stream.
		const bad = key && !draw(100);
		const bytes = { codec: bad ? [NO_SPS, INVALID, LACKED][draw(3)] : codec };
		socket.push({ t: 'part', key, bytes });
	};

	const check = () => {
		if (failure) throw failure;
		if (asks !== failures)
			fail(`${asks} keyframes asked for after ${failures} failures`);
		if (frames.codec !== (live()?.codec ?? null))
			fail(`the screen reads as decoding ${frames.codec}`);
	};

	const hello = () => {
		streaming = true;
		refused = false;
		broke = false;
		latest = null;
		frames.hello();
	};

	/** The session ends, and another starts, now and then with an update ahead of its
	 * hello. */
	const reconnect = () => {
		if (decs.some((d) => d.open && d.held.length)) reached.endedHolding++;
		streaming = false;
		latest = null;
		frames.closed();
		socket = [{ t: 'hello' }];
		keyOwed = true;
		if (!draw(10)) socket.unshift({ t: 'part', key: true, bytes: { codec } });
	};

	const part = (m: Extract<Message, { t: 'part' }>) => {
		const { bytes, key } = m;
		arrived++;
		const before = live();
		delivering = m;
		decodedInto = null;
		opened = null;
		threw = false;
		frames.part(step, key, bytes);
		delivering = null;
		const into = decodedInto as Dec | null;
		// Where the update should have gone.
		if (!streaming) {
			if (into || opened) fail('an update decoded before a hello');
			reached.droppedNoStream++;
		} else if (refused) {
			if (into) fail('a stream decoded after it could not');
			reached.droppedRefused++;
		} else if (key) {
			if (bytes.codec === NO_SPS || bytes.codec === INVALID) {
				if (into) fail(`a keyframe of ${bytes.codec} decoded`);
				if (before?.open)
					fail('a stream kept its decoder for a codec it cannot read');
				reached[bytes.codec === NO_SPS ? 'noSps' : 'configureThrew']++;
				if (before) reached.refusedWhileDecoding++;
				refused = true;
			} else {
				if (!into || into.codec !== bytes.codec)
					fail(`a keyframe of ${bytes.codec} went to ${into?.codec}`);
				if (before?.codec === bytes.codec) {
					if (into !== before)
						fail(`the stream left its decoder of ${bytes.codec}`);
					reached.kept++;
				} else {
					if (into !== opened)
						fail('a keyframe of a new codec went to an old decoder');
					if (before?.open) fail('the stream left its old decoder open');
					reached[before ? 'replaced' : 'opened']++;
					if (broke) reached.keyAfterFailure++;
					broke = false;
				}
			}
		} else if (before) {
			if (into !== before) fail('an update missed its decoder');
		} else {
			if (into) fail('an update went to a decoder the stream has not');
			reached.droppedWaiting++;
		}
		if (threw) {
			if (into?.open) fail('the stream kept a decoder that refused an update');
			failures++;
			broke = true;
			reached.refusedPart++;
		}
	};

	const runTask = (task: Task) => {
		const { dec } = task;
		if (task.t === 'output') {
			const p: Picture = { at: task.at, lets: 0, shown: false };
			pictures.push(p);
			const late = !dec.open;
			outputting = { dec, picture: p };
			frames.output(dec, p);
			outputting = null;
			if (p.lets !== 1)
				fail(`a picture of the update of ${task.at} let go ${p.lets} times`);
			if (late) {
				if (p.shown) fail('a picture from a closed decoder shown');
				reached.lateOutput++;
			} else if (!p.shown) fail('a picture of the live decoder let go');
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
			refused = true;
			reached.notSupported++;
		} else {
			failures++;
			broke = true;
			reached.failed++;
		}
		frames.error(dec, task.name, task.name);
	};

	const deliver = () => {
		while (!socket.length) produce();
		const m = socket.shift()!;
		if (m.t === 'hello') hello();
		else part(m);
	};

	for (step = 1; step <= STEPS; step++) {
		const roll = draw(100);
		if (roll < 40) deliver();
		else if (roll < 65) {
			const busy = decs.filter((d) => d.open && d.held.length);
			if (busy.length) {
				const dec = busy[draw(busy.length)];
				tasks.push({ t: 'output', dec, at: dec.held.shift()! });
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
			reconnect();
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
				tasks.push({ t: 'output', dec: d, at: d.held.shift()! });
		const task = tasks.shift();
		if (!task) break;
		step++;
		runTask(task);
		check();
	}
	// The session ends.
	streaming = false;
	latest = null;
	frames.closed();
	while (tasks.length) runTask(tasks.shift()!);
	check();
	for (const [k, d] of decs.entries())
		if (d.open || d.closes + Number(d.failed) !== 1)
			fail(`decoder ${k} closed ${d.closes} times, failed: ${d.failed}`);
	for (const p of pictures)
		if (p.lets !== 1) fail(`a picture of ${p.at} let go ${p.lets} times`);
	if (arrived !== accepted + frames.dropped)
		fail(
			`${arrived} updates arrived; ${accepted} came out, ${frames.dropped} dropped`
		);
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
