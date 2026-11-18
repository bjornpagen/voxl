// Dynamic resolution: ask the server for a desktop that matches this window, so it
// is shown 1:1 instead of being scaled down. Asked once a connection, at its first
// hello, and, once a frame, when the window changes size (sidebar drags, fullscreen,
// rotating a tablet); never again at a later hello. A hello follows every resize:
// asking at each would move the desktop between two pages' sizes for as long as both
// are open, and ask again after every refusal for a size the server cannot give.
import { cmd } from '../protocol';
import type { App } from './app';

export function createResize(app: App) {
	/** Whether the server resizes its desktop to this window, at what scale. */
	let resizes: { s: 'off' } | { s: 'on'; scale: number } = { s: 'off' };
	/** What this connection has asked for, as `w,h`; none until it asks. */
	let last: string | null = null;
	/** Whether a check of the window's size waits for the next frame. */
	let queued = false;

	function send() {
		// A hidden window has no size to ask for.
		if (resizes.s === 'off' || !window.innerWidth || !window.innerHeight)
			return;
		const s = resizes.scale;
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
		resizes = cfg === undefined ? { s: 'off' } : { s: 'on', scale: cfg.scale };
		send();
	}

	/** The connection is gone: the next one has asked for nothing. */
	function closed() {
		last = null;
	}

	// A drag resizes the window many times a frame: the newest size is asked once a frame,
	// and the server applies only the newest ask it has.
	window.addEventListener('resize', () => {
		if (queued) return;
		queued = true;
		requestAnimationFrame(() => {
			queued = false;
			send();
		});
	});
	return { setup, closed };
}
