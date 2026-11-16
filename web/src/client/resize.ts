// Dynamic resolution: ask the server for a desktop that matches this window, so it
// is shown 1:1 instead of being scaled down. Asked once a connection, at its first
// hello, and, debounced, when the window changes size (sidebar drags, fullscreen,
// rotating a tablet); never again at a later hello. A hello follows every resize:
// asking at each would move the desktop between two pages' sizes for as long as both
// are open, and ask again after every refusal for a size the server cannot give.
import { cmd } from '../protocol';
import type { App } from './app';

const DEBOUNCE_MS = 250;

export function createResize(app: App) {
	let scale = 0;
	// What this connection has asked for, as `w,h`; '' until it asks.
	let last = '';
	let timer: ReturnType<typeof setTimeout> | undefined;
	// ?scale= overrides the server's desktop-pixels-per-CSS-pixel (e.g. 2 for sharper text).
	const override = Number(new URLSearchParams(location.search).get('scale'));

	function send() {
		if (!scale || !window.innerWidth || !window.innerHeight) return;
		const s = override > 0 ? override : scale;
		const w = Math.round(window.innerWidth * s);
		const h = Math.round(window.innerHeight * s);
		const key = `${w},${h}`;
		if (key === last) return;
		last = key;
		app.send(cmd.resize(w, h));
	}

	/** From each hello: whether the server resizes (and at what scale). Only the
	 * connection's first ask, or a scale that changes the size, sends anything. */
	function setup(cfg: { scale: number } | undefined) {
		scale = cfg?.scale ?? 0;
		send();
	}

	/** The connection is gone: the next one has asked for nothing. */
	function closed() {
		last = '';
	}

	window.addEventListener('resize', () => {
		clearTimeout(timer);
		timer = setTimeout(send, DEBOUNCE_MS);
	});
	return { setup, closed };
}
