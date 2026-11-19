// Dynamic resolution: ask the server for a desktop that matches this window, so it
// is shown 1:1 instead of being scaled down. Asked once a connection, at its first
// hello, and, once a frame, when the window changes size (sidebar drags, fullscreen,
// rotating a tablet); never again at a later hello. A hello follows every resize:
// asking at each would move the desktop between two pages' sizes for as long as both
// are open, and ask again after every refusal for a size the server cannot give.
import { cmd } from '../protocol';
import type { Resize } from '../protocol';
import type { App } from './app';

/** A desktop's size the window asks for, or none: no hello has said the scale yet, or the
 * window is hidden and has no size. */
type Ask =
	| { readonly s: 'nothing' }
	| { readonly s: 'size'; readonly w: number; readonly h: number };

const NOTHING: Ask = { s: 'nothing' };

/** What this connection's hellos have said of the desktop's size: nothing yet, or its
 * scale, as the last said it. */
type Said =
	| { readonly s: 'nothing' }
	| { readonly s: 'resize'; readonly resize: Resize };

/** The desktop this window wants, as the server resizes. */
function wanted(said: Said): Ask {
	switch (said.s) {
		case 'nothing':
			return NOTHING;
		case 'resize': {
			const w = Math.round(window.innerWidth * said.resize.scale);
			const h = Math.round(window.innerHeight * said.resize.scale);
			return w > 0 && h > 0 ? { s: 'size', w, h } : NOTHING;
		}
	}
}

/** What the server is told for `ask`, where `asked` was asked before: the size, if it is
 * one and not the one asked. */
function says(ask: Ask, asked: Ask): readonly string[] {
	switch (ask.s) {
		case 'nothing':
			return [];
		case 'size':
			switch (asked.s) {
				case 'nothing':
					return [cmd.resize(ask.w, ask.h)];
				case 'size':
					return ask.w === asked.w && ask.h === asked.h
						? []
						: [cmd.resize(ask.w, ask.h)];
			}
	}
}

export function createResize(app: App) {
	/** The scale the last hello said. */
	let said: Said = { s: 'nothing' };
	/** What this connection has asked for. */
	let asked: Ask = NOTHING;
	/** Whether a check of the window's size waits for the next frame. */
	let queued = false;

	function send() {
		const ask = wanted(said);
		for (const command of says(ask, asked)) {
			asked = ask;
			app.send(command);
		}
	}

	/** From each hello: the scale. Only the connection's first ask, or a scale that
	 * changes the size, sends anything. */
	function setup(resize: Resize) {
		said = { s: 'resize', resize };
		send();
	}

	/** The connection is gone: the next one has been said and has asked for nothing. */
	function closed() {
		said = { s: 'nothing' };
		asked = NOTHING;
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
