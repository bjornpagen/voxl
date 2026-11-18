// Checks the worker's record of the screen (frames.ts) against a fake of the relay and
// WebCodecs, over many seeded runs:
//   node web/src/frames.test.ts
// The fake relay sends streams: a hello naming the stream's codec, then a keyframe, then
// deltas, now and then another keyframe, now and then a new hello (a resize) between any
// two updates; and now and then a fault: a delta before the stream's keyframe, a codec the
// browser cannot configure (`open` throws) or lacks (its decoder fails with
// NotSupportedError, and the check of the hello's codec says so, some time later), an
// update the decoder refuses (`decode` throws), or a decoder failing at any time. The fake
// browser runs the decoders' outputs and errors and the checks' answers as tasks in the
// order queued, between the socket's messages in any order; a task queued before the record
// closed a decoder may still run.
//
// Each session's lane is drawn: datagrams, where a stream's keyframe may miss and runs of
// deltas come before the next keyframe or hello, or a stream, where a delta before the
// keyframe is a fault.
//
// What it checks are the fake's own facts. After every step:
// - on a datagram lane, a stream waiting for its keyframe survives any run of deltas: none
//   decodes, each is counted passed, the session goes on, and it decodes from the next
//   keyframe; on a stream lane the first delta before the keyframe ends the session;
// - each update of a stream with no fault goes, in order, to the decoder opened, for the
//   hello's codec, on the stream's keyframe, and only to it;
// - each fault ends the session exactly once, at once, and nothing is decoded after it
//   until the next session: no keyframe is asked for, no decoder reconfigured;
// - a picture is shown exactly when it comes out of the decoder the record decodes with,
//   and let go otherwise, each once;
// - the record never decodes into, or closes, a decoder that is closed, and closes each
//   decoder it opened once, unless the decoder failed first.
import { Frames, type LaneKind, type World } from './frames.ts';

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

/** Codecs the fake browser decodes; one it lacks (NotSupportedError, and its check says
 * unsupported); one it cannot configure. */
const GOOD = ['vp09.00.10.08', 'vp09.00.41.08'];
const LACKED = 'vp09.02.10.10';
const INVALID = 'vp09.invalid';

/** An update's bytes: an object of its own, so a decode names the update it was given. */
interface Bytes {
	n: number;
}

interface Dec {
	codec: string;
	open: boolean;
	closes: number;
	failed: boolean;
	/** Arrival times of the updates it took and has not put out, oldest first. */
	held: number[];
}

interface Picture {
	lets: number;
	shown: boolean;
}

type Message =
	| { t: 'part'; key: boolean; bytes: Bytes }
	| { t: 'hello'; codec: string };

type Task =
	| { t: 'output'; dec: Dec; at: number }
	| { t: 'error'; dec: Dec; name: string }
	/** The check of a stream's codec answers that it is unsupported. */
	| { t: 'unsupported'; codec: string; stream: number };

/** How often each clause was reached, over every run. */
const reached = {
	opened: 0,
	keyAgain: 0,
	deltaFirst: 0,
	passedWaiting: 0,
	keyAfterPassing: 0,
	configureThrew: 0,
	notSupported: 0,
	checkedUnsupported: 0,
	refused: 0,
	failed: 0,
	lateError: 0,
	lateOutput: 0,
	shown: 0,
	helloMidStream: 0,
};

function run(seed: number) {
	const draw = random(seed);
	let step = 0;
	let failure: Error | null = null;
	const fail = (what: string): never => {
		failure ??= new Error(`seed ${seed}, step ${step}: ${what}`);
		throw failure;
	};

	const tasks: Task[] = [];
	const decs: Dec[] = [];
	const pictures: Picture[] = [];
	/** The relay's stream: its codec, and whether its keyframe went. */
	let codec = GOOD[0];
	/** The session's lane: drawn for each session. */
	let lane: LaneKind = draw(2) ? 'datagram' : 'stream';
	/** Deltas passed while waiting, as the fake counts them and as the record says. */
	let passedExpected = 0;
	let passed = 0;
	/** The stream passed deltas since its hello. */
	let passedSince = false;
	let socket: Message[] = [{ t: 'hello', codec }];
	let keyed = false;
	let n = 0;
	/** The record's stream as the fake knows it: its number, its codec, whether the session
	 * ended, and the decoder its keyframe opened. */
	let stream = 0;
	let streamCodec = codec;
	let inStream = false;
	let ended = false;
	let ends = 0;
	let expectedEnds = 0;
	let live: Dec | null = null;
	/** The check of the stream's codec has not been queued yet. */
	let checkOwed = false;
	/** The update being delivered, and what the record did with it. */
	let delivering: Extract<Message, { t: 'part' }> | null = null;
	let decodedInto: Dec | null = null;
	let threw = false;
	let outputting: Picture | null = null;

	const world: World<Dec, Picture, Bytes> = {
		open(c) {
			const part = delivering ?? fail('a decoder opened outside an update');
			if (!part.key) fail('a decoder opened on a delta');
			if (c !== streamCodec)
				fail(`a decoder opened for ${c}, not ${streamCodec}`);
			if (live?.open) fail('a second decoder in a stream');
			if (c === INVALID) throw new TypeError(`${c} is not a codec`);
			const dec: Dec = {
				codec: c,
				open: true,
				closes: 0,
				failed: false,
				held: [],
			};
			decs.push(dec);
			live = dec;
			if (c === LACKED) {
				tasks.push({ t: 'error', dec, name: 'NotSupportedError' });
				if (checkOwed) tasks.push({ t: 'unsupported', codec: c, stream });
				checkOwed = false;
			}
			return dec;
		},
		decode(dec, key, at, bytes) {
			const part = delivering ?? fail('a decode outside an update');
			if (!dec.open) fail('a decode into a closed decoder');
			if (dec !== live) fail('a decode into another decoder than the stream’s');
			if (key !== part.key || at !== step || bytes !== part.bytes)
				fail('a decode of another update than the one that arrived');
			decodedInto = dec;
			if (!draw(300)) {
				threw = true;
				throw new DOMException('refused', 'DataError');
			}
			dec.held.push(at);
		},
		close(dec) {
			if (!dec.open) fail('a decoder closed twice, or after its error');
			dec.closes++;
			dec.open = false;
			dec.held.length = 0;
		},
		letGo(p) {
			if (outputting !== p) fail('a picture let go outside its output');
			p.lets++;
		},
		show(p) {
			if (outputting !== p) fail('a picture shown outside its output');
			p.lets++;
			p.shown = true;
			reached.shown++;
		},
		passed() {
			passed++;
		},
		end() {
			if (ended) fail('a session ended twice');
			ended = true;
			ends++;
		},
	};
	const frames = new Frames(world);

	const check = () => {
		if (failure) throw failure;
		if (ends !== expectedEnds)
			fail(`${ends} ends where the faults make ${expectedEnds}`);
		if (passed !== passedExpected)
			fail(`${passed} deltas passed where ${passedExpected} waited`);
	};

	/** The relay's next message: a hello now and then, else an update. */
	const produce = () => {
		if (!draw(40)) {
			if (keyed) reached.helloMidStream++;
			codec = draw(15) ? GOOD[draw(2)] : [LACKED, INVALID][draw(2)];
			socket.push({ t: 'hello', codec });
			keyed = false;
			return;
		}
		// Now and then a stream starts on deltas: on a datagram lane its keyframe missed, and
		// runs of deltas come before the next hello; on a stream lane, a fault.
		const key = keyed ? !draw(20) : draw(lane === 'datagram' ? 3 : 30) !== 0;
		socket.push({ t: 'part', key, bytes: { n: n++ } });
		keyed ||= key;
	};

	/** The session closes and another starts (the viewer asked, after an end). */
	const reconnect = () => {
		frames.closed();
		ended = false;
		inStream = false;
		live = null;
		socket = [{ t: 'hello', codec }];
		keyed = false;
		lane = draw(2) ? 'datagram' : 'stream';
	};

	const deliver = () => {
		// An ended session's socket is closed: nothing more arrives on it.
		if (ended) reconnect();
		while (!socket.length) produce();
		const m = socket.shift()!;
		if (m.t === 'hello') {
			streamCodec = m.codec;
			inStream = true;
			live = null;
			stream = frames.hello(lane, m.codec);
			passedSince = false;
			// The check of a codec the browser lacks answers later: before the decoder's own
			// error, or after it.
			checkOwed = m.codec === LACKED;
			if (checkOwed && draw(2)) {
				tasks.push({ t: 'unsupported', codec: m.codec, stream });
				checkOwed = false;
			}
			return;
		}
		const before = live;
		delivering = m;
		decodedInto = null;
		threw = false;
		frames.part(step, m.key, m.bytes);
		delivering = null;
		const into = decodedInto as Dec | null;
		// What this update should have done.
		let fault: keyof typeof reached | null = null;
		if (!inStream) fault = 'deltaFirst';
		else if (before === null) {
			if (!m.key && lane === 'datagram') {
				// Waiting survives it: undecoded, counted, the session goes on.
				passedExpected++;
				reached.passedWaiting++;
				passedSince = true;
				if (ended) fail('a delta while a datagram stream waits ended it');
				if (into) fail('a delta while waiting decoded');
				return;
			}
			if (!m.key) fault = 'deltaFirst';
			else if (streamCodec === INVALID) fault = 'configureThrew';
		}
		if (threw) fault = 'refused';
		if (fault) {
			reached[fault]++;
			expectedEnds++;
			inStream = false;
			if (!ended) fail(`${fault} did not end the session`);
			if (into && fault !== 'refused') fail(`${fault} decoded`);
			if (before?.open || live?.open) fail(`${fault} left a decoder open`);
			return;
		}
		if (into === null) fail('an update of a sound stream decoded nowhere');
		if (before === null) {
			reached.opened++;
			if (passedSince) reached.keyAfterPassing++;
		} else if (m.key) reached.keyAgain++;
		if (before !== null && into !== before)
			fail('an update went to a new decoder');
	};

	const runTask = (task: Task) => {
		if (task.t === 'unsupported') {
			// It ends the session only while the record is still in that stream.
			const ours = !ended && inStream && stream === task.stream;
			frames.unsupported(task.stream);
			if (ours) {
				reached.checkedUnsupported++;
				expectedEnds++;
				inStream = false;
			}
			return;
		}
		const { dec } = task;
		if (task.t === 'output') {
			const p: Picture = { lets: 0, shown: false };
			pictures.push(p);
			const shouldShow = dec.open && dec === live && !ended;
			outputting = p;
			frames.output(dec, p);
			outputting = null;
			if (p.lets !== 1) fail('a picture let go other than once');
			if (p.shown !== shouldShow) fail(`a picture shown: ${p.shown}`);
			if (!shouldShow) reached.lateOutput++;
			return;
		}
		if (!dec.open) {
			if (dec.failed) return;
			reached.lateError++;
			frames.error(dec, task.name, 'late');
			return;
		}
		dec.failed = true;
		dec.open = false;
		dec.held.length = 0;
		const ours = dec === live && !ended;
		frames.error(dec, task.name, task.name);
		if (ours) {
			reached[task.name === 'NotSupportedError' ? 'notSupported' : 'failed']++;
			expectedEnds++;
			inStream = false;
		}
	};

	for (step = 1; step <= STEPS; step++) {
		const roll = draw(100);
		if (roll < 45) deliver();
		else if (roll < 70) {
			const busy = decs.filter((d) => d.open && d.held.length);
			if (busy.length) {
				const dec = busy[draw(busy.length)];
				tasks.push({ t: 'output', dec, at: dec.held.shift()! });
			}
		} else if (roll < 97) {
			const task = tasks.shift();
			if (task) runTask(task);
		} else if (roll < 99) {
			const open = decs.filter((d) => d.open);
			if (open.length)
				tasks.push({
					t: 'error',
					dec: open[draw(open.length)],
					name: 'EncodingError',
				});
		} else reconnect();
		check();
	}
	frames.closed();
	ended = true;
	while (tasks.length) runTask(tasks.shift()!);
	check();
	for (const [k, d] of decs.entries())
		if (d.open || d.closes + Number(d.failed) !== 1)
			fail(`decoder ${k} closed ${d.closes} times, failed: ${d.failed}`);
	for (const p of pictures)
		if (p.lets !== 1) fail('a picture let go other than once');
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
