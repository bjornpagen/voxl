// Checks the touch gestures (touch.ts) over many seeded runs:
//   node web/src/touch.test.ts
// A run drives one machine with fingers and time, as the page would: stories of taps, and
// stories of chaos (one to three fingers going down, moving a little or a lot, lifting or
// being cancelled, time passing a little or past every hold, the mode switched between
// gestures or in the middle of one). Time comes in with the events; a timer's tick is
// delivered on time, late (the next event brings it) or not at all until then.
//
// What it checks, after every call:
// - a button pressed is released, and never pressed twice; the right button is only ever
//   clicked (pressed and released at once); no button is held once every finger is up;
// - a pinch sends nothing to the desktop (no pointer move, button or scroll) until fewer
//   than two fingers are down;
// - a gesture that begins in trackpad mode begins by saying where the trackpad pointer
//   is, which is where it was left (or the middle of the desktop at first) and on the
//   desktop; the pointer never leaves the desktop.
// A tap story, a finger down and up again without moving much and without holding, is
// exactly one click of the left button: in direct mode at the tap's point (or at the last
// tap's, if that came soon and close), in trackpad mode at the pointer, where it was left,
// even if the finger wobbled a few pixels.
import {
	createGestures,
	HOLD,
	LONG_PRESS,
	MULTI_TAP_MS,
	MULTI_TAP_PX,
	SLOP,
	STILL,
	TAP_MS,
	type Env,
	type Mode,
	type Out,
} from './touch.ts';

const RUNS = 20_000;
/** A tap story lasts this long (ms), less than the time of a tap. */
const TAP_STORY_MS = 150;
if (TAP_STORY_MS >= TAP_MS) throw new Error('a tap story outlasts a tap');

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

const reached: Record<string, number> = {
	'direct tap': 0,
	'direct tap at the last tap': 0,
	'trackpad tap': 0,
	'trackpad tap that wobbled': 0,
	'direct drag': 0,
	'long press right click': 0,
	'trackpad slide': 0,
	'trackpad grab': 0,
	scroll: 0,
	pinch: 0,
	'two finger right click': 0,
	'cancel while a button is held': 0,
	'cancel with no button held': 0,
	'tick delivered late': 0,
	'mode switched in a gesture': 0,
	'a third finger': 0,
	'trackpad pointer at the edge': 0,
};
const reach = (clause: string) => {
	reached[clause]++;
};

interface P {
	x: number;
	y: number;
}

const clicks = (batch: Out[]) =>
	batch
		.filter((o) => o.k === 'button')
		.map((o) => `${o.n}${o.down ? 'v' : '^'}`)
		.join(' ');

function run(seed: number) {
	const draw = random(seed);
	const pick = <T>(xs: readonly T[]): T => xs[draw(xs.length)];
	const env: Env = {
		w: 100 + draw(2900),
		h: 100 + draw(1900),
		perPx: (1 + draw(8)) / 2,
	};
	const outs: Out[] = [];
	const touch = createGestures((o) => outs.push(o));
	touch.env = env;
	let now = draw(10_000);
	const down = new Map<number, P>();
	const held = { 1: false, 3: false };
	let pinching = false;
	let lastTo: P | null = null;
	/** The last direct tap, as the oracle remembers it. */
	let prevTap: { x: number; y: number; t: number } | null = null;

	const fail = (why: string): never => {
		throw new Error(`seed ${seed}: ${why}`);
	};
	const centre = (): P => lastTo ?? { x: env.w / 2, y: env.h / 2 };

	/** Runs one call of the machine and checks what it put out; returns that. */
	function call(label: string, f: () => void): Out[] {
		const n = outs.length;
		// The first finger of a gesture, in trackpad mode.
		const startsTrackpad =
			label === 'down' && down.size === 1 && touch.mode === 'trackpad';
		f();
		const batch = outs.slice(n);
		for (const o of batch) {
			if (
				pinching &&
				o.k !== 'pinch' &&
				o.k !== 'pinch-begin' &&
				o.k !== 'buzz'
			)
				fail(`${o.k} while pinching`);
			if (o.k === 'pinch-begin') {
				pinching = true;
				reach('pinch');
			}
			if (o.k === 'pan') reach('scroll');
			if (o.k === 'button') {
				if (held[o.n] === o.down)
					fail(`button ${o.n} ${o.down ? 'pressed twice' : 'released twice'}`);
				held[o.n] = o.down;
				if (o.n === 1 && o.down && label === 'tick') reach('trackpad grab');
				if (o.n === 1 && o.down && label === 'move') reach('direct drag');
				if (o.n === 3 && o.down && label === 'tick')
					reach('long press right click');
				if (o.n === 3 && o.down && label === 'up')
					reach('two finger right click');
				if (o.n === 1 && !o.down && label === 'cancel')
					reach('cancel while a button is held');
			}
			if (o.k === 'to') {
				if (o.x < 0 || o.y < 0 || o.x > env.w - 1 || o.y > env.h - 1)
					fail(`pointer left the desktop at ${o.x},${o.y}`);
				if (o.x === 0 || o.y === 0 || o.x === env.w - 1 || o.y === env.h - 1)
					reach('trackpad pointer at the edge');
				if (label === 'move') reach('trackpad slide');
				lastTo = { x: o.x, y: o.y };
			}
		}
		if (held[3]) fail('the right button is held');
		if (down.size < 2) pinching = false;
		if (startsTrackpad) {
			const c = batch[0];
			const want = centre();
			if (!c || c.k !== 'to')
				fail('a trackpad gesture did not begin by placing the pointer');
			else if (c.x !== want.x || c.y !== want.y)
				fail(
					`the pointer began at ${c.x},${c.y}, not where it was left (${want.x},${want.y})`
				);
		}
		if (down.size === 0 && (held[1] || held[3]))
			fail('a button is held with no finger down');
		if (down.size === 0 && (!touch.idle || touch.due !== null))
			fail('fingers are up but the machine is not idle');
		return batch;
	}

	/** Time passes; the tick of a hold that has come due is delivered on time, late, or
	 * left for the next event to bring. */
	function wait(ms: number) {
		now += ms;
		const due = touch.due;
		if (due !== null && due <= now && draw(2)) {
			if (now - due > 0) reach('tick delivered late');
			call('tick', () => touch.tick(now));
		}
	}
	const finger = (id: number, x: number, y: number) => {
		down.set(id, { x, y });
		return call('down', () => touch.down(id, x, y, now));
	};
	const moveTo = (id: number, x: number, y: number) => {
		down.set(id, { x, y });
		return call('move', () => touch.move(id, x, y, now));
	};
	const lift = (id: number) => {
		down.delete(id);
		return call('up', () => touch.up(id, now));
	};
	const cancel = (id: number) => {
		const grabbed = held[1];
		down.delete(id);
		const batch = call('cancel', () => touch.cancel(id, now));
		if (!grabbed && down.size === 0) reach('cancel with no button held');
		return batch;
	};
	/** A finger goes down and up again within the time of a tap and wanders `reach` px at
	 * most; this is one click of the left button, at the pointer. */
	function tapStory(mode: Mode, wander: number) {
		const id = draw(3);
		const x = draw(2000),
			y = draw(1200);
		const start = now;
		const pointer = mode === 'trackpad' ? centre() : null;
		// Close to the last tap, now and then, so double-taps come up.
		const near = prevTap !== null && mode === 'direct' && draw(2) === 0;
		const px = near ? prevTap!.x + draw(2 * MULTI_TAP_PX) - MULTI_TAP_PX : x;
		const py = near ? prevTap!.y + draw(2 * MULTI_TAP_PX) - MULTI_TAP_PX : y;
		const batch = finger(id, px, py);
		for (let i = draw(3); i > 0; i--) {
			now += 1 + draw(30);
			const a = (draw(360) * Math.PI) / 180,
				r = draw(wander + 1);
			batch.push(
				...moveTo(
					id,
					px + Math.round(r * Math.cos(a)),
					py + Math.round(r * Math.sin(a))
				)
			);
		}
		now = Math.min(now + 1 + draw(40), start + TAP_STORY_MS);
		batch.push(...lift(id));
		const spot =
			prevTap &&
			now - prevTap.t < MULTI_TAP_MS &&
			Math.hypot(px - prevTap.x, py - prevTap.y) < MULTI_TAP_PX
				? prevTap
				: { x: px, y: py };
		if (clicks(batch) !== '1v 1^') fail(`a tap made clicks: ${clicks(batch)}`);
		const n = batch.length;
		if (batch.slice(n - 2).some((o) => o.k !== 'button'))
			fail('a click was not the last thing');
		if (mode === 'direct') {
			const at = batch[n - 3];
			if (
				batch.length !== 3 ||
				at.k !== 'at' ||
				at.x !== spot.x ||
				at.y !== spot.y
			)
				fail(
					`a direct tap at ${px},${py} did not click at ${spot.x},${spot.y}: ${JSON.stringify(batch)}`
				);
			prevTap = { x: spot.x, y: spot.y, t: now };
			reach('direct tap');
			if (spot.x !== px || spot.y !== py) reach('direct tap at the last tap');
		} else {
			const at = batch[n - 3];
			const c = pointer!;
			if (at.k !== 'to' || at.x !== c.x || at.y !== c.y)
				fail(
					`a trackpad tap did not click at the pointer: ${JSON.stringify(batch)}`
				);
			if (batch.some((o) => o.k !== 'to' && o.k !== 'button'))
				fail('a trackpad tap sent more than moves and a click');
			reach(batch.length > 3 ? 'trackpad tap that wobbled' : 'trackpad tap');
		}
	}

	/** Fingers go down, move, lift or are cancelled, and time passes, any way at all. */
	function chaosStory() {
		for (let steps = 3 + draw(14); steps > 0; steps--) {
			const ids = [0, 1, 2].filter((i) => down.has(i));
			const free = [0, 1, 2].filter((i) => !down.has(i));
			const roll = draw(100);
			if (roll < 22 && free.length) {
				if (down.size === 2) reach('a third finger');
				finger(pick(free), draw(2000), draw(1200));
			} else if (roll < 55 && ids.length) {
				const id = pick(ids),
					p = down.get(id)!;
				const far = pick([3, 15, 200]);
				moveTo(
					id,
					p.x + draw(2 * far + 1) - far,
					p.y + draw(2 * far + 1) - far
				);
			} else if (roll < 68 && ids.length) lift(pick(ids));
			else if (roll < 74 && ids.length) cancel(pick(ids));
			else if (roll < 82) {
				const was = touch.mode;
				touch.mode = was === 'direct' ? 'trackpad' : 'direct';
				if (down.size) reach('mode switched in a gesture');
				else if (touch.mode === 'trackpad') call('place', () => touch.place());
			} else if (roll < 90) call('tick', () => touch.tick(now));
			else
				wait(pick([HOLD - 1, HOLD + 1, LONG_PRESS - 1, LONG_PRESS + 1, 2000]));
			wait(1 + draw(60));
		}
		for (const id of down.keys()) {
			if (draw(4) === 0) cancel(id);
			else lift(id);
			wait(1 + draw(60));
		}
	}

	for (let stories = 1 + draw(6); stories > 0; stories--) {
		// A gap that may be short enough for a double-tap, or past every hold.
		now += pick([1, 50, 300, MULTI_TAP_MS + 1, 2000]);
		const mode = pick(['direct', 'trackpad'] as const);
		touch.mode = mode;
		if (draw(3)) {
			if (mode === 'trackpad' && draw(2)) call('place', () => touch.place());
			const wobble = mode === 'trackpad' && draw(2) === 0;
			tapStory(
				mode,
				mode === 'direct' ? SLOP - 2 : wobble ? SLOP - 2 : STILL - 1
			);
		} else {
			chaosStory();
			// Past the double-tap time of any tap before it, which the oracle forgets.
			now += MULTI_TAP_MS + 1;
			prevTap = null;
		}
	}
	if (held[1] || held[3] || down.size)
		fail('the run ended with a finger or a button down');
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
