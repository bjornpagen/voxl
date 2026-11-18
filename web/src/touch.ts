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

/** What the fingers are doing. */
type Gesture =
	| { k: 'idle' }
	/** One finger down, not moved enough to be anything yet. */
	| { k: 'pending'; mode: Mode; at: Stamp }
	/** The left button is held and the finger moves the pointer. */
	| { k: 'drag'; mode: Mode; last: Stamp }
	/** Trackpad: the finger slides the pointer; `far` is the most it has moved from where
	 * it began, `from` the pointer then. */
	| { k: 'slide'; at: Stamp; from: P; far: number; last: Stamp }
	/** Nothing more comes of it: a click was made, or fingers came and went. */
	| { k: 'spent' }
	/** Two fingers down, not yet moved enough to tell a scroll from a pinch (or, in
	 * trackpad mode, from a right-click tap). */
	| { k: 'two'; rightTap: boolean; startMid: P; startSpread: number }
	| { k: 'scroll'; lastMid: P }
	| { k: 'pinch' };

export function createGestures(out: (o: Out) => void) {
	const fingers = new Map<number, P>();
	let g: Gesture = { k: 'idle' };
	let lastTap: Stamp = { x: 0, y: 0, t: -Infinity };
	/** The trackpad pointer, in desktop pixels; none until it is first put down. */
	let cursor: P | null = null;

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

	/** Puts the trackpad pointer down (in the middle of the desktop at first) and says
	 * where it is: the desktop's pointer may have been moved since. */
	function place() {
		const e = api.env;
		if (!e) return;
		const c = cursor ?? { x: e.w / 2, y: e.h / 2 };
		cursor = {
			x: Math.max(0, Math.min(e.w - 1, c.x)),
			y: Math.max(0, Math.min(e.h - 1, c.y)),
		};
		to(cursor);
	}
	/** Moves the trackpad pointer by a finger movement (CSS px) made over `dt` ms: slow
	 * slides move it less than the finger, for precision; fast ones more. */
	function nudge(dx: number, dy: number, dt: number) {
		const e = api.env;
		if (!e || !cursor) return;
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
		if (fingers.has(id) || (fingers.size === 0 && !api.env)) return;
		tick(t);
		fingers.set(id, { x, y });
		if (fingers.size === 1) {
			const mode = api.mode;
			if (mode === 'trackpad') place();
			g = { k: 'pending', mode, at: { x, y, t } };
		} else if (fingers.size === 2) {
			const rightTap =
				(g.k === 'pending' && g.mode === 'trackpad') || g.k === 'slide';
			if (g.k === 'drag') button(1, false);
			g = { k: 'two', rightTap, startMid: mid(), startSpread: spread() };
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
					const { at } = g;
					g = {
						k: 'slide',
						at,
						from: { ...(cursor as P) },
						far,
						last: { x, y, t },
					};
					nudge(x - at.x, y - at.y, t - at.t);
				}
			} else if (g.k === 'slide') {
				const { at, last } = g;
				const far = Math.max(g.far, Math.hypot(x - at.x, y - at.y));
				g = { ...g, far, last: { x, y, t } };
				nudge(x - last.x, y - last.y, t - last.t);
			} else if (g.k === 'drag') {
				const { mode, last } = g;
				g = { k: 'drag', mode, last: { x, y, t } };
				if (mode === 'direct') out({ k: 'at', x, y });
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
				if (g.k === 'two' && g.rightTap) click(3);
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
		/** The mode the next gesture starts in. */
		mode: 'direct' as Mode,
		/** The desktop; none while there is none to drive (no gesture starts). */
		env: null as Env | null,
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
