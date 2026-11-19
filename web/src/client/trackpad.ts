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
	/** What the pointer is drawn as: nothing yet, the dot, or the cursor's image. */
	let drawn: { s: 'nothing' } | { s: 'dot' } | { s: 'image'; url: string } = {
		s: 'nothing',
	};
	/** Draws the dot, and says where its hot spot is. A hidden remote cursor still gets one,
	 * or the pointer would be lost. */
	function dot() {
		const hot = { hx: DOT / 2, hy: DOT / 2 };
		switch (drawn.s) {
			case 'dot':
				return hot;
			case 'nothing':
			case 'image':
				drawn = { s: 'dot' };
				ptr.classList.add('dot');
				ptr.style.backgroundImage = '';
				ptr.style.width = '';
				ptr.style.height = '';
				return hot;
		}
	}
	/** Whether the pointer is drawn as `img` already. */
	function drawnAs(img: CursorImage): boolean {
		switch (drawn.s) {
			case 'image':
				return drawn.url === img.url;
			case 'nothing':
			case 'dot':
				return false;
		}
	}
	/** Draws the cursor's image, and says where its hot spot is. */
	function picture(img: CursorImage) {
		if (!drawnAs(img)) {
			drawn = { s: 'image', url: img.url };
			ptr.classList.remove('dot');
			ptr.style.backgroundImage = `url(${img.url})`;
			ptr.style.width = `${img.w}px`;
			ptr.style.height = `${img.h}px`;
		}
		return { hx: img.hx, hy: img.hy };
	}

	return {
		/** Draws the pointer at a desktop position (hidden if none), with a ring while the
		 * left button is held. */
		draw(at: { x: number; y: number } | null, grabbing: boolean) {
			const desk = app.desk;
			if (at === null || desk.s === 'none') {
				ptr.style.display = 'none';
				return;
			}
			ptr.style.display = 'block';
			const img = cursor.image();
			const { hx, hy } = img === null ? dot() : picture(img);
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
