// Mouse, wheel/trackpad and touch input on the desktop surface.
import { cmd } from '../protocol';
import type { App } from './app';
import type { Cursor } from './cursor';
import { createTouch } from './touch';
import type { Zoom } from './zoom';

/** DOM button -> X button (left, middle, right, back, forward). */
const BUTTONS = [1, 2, 3, 8, 9];

export function setupPointer(
	app: App,
	cursor: Cursor,
	zoom: Zoom,
	onInteract: () => void
) {
	const { surface, send } = app;

	function toDesk(
		p: { clientX: number; clientY: number },
		w: number,
		h: number
	): [number, number] {
		const r = surface.getBoundingClientRect();
		const x = Math.round(((p.clientX - r.left) * w) / r.width);
		const y = Math.round(((p.clientY - r.top) * h) / r.height);
		return [Math.max(0, Math.min(w - 1, x)), Math.max(0, Math.min(h - 1, y))];
	}
	/** A DOM button pressed or released, as the desktop's; a button the desktop has no
	 * number for (a pen's eraser) presses nothing. */
	function button(dom: number, down: boolean) {
		const x = BUTTONS[dom];
		if (x !== undefined) send(cmd.button(x, down));
	}
	let lastMove = '';
	function move(p: { clientX: number; clientY: number }, force = false) {
		const desk = app.desk;
		// No desktop, no place on it to move to.
		if (desk.s === 'none') return;
		const s = cmd.move(...toDesk(p, desk.w, desk.h));
		if (s !== lastMove || force) {
			lastMove = s;
			send(s);
		}
	}

	// Scroll is sent in desktop pixels, batched per frame; the server turns it into
	// smooth (pixel-precise) scrolling, or wheel clicks on X servers that can't.
	let sx = 0,
		sy = 0,
		queued = false;
	function queueScroll(dx: number, dy: number) {
		sx += dx;
		sy += dy;
		if (queued) return;
		queued = true;
		requestAnimationFrame(() => {
			queued = false;
			if (Math.abs(sx) < 0.5 && Math.abs(sy) < 0.5) return;
			send(cmd.scroll(sx, sy));
			sx = 0;
			sy = 0;
		});
	}
	/** Desktop pixels per on-screen pixel; none without a desktop. */
	const deskPerPx = () =>
		app.desk.s === 'shown'
			? app.desk.w / surface.getBoundingClientRect().width
			: null;

	const touch = createTouch(app, {
		moveTo: (x, y) => move({ clientX: x, clientY: y }, true),
		moveToDesk(x, y) {
			lastMove = cmd.move(Math.round(x), Math.round(y));
			send(lastMove);
		},
		queueScroll,
		deskPerPx,
		cursor,
		zoom,
	});

	surface.addEventListener('pointermove', (e) => {
		if (e.pointerType === 'touch') touch.move(e);
		else move(e);
	});
	surface.addEventListener('pointerdown', (e) => {
		e.preventDefault();
		onInteract();
		// A finger is the touch machine's: it never focuses the keyboard (only the
		// toolbar's keyboard button raises the on-screen one).
		if (e.pointerType === 'touch') {
			touch.down(e);
			return;
		}
		app.kbd.focus({ preventScroll: true });
		move(e);
		surface.setPointerCapture(e.pointerId);
		button(e.button, true);
	});
	surface.addEventListener('pointerup', (e) => {
		e.preventDefault();
		if (e.pointerType === 'touch') {
			touch.up(e);
			return;
		}
		move(e);
		button(e.button, false);
	});
	surface.addEventListener('pointercancel', (e) => {
		if (e.pointerType === 'touch') touch.cancel(e);
		else send(cmd.releaseAll);
	});
	surface.addEventListener('contextmenu', (e) => e.preventDefault());
	surface.addEventListener(
		'wheel',
		(e) => {
			e.preventDefault();
			move(e);
			const perPx = deskPerPx();
			// Without a desktop there is nothing to scroll.
			if (perPx === null) return;
			const k = (e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1) * perPx;
			queueScroll(e.deltaX * k, e.deltaY * k);
		},
		{ passive: false }
	);
}
