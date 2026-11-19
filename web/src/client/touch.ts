// Touch: the page's side of the gestures (touch.ts): it feeds the fingers to the machine
// with their time stamps, wakes it when a hold comes due, and does what it puts out:
// moves and buttons for the desktop, scrolling, and the pinch zoom of the view (zoom.ts).
// The toolbar's trackpad button switches the mode (direct touch, or trackpad), which is
// kept across visits; a gesture stays in the mode it began in.
import { cmd } from '../protocol';
import { createGestures } from '../touch';
import type { Out } from '../touch';
import type { App } from './app';
import { button } from './app';
import type { Cursor } from './cursor';
import { createPointerView } from './trackpad';
import type { Zoom } from './zoom';

const KEY = 'voxl.trackpad';
/** A zoomed-in view pans to keep the trackpad pointer at least this far (CSS px) inside
 * the window. */
const EDGE = 32;

interface Hooks {
	/** Move the remote pointer to a client (CSS pixel) position. */
	moveTo(x: number, y: number): void;
	/** Move the remote pointer to a desktop position. */
	moveToDesk(x: number, y: number): void;
	queueScroll(dx: number, dy: number): void;
	/** Desktop pixels per on-screen pixel; none without a desktop. */
	deskPerPx(): number | null;
	cursor: Cursor;
	zoom: Zoom;
}

export function createTouch(app: App, hooks: Hooks) {
	const { zoom } = hooks;
	const btn = button('tp-btn');
	const view = createPointerView(app, hooks.cursor, zoom);
	const machine = createGestures(apply);
	let timer: ReturnType<typeof setTimeout> | undefined;

	/** Does what the gestures say. What it returns says only that everything they say has
	 * its case. */
	function apply(o: Out): 'done' {
		switch (o.k) {
			case 'at':
				hooks.moveTo(o.x, o.y);
				return 'done';
			case 'to':
				hooks.moveToDesk(o.x, o.y);
				follow(o);
				return 'done';
			case 'button':
				app.send(cmd.button(o.n, o.down));
				return 'done';
			case 'pan': {
				// The content follows the fingers: a zoomed-in view moves, then the desktop scrolls.
				const [rx, ry] = zoom.pan(o.dx, o.dy);
				const k = hooks.deskPerPx();
				if (k !== null) hooks.queueScroll(-rx * k, -ry * k);
				return 'done';
			}
			case 'pinch-begin':
				zoom.begin(o.x, o.y, o.spread);
				return 'done';
			case 'pinch':
				zoom.pinch(o.x, o.y, o.spread);
				return 'done';
			case 'buzz':
				// Not every browser can buzz.
				if ('vibrate' in navigator) navigator.vibrate(10);
				return 'done';
		}
	}

	/** Pans a zoomed-in view so the pointer (desktop pixels) stays inside the window. */
	function follow(at: { x: number; y: number }) {
		const desk = app.desk;
		if (zoom.scale() === 1 || desk.s === 'none') return;
		const r = app.surface.getBoundingClientRect();
		const off = (v: number, size: number) =>
			v < EDGE ? EDGE - v : v > size - EDGE ? size - EDGE - v : 0;
		zoom.pan(
			off(r.left + (at.x * r.width) / desk.w, window.innerWidth),
			off(r.top + (at.y * r.height) / desk.h, window.innerHeight)
		);
	}

	const trackpad = () => machine.mode === 'trackpad';
	/** Runs a call of the machine, then draws the pointer and sets the timer a hold waits on. */
	function feed(f: () => void) {
		const perPx = hooks.deskPerPx();
		const desk = app.desk;
		machine.env =
			perPx === null || desk.s === 'none'
				? null
				: { w: desk.w, h: desk.h, perPx };
		f();
		view.draw(trackpad() ? machine.cursor : null, machine.grabbing);
		clearTimeout(timer);
		const due = machine.due;
		if (due !== null)
			timer = setTimeout(
				() => feed(() => machine.tick(performance.now())),
				Math.max(0, due - performance.now())
			);
	}
	/** The pointer is put down once the desktop is there, and drawn again as the layout,
	 * the zoom or the cursor image change. */
	function layout() {
		feed(() => {
			if (trackpad() && machine.idle && machine.cursor === null)
				machine.place();
		});
	}
	hooks.cursor.onChange(layout);

	function setTrackpad(on: boolean) {
		machine.mode = on ? 'trackpad' : 'direct';
		try {
			localStorage.setItem(KEY, on ? '1' : '0');
		} catch {}
		btn.classList.toggle('on', on);
		btn.setAttribute('aria-pressed', String(on));
		// The desktop's pointer may have been moved since: say where the pointer is.
		feed(() => {
			if (on && machine.idle) machine.place();
		});
	}
	btn.addEventListener('click', () => setTrackpad(!trackpad()));
	let saved = false;
	try {
		saved = localStorage.getItem(KEY) === '1';
	} catch {}
	machine.mode = saved ? 'trackpad' : 'direct';
	btn.classList.toggle('on', saved);
	btn.setAttribute('aria-pressed', String(saved));

	return {
		down(e: PointerEvent) {
			app.surface.setPointerCapture(e.pointerId);
			feed(() => machine.down(e.pointerId, e.clientX, e.clientY, e.timeStamp));
		},
		move(e: PointerEvent) {
			feed(() => machine.move(e.pointerId, e.clientX, e.clientY, e.timeStamp));
		},
		up(e: PointerEvent) {
			feed(() => machine.up(e.pointerId, e.timeStamp));
		},
		cancel(e: PointerEvent) {
			feed(() => machine.cancel(e.pointerId, e.timeStamp));
		},
	};
}
