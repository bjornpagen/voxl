// The trackpad pointer, drawn on the surface: touch browsers show no CSS cursor, and in
// trackpad mode the pointer is not under the finger (see touch.ts).
import type { App } from './app';
import type { CursorImage } from './cursor';
import type { Zoom } from './zoom';

/** Size (CSS px) of the dot drawn when the desktop hides its cursor; see #tp-pointer.dot. */
const DOT = 12;

export function createPointerView(
	app: App,
	cursor: { image(): CursorImage | null },
	zoom: Zoom
) {
	const ptr = document.createElement('div');
	ptr.id = 'tp-pointer';
	app.surface.appendChild(ptr);
	// The cursor image last drawn; none until the first draw (which sets it, or the dot).
	let shown: string | null = null;

	return {
		/** Draws the pointer at a desktop position (hidden if none), with a ring while the
		 * left button is held. */
		draw(at: { x: number; y: number } | null, grabbing: boolean) {
			const desk = app.desk;
			if (!at || desk.s === 'none') {
				ptr.style.display = 'none';
				return;
			}
			ptr.style.display = 'block';
			// A hidden remote cursor still gets a dot, or the pointer would be lost.
			const img = cursor.image();
			const url = img?.url ?? '';
			if (url !== shown) {
				shown = url;
				ptr.classList.toggle('dot', !img);
				ptr.style.backgroundImage = img ? `url(${img.url})` : '';
				ptr.style.width = img ? `${img.w}px` : '';
				ptr.style.height = img ? `${img.h}px` : '';
			}
			const hx = img ? img.hx : DOT / 2,
				hy = img ? img.hy : DOT / 2;
			ptr.style.setProperty('--hx', `${hx}px`);
			ptr.style.setProperty('--hy', `${hy}px`);
			// The surface is scaled by pinch zoom; keep the pointer its usual size.
			const s = app.view.scale;
			const x = Math.min(at.x, desk.w - 1),
				y = Math.min(at.y, desk.h - 1);
			ptr.style.transform = `translate(${x * s - hx}px, ${y * s - hy}px) scale(${1 / zoom.scale()})`;
			ptr.classList.toggle('down', grabbing);
		},
	};
}
