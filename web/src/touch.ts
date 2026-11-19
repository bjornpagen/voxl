// What fingers on a touchscreen do to the desktop: a state machine with fingers and
// time in and pointer input out, and no DOM, timer or clock of its own (the page feeds it
// the events with their time stamps and calls `tick` when `due` has come).
//
// One finger, in direct mode (the pointer is under the finger): a tap clicks, a slide
// drags with the left button, holding still clicks the right button. In trackpad mode (the
// finger moves a pointer the way a laptop's trackpad does): a slide moves the pointer, a
// tap clicks where it is, holding still grabs (the left button is held while the finger
// slides). Two fingers scroll or pinch to zoom the view (in either mode, and a pinch sends
// nothing to the desktop); in trackpad mode two fingers that lift without either
// right-click. A gesture ends when its last finger lifts, and stays in the mode it began.

/** Moving this far (CSS px) in direct mode turns a touch into a drag, and two fingers into
 * a scroll or pinch; a touch that moved less and lifted at once is a tap. */
export const SLOP = 10;
/** Holding still this long (ms) right-clicks, in direct mode. */
export const LONG_PRESS = 550;
/** A tap this soon (ms; GTK's and Qt's double-click time) and this close (CSS px) after
 * the last one clicks the same spot, so double-taps double-click though the finger lands
 * a little off. */
export const MULTI_TAP_MS = 400;
export const MULTI_TAP_PX = 24;
/** Moving less than this (CSS px) keeps a trackpad touch a tap or a hold. */
export const STILL = 4;
/** A trackpad touch that moved less than SLOP and lifted within this many ms is a tap. */
export const TAP_MS = 200;
/** Holding still this long (ms) grabs, in trackpad mode. */
export const HOLD = 400;

export type Mode = 'direct' | 'trackpad';

/** The desktop the fingers drive: its size and desktop pixels per CSS pixel on screen. */
export interface Env {
	w: number;
	h: number;
	perPx: number;
}

/** What the page does for the gesture. */
export type Out =
	/** Move the pointer to a client position (CSS px). */
	| { k: 'at'; x: number; y: number }
	/** Move the pointer to a desktop position (px). */
	| { k: 'to'; x: number; y: number }
	| { k: 'button'; n: 1 | 3; down: boolean }
	/** The fingers moved the content by this much (CSS px): the view, then the desktop. */
	| { k: 'pan'; dx: number; dy: number }
	/** A pinch is measured from here: the fingers' centroid and spread. */
	| { k: 'pinch-begin'; x: number; y: number; spread: number }
	| { k: 'pinch'; x: number; y: number; spread: number }
	/** A short tick for the finger. */
	| { k: 'buzz' };

interface P {
	x: number;
	y: number;
}
interface Stamp extends P {
	t: number;
}

/** What two fingers that land and lift without moving do: a right click when they came on
 * a trackpad touch (the trackpad's two-finger tap), nothing on the desktop itself. */
type Lift = 'rightClick' | 'nothing';

/** What lifting two fingers does when the second came on a first that had not moved yet,
 * by the mode the first began in. */
const PENDING_LIFT: Readonly<Record<Mode, Lift>> = {
	trackpad: 'rightClick',
	direct: 'nothing',
};

/** What a second finger landing makes of the gesture `g` it lands in: what lifting both
 * then does. Every gesture has its answer, so a new one must give its own. */
function liftOf(g: Gesture): Lift {
	switch (g.k) {
		case 'pending':
			return PENDING_LIFT[g.mode];
		// The trackpad pointer was sliding: a second finger still makes a tap.
		case 'slide':
			return 'rightClick';
		case 'idle':
		case 'drag':
		case 'spent':
		case 'two':
		case 'scroll':
		case 'pinch':
			return 'nothing';
	}
}

/** What the fingers are doing. */
type Gesture =
	| { k: 'idle' }
	/** One finger down, not moved enough to be anything yet. In trackpad mode the pointer
	 * was put down as the finger came: `from` is where. */
	| { k: 'pending'; mode: 'direct'; at: Stamp }
	| { k: 'pending'; mode: 'trackpad'; at: Stamp; from: P }
	/** The left button is held and the finger moves the pointer. */
	| { k: 'drag'; mode: Mode; last: Stamp }
	/** Trackpad: the finger slides the pointer; `far` is the most it has moved from where
	 * it began, `from` the pointer then. */
	| { k: 'slide'; at: Stamp; from: P; far: number; last: Stamp }
	/** Nothing more comes of it: a click was made, or fingers came and went. */
	| { k: 'spent' }
	/** Two fingers down, not yet moved enough to tell a scroll from a pinch, or from a tap
	 * that does what `onLift` says when they lift. */
	| { k: 'two'; onLift: Lift; startMid: P; startSpread: number }
	| { k: 'scroll'; lastMid: P }
	| { k: 'pinch' };

export function createGestures(out: (o: Out) => void) {
	const fingers = new Map<number, P>();
	let g: Gesture = { k: 'idle' };
	let lastTap: Stamp = { x: 0, y: 0, t: -Infinity };
	/** The trackpad pointer, in desktop pixels; none until it is first put down. */
	let cursor: P | null = null;
	/** The mode the next gesture starts in. */
	let mode: Mode = 'direct';
	/** The desktop; none while there is none to drive (no gesture starts). */
	let env: Env | null = null;

	const mid = (): P => {
		let x = 0,
			y = 0;
		for (const p of fingers.values()) {
			x += p.x;
			y += p.y;
		}
		return { x: x / fingers.size, y: y / fingers.size };
	};
	/** Mean distance of the fingers from their centroid (half the gap for two). */
	const spread = () => {
		const m = mid();
		let d = 0;
		for (const p of fingers.values()) d += Math.hypot(p.x - m.x, p.y - m.y);
		return d / fingers.size;
	};
	const button = (n: 1 | 3, pressed: boolean) =>
		out({ k: 'button', n, down: pressed });
	const click = (n: 1 | 3) => {
		button(n, true);
		button(n, false);
	};
	const to = (p: P) => out({ k: 'to', x: p.x, y: p.y });
	/** Two fingers lifted without moving. */
	function lifted(lift: Lift): Lift {
		switch (lift) {
			case 'rightClick':
				click(3);
				return lift;
			case 'nothing':
				return lift;
		}
	}

	/** Puts the trackpad pointer down on the desktop `e` (in its middle at first) and says
	 * where it is: the desktop's pointer may have been moved since. */
	function put(e: Env): P {
		const c = cursor === null ? { x: e.w / 2, y: e.h / 2 } : cursor;
		const at = {
			x: Math.max(0, Math.min(e.w - 1, c.x)),
			y: Math.max(0, Math.min(e.h - 1, c.y)),
		};
		cursor = at;
		to(at);
		return at;
	}
	/** Puts the trackpad pointer down, if there is a desktop to put it on. */
	function place() {
		if (env !== null) put(env);
	}
	/** What a first finger at `at` begins, on the desktop `e`. */
	function begun(at: Stamp, e: Env): Gesture {
		switch (mode) {
			case 'direct':
				return { k: 'pending', mode, at };
			case 'trackpad':
				return { k: 'pending', mode, at, from: put(e) };
		}
	}
	/** Moves the trackpad pointer by a finger movement (CSS px) made over `dt` ms: slow
	 * slides move it less than the finger, for precision; fast ones more. */
	function nudge(dx: number, dy: number, dt: number) {
		const e = env;
		if (e === null || cursor === null) return;
		const speed = Math.hypot(dx, dy) / Math.max(dt, 1);
		const gain = Math.min(3, Math.max(0.6, speed * 1.5)) * e.perPx;
		cursor = {
			x: Math.max(0, Math.min(e.w - 1, cursor.x + dx * gain)),
			y: Math.max(0, Math.min(e.h - 1, cursor.y + dy * gain)),
		};
		to(cursor);
	}

	/** Measure two or more fingers from where they are now (one was added or lifted). */
	function rebase() {
		const m = mid(),
			s = spread();
		if (g.k === 'two') g = { ...g, startMid: m, startSpread: s };
		else if (g.k === 'scroll') g = { k: 'scroll', lastMid: m };
		else if (g.k === 'pinch')
			out({ k: 'pinch-begin', x: m.x, y: m.y, spread: s });
	}

	/** When the finger has held still long enough: the time to `tick` at; none if
	 * nothing waits on time. */
	function due(): number | null {
		if (g.k !== 'pending') return null;
		return g.at.t + (g.mode === 'direct' ? LONG_PRESS : HOLD);
	}
	function tick(t: number) {
		const d = due();
		if (d === null || t < d || g.k !== 'pending') return;
		if (g.mode === 'direct') {
			out({ k: 'at', x: g.at.x, y: g.at.y });
			click(3);
			g = { k: 'spent' };
		} else {
			button(1, true);
			g = { k: 'drag', mode: 'trackpad', last: { ...g.at, t: d } };
		}
		out({ k: 'buzz' });
	}

	function down(id: number, x: number, y: number, t: number) {
		if (fingers.has(id)) return;
		if (fingers.size === 0) {
			// No gesture starts with no desktop to drive.
			if (env === null) return;
			tick(t);
			fingers.set(id, { x, y });
			g = begun({ x, y, t }, env);
			return;
		}
		tick(t);
		fingers.set(id, { x, y });
		if (fingers.size === 2) {
			const onLift = liftOf(g);
			if (g.k === 'drag') button(1, false);
			g = { k: 'two', onLift, startMid: mid(), startSpread: spread() };
		} else rebase();
	}

	function move(id: number, x: number, y: number, t: number) {
		const p = fingers.get(id);
		if (!p) return;
		tick(t);
		p.x = x;
		p.y = y;
		if (fingers.size === 1) {
			if (g.k === 'pending') {
				const far = Math.hypot(x - g.at.x, y - g.at.y);
				if (g.mode === 'direct' && far > SLOP) {
					out({ k: 'at', x: g.at.x, y: g.at.y });
					button(1, true);
					out({ k: 'at', x, y });
					g = { k: 'drag', mode: 'direct', last: { x, y, t } };
				} else if (g.mode === 'trackpad' && far > STILL) {
					const { at, from } = g;
					g = { k: 'slide', at, from: { ...from }, far, last: { x, y, t } };
					nudge(x - at.x, y - at.y, t - at.t);
				}
			} else if (g.k === 'slide') {
				const { at, last } = g;
				const far = Math.max(g.far, Math.hypot(x - at.x, y - at.y));
				g = { ...g, far, last: { x, y, t } };
				nudge(x - last.x, y - last.y, t - last.t);
			} else if (g.k === 'drag') {
				const { mode: dragging, last } = g;
				g = { k: 'drag', mode: dragging, last: { x, y, t } };
				if (dragging === 'direct') out({ k: 'at', x, y });
				else nudge(x - last.x, y - last.y, t - last.t);
			}
			return;
		}
		if (g.k === 'two') {
			// A pinch changes the fingers' gap faster than it moves their centroid
			// (twice as fast with one finger still); a scroll moves them together.
			const m = mid();
			const moved = Math.hypot(m.x - g.startMid.x, m.y - g.startMid.y);
			const gap = 2 * Math.abs(spread() - g.startSpread);
			if (gap > SLOP && gap > moved) {
				g = { k: 'pinch' };
				rebase();
			} else if (moved > SLOP) g = { k: 'scroll', lastMid: g.startMid };
		}
		if (g.k === 'pinch') {
			const m = mid();
			out({ k: 'pinch', x: m.x, y: m.y, spread: spread() });
		} else if (g.k === 'scroll') {
			const m = mid();
			out({ k: 'pan', dx: m.x - g.lastMid.x, dy: m.y - g.lastMid.y });
			g = { k: 'scroll', lastMid: m };
		}
	}

	function up(id: number, t: number) {
		if (!fingers.has(id)) return;
		tick(t);
		fingers.delete(id);
		if (g.k === 'pending') {
			if (g.mode === 'direct') {
				const { at } = g;
				const near =
					t - lastTap.t < MULTI_TAP_MS &&
					Math.hypot(at.x - lastTap.x, at.y - lastTap.y) < MULTI_TAP_PX;
				const spot = near ? lastTap : at;
				lastTap = { x: spot.x, y: spot.y, t };
				out({ k: 'at', x: spot.x, y: spot.y });
			}
			click(1);
			g = { k: 'spent' };
		} else if (g.k === 'slide') {
			// A tap that wobbled: undo the wobble and click where the pointer was.
			if (g.far < SLOP && t - g.at.t < TAP_MS) {
				cursor = g.from;
				to(cursor);
				click(1);
			}
			g = { k: 'spent' };
		} else if (g.k === 'drag') {
			button(1, false);
			g = { k: 'spent' };
		} else if (g.k === 'two' || g.k === 'scroll' || g.k === 'pinch') {
			if (fingers.size >= 2) rebase();
			else {
				if (g.k === 'two') lifted(g.onLift);
				g = { k: 'spent' };
			}
		}
		if (fingers.size === 0) g = { k: 'idle' };
	}

	function cancel(id: number, t: number) {
		if (!fingers.has(id)) return;
		tick(t);
		if (g.k === 'drag') button(1, false);
		fingers.delete(id);
		g = fingers.size ? { k: 'spent' } : { k: 'idle' };
	}

	const api = {
		get mode(): Mode {
			return mode;
		},
		set mode(next: Mode) {
			mode = next;
		},
		get env(): Env | null {
			return env;
		},
		set env(next: Env | null) {
			env = next;
		},
		down,
		move,
		up,
		cancel,
		tick,
		place,
		/** The time to `tick` at; none if nothing waits on time. */
		get due() {
			return due();
		},
		/** The trackpad pointer in desktop pixels, if it has been put down. */
		get cursor(): Readonly<P> | null {
			return cursor;
		},
		/** The left button is held by a grab (the trackpad pointer is drawn with a ring). */
		get grabbing() {
			return g.k === 'drag' && g.mode === 'trackpad';
		},
		/** Whether no finger is down. */
		get idle() {
			return g.k === 'idle';
		},
	};
	return api;
}

export type Gestures = ReturnType<typeof createGestures>;
