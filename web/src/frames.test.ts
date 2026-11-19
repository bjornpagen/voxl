// Checks the worker's record of the screen (frames.ts) against a fake of the daemon, the
// relay and WebCodecs, over many seeded runs:
//   node web/src/frames.test.ts
// A fake daemon sends a session's hello, naming the session's codec, then its updates in
// order on the one lane: a keyframe, then deltas, a keyframe again now and then (a viewer
// starting, a resize). A fake relay gives the page one update at a time, and the next only
// once the page took the one before, which the worker says when the record hands that
// update's picture to the page; a hello it gives as it comes. Now and then a fault: an update before the
// hello, a delta before the first keyframe, a second hello, an update given before the one
// before it was taken, a codec the browser cannot configure (`open` throws) or lacks (its
// decoder fails with NotSupportedError, and the check of the session's codec says so, some
// time later), an update the decoder refuses (`decode` throws), or a decoder failing at any
// time. The fake browser runs the decoders' outputs and errors and the checks' answers as
// tasks in the order queued, between the relay's messages in any order; a task queued
// before the record closed a decoder may still run. One decoder in twelve keeps a picture
// in until its next input, which WebCodecs allows ("the underlying codec implementation MAY
// emit new outputs only when new inputs are provided") and which, with one update given at
// a time, never comes. A run ends with the daemon's last updates given and decoded to the
// end, or, with such a decoder, with the session where that decoder left it.
//
// What it checks are the fake's own facts. After every step:
// - the page holds at most one update whose picture has not come out of its decoder;
// - every update from the session's first keyframe on decodes, into the decoder that
//   keyframe opened, in arrival order, exactly once;
// - the picture of the update the relay gave last is handed to the page, once, when it
//   comes out, and only then is the next given: so every update given is handed on once,
//   in order;
// - the relay is left waiting only by a decoder that keeps its picture in: then the
//   session neither ends nor moves, nothing more is given, and the page's meter (stats.ts,
//   kept from what the record tells the world) reads the decoder as decoding since that
//   update's arrival, as a harness reads it, step after step; with a decoder that puts out
//   a picture for each update, every update given is taken;
// - after every step the meter reads the decoder as holding exactly the update the relay
//   waits for, since its arrival, or as idle;
// - a session's first hello has its codec checked, once, and no other hello does; the
//   page is told what the decoder decodes exactly when one opens, and that it decodes
//   nothing exactly when that one closes or fails;
// - each fault ends the session exactly once, at once, and nothing is decoded after it
//   until the next session: no keyframe is asked for, no decoder reconfigured;
// - a picture is handed on exactly when it comes out of the decoder the record decodes with
//   while that holds an update, and let go otherwise, each once;
// - the record never decodes into, or closes, a decoder that is closed, and closes each
//   decoder it opened once, unless the decoder failed first.
import { Frames, type Session, type Update, type World } from './frames.ts';
import { holds, Meter, type Decoder } from './stats.ts';

const RUNS = 20_000;

/** The arrival of the update a decoder's state says is inside it, or null for none. */
function arrivalInside(state: Decoder): number | null {
	switch (state.s) {
		case 'decoding':
			return state.since;
		case 'idle':
			return null;
	}
}
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
	/** It keeps a picture in until its next input, as WebCodecs lets a decoder do: with one
	 * update given at a time, that input never comes. */
	withholds: boolean;
}

interface Picture {
	lets: number;
	handed: boolean;
}

type Message =
	| { t: 'part'; u: Update; bytes: Bytes }
	| { t: 'hello'; codec: string };

type Task =
	| { t: 'output'; dec: Dec; at: number }
	| { t: 'error'; dec: Dec; name: string }
	/** The check of a stream's codec answers that it is unsupported. */
	| { t: 'unsupported'; session: Session };

/** How often each clause was reached, over every run. */
const reached = {
	opened: 0,
	keyAgain: 0,
	deltaFirst: 0,
	helloAgain: 0,
	configureThrew: 0,
	notSupported: 0,
	checkedUnsupported: 0,
	refused: 0,
	failed: 0,
	early: 0,
	unasked: 0,
	waited: 0,
	drained: 0,
	withheld: 0,
	lateError: 0,
	lateOutput: 0,
	handed: 0,
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
	/** The fake daemon: the next update's number, and whether it sent a keyframe yet. */
	let n = 1;
	let keyed = false;
	let socket: Message[] = [];
	/** The fake relay: the arrival of the update it gave that the page has not taken, and
	 * the updates given and handed in the session. */
	let owed: number | null = null;
	let given = 0;
	let handed = 0;
	/** The record's session as the fake knows it: its number, whether it is in one, whether
	 * it ended, the decoder its first keyframe opened, and whether it decoded a keyframe. */
	let session: Session = { codec: 'none yet' };
	let inSession = false;
	let ended = false;
	let ends = 0;
	let expectedEnds = 0;
	let live: Dec | null = null;
	let decodedKey = false;
	/** The check of the session's codec has not been queued yet. */
	let checkOwed = false;
	/** The codec of the hello being delivered, and the checks the record asked for in it. */
	let helloing: string | null = null;
	let checks = 0;
	/** What the page was last told the decoder decodes; null for nothing. */
	let told: string | null = null;
	/** What the record told the world is inside the decoder: the arrival of the update
	 * inside it, or null for none. The page's meter is kept from the same word. */
	let inside: number | null = null;
	const meter = new Meter(0);
	/** The update being delivered, and what the record did with it. */
	let delivering: Extract<Message, { t: 'part' }> | null = null;
	let decodedInto: Dec | null = null;
	let threw = false;
	let outputting: Picture | null = null;

	/** What the record did, read where the compiler has lost track of who writes it. */
	const decodedNow = (): Dec | null => decodedInto;
	const decoder = (): Dec | null => live;

	const world: World<Dec, Picture, Bytes> = {
		check(asked) {
			if (helloing !== asked.codec)
				fail(`a check of ${asked.codec} outside its hello`);
			session = asked;
			checks++;
		},
		decodes(c) {
			if (told !== null) fail(`the page told ${c} while it knew of ${told}`);
			if (!delivering) fail('a codec told outside an update');
			told = c;
		},
		decodesNothing() {
			if (told === null) fail('the page told of no codec twice');
			told = null;
		},
		open(c) {
			const part = delivering ?? fail('a decoder opened outside an update');
			if (part.u.coding !== 'key') fail('a decoder opened on a delta');
			if (c !== codec) fail(`a decoder opened for ${c}, not ${codec}`);
			if (live?.open) fail('a second decoder in a session');
			if (c === INVALID) throw new TypeError(`${c} is not a codec`);
			const dec: Dec = {
				codec: c,
				open: true,
				closes: 0,
				failed: false,
				held: [],
				withholds: !draw(12),
			};
			decs.push(dec);
			live = dec;
			if (c === LACKED) {
				tasks.push({ t: 'error', dec, name: 'NotSupportedError' });
				if (checkOwed) tasks.push({ t: 'unsupported', session });
				checkOwed = false;
			}
			return dec;
		},
		decode(dec, coding, at, bytes) {
			const part = delivering ?? fail('a decode outside an update');
			if (!dec.open) fail('a decode into a closed decoder');
			if (dec !== live)
				fail("a decode into another decoder than the session's");
			if (coding !== part.u.coding || at !== step || bytes !== part.bytes)
				fail('a decode of another update than the one that arrived');
			decodedInto = dec;
			if (!draw(300)) {
				threw = true;
				throw new DOMException('refused', 'DataError');
			}
			dec.held.push(at);
		},
		decoder(state: Decoder) {
			holds(meter.counts, state);
			const next = arrivalInside(state);
			if (next !== null && decodedNow() === null)
				fail('the decoder told to hold an update it was not given');
			if (next === null && inside === null)
				fail('the decoder told empty twice');
			inside = next;
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
		present(p) {
			if (outputting !== p) fail('a picture handed on outside its output');
			if (inside !== null) fail('a picture handed on with its update inside');
			if (owed === null)
				fail('a picture handed on that the relay gave no update of');
			p.lets++;
			p.handed = true;
			// The worker says here that the page took the update.
			owed = null;
			handed++;
			reached.handed++;
		},
		end() {
			if (ended) fail('a session ended twice');
			ended = true;
			ends++;
		},
	};
	const frames = new Frames(world);

	/** The updates the page holds undecoded. */
	const held = () =>
		decs.reduce((sum, d) => sum + (d.open ? d.held.length : 0), 0);

	const check = () => {
		if (failure) throw failure;
		if (ends !== expectedEnds)
			fail(`${ends} ends where the faults make ${expectedEnds}`);
		if (held() > 1) fail(`the page holds ${held()} updates undecoded`);
		// The page knows what the decoder decodes exactly while one is open.
		const open = decs.find((d) => d.open);
		if (told !== (open ? open.codec : null))
			fail(`the page was told ${told} with ${open?.codec} decoding`);
		// What the world sees inside the decoder is the update the relay waits for.
		const waited = inSession && !ended ? owed : null;
		if (inside !== waited)
			fail(
				`the decoder is seen to hold ${inside}, and the relay waits for ${waited}`
			);
		// The meter reads it so, at any time after it.
		const read = JSON.stringify(meter.decoder(step + 0.5));
		const meant: Decoder =
			waited === null ? { s: 'idle' } : { s: 'decoding', since: waited };
		if (read !== JSON.stringify(meant))
			fail(`the meter reads ${read}, and the relay waits for ${waited}`);
		// With none given and untaken, every update given was handed.
		if (inSession && !ended && owed === null && handed !== given)
			fail(`${handed} updates handed on of ${given} given`);
	};

	/** A session's first messages: its hello, or, rarely, an update before it. */
	const begin = () => {
		codec = draw(15) ? GOOD[draw(2)] : [LACKED, INVALID][draw(2)];
		keyed = false;
		socket = draw(200) ? [{ t: 'hello', codec }] : [];
		owed = null;
		given = 0;
		handed = 0;
	};
	begin();

	/** The fake daemon's next message: rarely a second hello, else an update: the lane's
	 * first a keyframe (rarely a delta), later a keyframe now and then. */
	const produce = () => {
		if (!draw(300)) {
			socket.push({ t: 'hello', codec });
			return;
		}
		const key = keyed ? !draw(20) : draw(30) !== 0;
		keyed = true;
		const frame = n++;
		const u: Update = { coding: key ? 'key' : 'delta' };
		socket.push({ t: 'part', u, bytes: { n: frame } });
	};

	/** The session closes and another starts (the viewer asked, after an end). */
	const reconnect = () => {
		frames.closed();
		ended = false;
		inSession = false;
		live = null;
		decodedKey = false;
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
		// An update waits in the relay until the page took the one before; rarely, wrongly,
		// it is given before.
		if (socket[0].t === 'part' && owed !== null && draw(400)) {
			reached.waited++;
			return;
		}
		const m = socket.shift() ?? fail('the daemon sent nothing');
		if (m.t === 'hello') {
			helloing = m.codec;
			checks = 0;
			frames.hello(m.codec);
			helloing = null;
			if (!inSession) {
				inSession = true;
				// The session's codec is what its first hello names.
				codec = m.codec;
				if (checks !== 1) fail(`a session's hello asked for ${checks} checks`);
				// The check of a codec the browser lacks answers later: before the decoder's
				// own error, or after it.
				checkOwed = m.codec === LACKED;
				if (checkOwed && draw(2)) {
					tasks.push({ t: 'unsupported', session });
					checkOwed = false;
				}
				return;
			}
			if (checks) fail('a second hello began a session');
			return faulted('helloAgain', false);
		}
		const before = live;
		const early = owed !== null;
		delivering = m;
		decodedInto = null;
		threw = false;
		frames.part(step, m.u, m.bytes);
		delivering = null;
		const into = decodedNow();
		if (!inSession) return faulted('beforeHello', into !== null);
		if (m.u.coding === 'delta' && !decodedKey)
			return faulted('deltaFirst', into !== null);
		if (early && decodedKey) return faulted('early', into !== null);
		if (m.u.coding === 'key' && codec === INVALID && before === null)
			return faulted('configureThrew', into !== null);
		if (threw) return faulted('refused', true);
		if (into === null) fail('a rooted update decoded nowhere');
		if (before === null) reached.opened++;
		else if (m.u.coding === 'key') reached.keyAgain++;
		if (m.u.coding === 'key') decodedKey = true;
		if (before !== null && into !== before)
			fail('an update went to a new decoder');
		owed = step;
		given++;
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
			const p: Picture = { lets: 0, handed: false };
			pictures.push(p);
			const shouldHand = dec.open && dec === live && !ended;
			if (shouldHand && owed !== task.at)
				fail('a picture of another update than the one given last');
			outputting = p;
			frames.output(dec, p);
			outputting = null;
			if (p.lets !== 1) fail('a picture let go other than once');
			if (p.handed !== shouldHand) fail(`a picture handed on: ${p.handed}`);
			if (!shouldHand) reached.lateOutput++;
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

	/** A decoder that holds an update puts its picture out, as a task. */
	const output = () => {
		const busy = decs.filter((d) => d.open && d.held.length && !d.withholds);
		if (!busy.length) return;
		const dec = busy[draw(busy.length)];
		const at = dec.held.shift() ?? fail('a busy decoder holds nothing');
		tasks.push({ t: 'output', dec, at });
	};

	for (step = 1; step <= STEPS; step++) {
		const roll = draw(100);
		if (roll < 45) deliver();
		else if (roll < 70) output();
		else if (roll < 97) {
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
		} else if (roll === 99 && draw(3)) {
			// A decoder that holds no update puts out a picture all the same: no update's,
			// so it is let go, and nothing is taken.
			const dec = decoder();
			if (dec?.open && !dec.held.length && owed === null && !ended) {
				const p: Picture = { lets: 0, handed: false };
				pictures.push(p);
				outputting = p;
				frames.output(dec, p);
				outputting = null;
				if (p.lets !== 1 || p.handed)
					fail('a picture of no update was handed on');
				reached.unasked++;
			}
		} else reconnect();
		check();
	}
	// The daemon's last updates are given and decoded to the end: the relay is never left
	// with one the page does not take.
	const going = () => !ended && inSession;
	const drain = () => {
		for (let k = 0; k < 6; k++) produce();
		// A decoder that keeps its picture in leaves the session where it is: the update
		// is never taken, and nothing more is given.
		const stuck = () => {
			const dec = decoder();
			return (
				tasks.length === 0 &&
				dec !== null &&
				dec.withholds &&
				dec.held.length > 0
			);
		};
		const more = () =>
			going() &&
			!stuck() &&
			(socket.length > 0 || owed !== null || tasks.length > 0);
		for (let turns = 0; more(); turns++) {
			step++;
			if (turns > 2000) fail('the updates given were not all taken');
			const roll = draw(3);
			if (roll === 0 && socket.length) deliver();
			else if (roll === 1) output();
			else {
				const task = tasks.shift();
				if (task) runTask(task);
			}
			check();
		}
		if (!going()) return;
		if (!stuck()) {
			reached.drained++;
			return;
		}
		// The page does not end the session and does not take the update: the relay goes
		// on waiting, and what shows it is the decoder seen to hold the update, since its
		// arrival.
		const before = given;
		for (let k = 0; k < 50 && going(); k++) {
			step++;
			if (draw(2)) deliver();
			else output();
			check();
		}
		if (!going()) return;
		if (given !== before || handed !== given - 1 || inside !== owed)
			fail(
				'a decoder that kept its picture in did not leave the session waiting'
			);
		// What a harness sees: the decoder decoding since the update's arrival, long
		// after it.
		const seen = meter.decoder(step + 1_000_000);
		if (seen.s !== 'decoding' || seen.since !== owed)
			fail(`a withheld picture reads as ${JSON.stringify(seen)}`);
		reached.withheld++;
	};
	if (going()) drain();

	frames.closed();
	ended = true;
	for (let task = tasks.shift(); task; task = tasks.shift()) runTask(task);
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
