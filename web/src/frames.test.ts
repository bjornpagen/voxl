// Checks the worker's record of the screen (frames.ts) against a fake of the relay and
// WebCodecs, over many seeded runs:
//   node web/src/frames.test.ts
// A fake daemon numbers its updates and roots each delta in the newest keyframe before it;
// the session's first message is its hello, naming the session's codec. Over a datagram lane
// any update may miss (a keyframe too), so deltas can arrive whose keyframe never did; over
// a stream lane every update arrives, and only a session that starts on a delta has one.
// More hellos (resizes) come at any point between updates, before or after the keyframe
// that follows them, as the control stream and the lane leave them unordered; now and then
// one names another codec. And now and then a fault: a codec the browser cannot configure
// (`open` throws) or lacks (its decoder fails with NotSupportedError, and the check of the
// session's codec says so, some time later), an update the decoder refuses (`decode`
// throws), or a decoder failing at any time. The fake browser runs the decoders' outputs and
// errors and the checks' answers as tasks in the order queued, between the socket's messages
// in any order; a task queued before the record closed a decoder may still run.
//
// What it checks are the fake's own facts. After every step:
// - a keyframe decodes; a delta decodes exactly when the keyframe it descends from is the
//   last keyframe the decoder took, into that decoder, in arrival order;
// - any other delta is passed over and counted on a datagram lane, and the session goes on;
//   on a stream lane it ends the session;
// - a hello after the first changes nothing, unless it names another codec, which ends the
//   session;
// - each fault ends the session exactly once, at once, and nothing is decoded after it
//   until the next session: no keyframe is asked for, no decoder reconfigured;
// - a picture is shown exactly when it comes out of the decoder the record decodes with,
//   and let go otherwise, each once;
// - the record never decodes into, or closes, a decoder that is closed, and closes each
//   decoder it opened once, unless the decoder failed first.
import { Frames, type LaneKind, type Update, type World } from './frames.ts';

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
	| { t: 'part'; u: Update; bytes: Bytes }
	| { t: 'hello'; codec: string };

type Task =
	| { t: 'output'; dec: Dec; at: number }
	| { t: 'error'; dec: Dec; name: string }
	/** The check of a stream's codec answers that it is unsupported. */
	| { t: 'unsupported'; codec: string; session: number };

/** How often each clause was reached, over every run. */
const reached = {
	opened: 0,
	keyAgain: 0,
	deltaFirst: 0,
	passedUnrooted: 0,
	passedAfterDecoding: 0,
	keyAfterPassing: 0,
	codecChanged: 0,
	helloAfterItsKey: 0,
	configureThrew: 0,
	notSupported: 0,
	checkedUnsupported: 0,
	refused: 0,
	failed: 0,
	lateError: 0,
	lateOutput: 0,
	shown: 0,
	helloMidStream: 0,
	beforeHello: 0,
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
	/** The session's codec, as its first hello names it. */
	let codec = GOOD[0];
	/** The session's lane: drawn for each session. */
	let lane: LaneKind = draw(2) ? 'datagram' : 'stream';
	/** Deltas passed, as the fake counts them and as the record says. */
	let passedExpected = 0;
	let passed = 0;
	/** The fake daemon: the next update's number, and the newest keyframe's. */
	let n = 1;
	let rootKey: number | null = null;
	/** A hello waits to follow the next keyframe (a resize's, arriving after it). */
	let helloAfterKey = false;
	let socket: Message[] = [];
	/** The record's session as the fake knows it: its number, whether it is in one, whether
	 * it ended, the decoder its first keyframe opened, and the last keyframe it decoded. */
	let session = 0;
	let inSession = false;
	let ended = false;
	let ends = 0;
	let expectedEnds = 0;
	let live: Dec | null = null;
	let decodedKey: number | null = null;
	/** The stream passed deltas since it last decoded a keyframe. */
	let passedSince = false;
	/** The check of the session's codec has not been queued yet. */
	let checkOwed = false;
	/** The update being delivered, and what the record did with it. */
	let delivering: Extract<Message, { t: 'part' }> | null = null;
	let decodedInto: Dec | null = null;
	let threw = false;
	let outputting: Picture | null = null;

	const world: World<Dec, Picture, Bytes> = {
		open(c) {
			const part = delivering ?? fail('a decoder opened outside an update');
			if (!part.u.key) fail('a decoder opened on a delta');
			if (c !== codec) fail(`a decoder opened for ${c}, not ${codec}`);
			if (live?.open) fail('a second decoder in a session');
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
				if (checkOwed) tasks.push({ t: 'unsupported', codec: c, session });
				checkOwed = false;
			}
			return dec;
		},
		decode(dec, key, at, bytes) {
			const part = delivering ?? fail('a decode outside an update');
			if (!dec.open) fail('a decode into a closed decoder');
			if (dec !== live)
				fail("a decode into another decoder than the session's");
			if (key !== part.u.key || at !== step || bytes !== part.bytes)
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
			fail(`${passed} deltas passed where ${passedExpected} had no keyframe`);
	};

	/** A session's first messages: its hello, or, rarely, an update before it. */
	const begin = () => {
		codec = draw(15) ? GOOD[draw(2)] : [LACKED, INVALID][draw(2)];
		lane = draw(2) ? 'datagram' : 'stream';
		rootKey = null;
		helloAfterKey = false;
		socket = draw(200) ? [{ t: 'hello', codec }] : [];
	};
	begin();

	/** The fake daemon's next message: a hello now and then (before or after the keyframe
	 * that follows it), else an update, which on a datagram lane may miss. */
	const produce = () => {
		if (!draw(40)) {
			reached.helloMidStream++;
			const other = !draw(10);
			const c = other ? GOOD.find((g) => g !== codec)! : codec;
			if (!other && draw(2)) helloAfterKey = true;
			else socket.push({ t: 'hello', codec: c });
			return;
		}
		const key =
			rootKey === null ? draw(lane === 'datagram' ? 3 : 30) !== 0 : !draw(20);
		const frame = n++;
		if (key) rootKey = frame;
		const u: Update = { key, frame, from: rootKey ?? frame - 1 };
		if (lane === 'datagram' && !draw(6)) return;
		socket.push({ t: 'part', u, bytes: { n: frame } });
		if (key && helloAfterKey) {
			helloAfterKey = false;
			reached.helloAfterItsKey++;
			socket.push({ t: 'hello', codec });
		}
	};

	/** The session closes and another starts (the viewer asked, after an end). */
	const reconnect = () => {
		frames.closed();
		ended = false;
		inSession = false;
		live = null;
		decodedKey = null;
		passedSince = false;
		begin();
	};

	/** A fault the record must end the session for. */
	const faulted = (fault: keyof typeof reached, decoded: boolean) => {
		reached[fault]++;
		expectedEnds++;
		inSession = false;
		if (!ended) fail(`${fault} did not end the session`);
		if (decoded && fault !== 'refused') fail(`${fault} decoded`);
		if (live?.open) fail(`${fault} left a decoder open`);
	};

	const deliver = () => {
		// An ended session's socket is closed: nothing more arrives on it.
		if (ended) reconnect();
		while (!socket.length) produce();
		const m = socket.shift()!;
		if (m.t === 'hello') {
			if (!inSession) {
				inSession = true;
				// The session's codec is what its first hello names.
				codec = m.codec;
				session =
					frames.hello(lane, m.codec) ??
					fail("a session's first hello returned no session");
				// The check of a codec the browser lacks answers later: before the decoder's
				// own error, or after it.
				checkOwed = m.codec === LACKED;
				if (checkOwed && draw(2)) {
					tasks.push({ t: 'unsupported', codec: m.codec, session });
					checkOwed = false;
				}
				return;
			}
			const again = frames.hello(lane, m.codec);
			if (again !== null) fail('a later hello began a session');
			if (m.codec !== codec) return faulted('codecChanged', false);
			if (ended) fail('a hello of the same codec ended the session');
			return;
		}
		const before = live;
		delivering = m;
		decodedInto = null;
		threw = false;
		frames.part(step, m.u, m.bytes);
		delivering = null;
		const into = decodedInto as Dec | null;
		if (!inSession) return faulted('beforeHello', into !== null);
		const rooted = m.u.key || m.u.from === decodedKey;
		if (!rooted) {
			if (lane === 'stream') return faulted('deltaFirst', into !== null);
			passedExpected++;
			reached[decodedKey === null ? 'passedUnrooted' : 'passedAfterDecoding']++;
			passedSince = true;
			if (ended) fail('a passed delta ended the session');
			if (into) fail('a delta without its keyframe decoded');
			return;
		}
		if (m.u.key && codec === INVALID && before === null)
			return faulted('configureThrew', into !== null);
		if (threw) return faulted('refused', true);
		if (into === null) fail('a rooted update decoded nowhere');
		if (before === null) reached.opened++;
		else if (m.u.key) reached.keyAgain++;
		if (m.u.key && passedSince) reached.keyAfterPassing++;
		if (m.u.key) {
			decodedKey = m.u.frame;
			passedSince = false;
		}
		if (before !== null && into !== before)
			fail('an update went to a new decoder');
	};

	const runTask = (task: Task) => {
		if (task.t === 'unsupported') {
			// It ends the session only while the record is still in that session.
			const ours = !ended && inSession && session === task.session;
			frames.unsupported(task.session);
			if (ours) {
				reached.checkedUnsupported++;
				expectedEnds++;
				inSession = false;
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
			inSession = false;
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
			const alive = decs.filter((d) => d.open);
			if (alive.length)
				tasks.push({
					t: 'error',
					dec: alive[draw(alive.length)],
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
