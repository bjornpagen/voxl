// Toolbar (top right, on hover): files, microphone and fullscreen buttons.
import type { App } from './app';
import { el } from './app';

/** Why the browser refused fullscreen (it needs a user gesture). */
const fullscreen = (e: unknown) => console.warn('[voxl] fullscreen', e);

export function setupToolbar(app: App, onFullscreenChange: () => void) {
	const { tools } = app;
	const fsBtn = el<HTMLButtonElement>('fs-btn');

	// The toolbar shows while the pointer is over the corner it takes.
	document.addEventListener('pointermove', (e) => {
		const r = tools.getBoundingClientRect();
		tools.classList.toggle(
			'show',
			e.clientY <= r.bottom && e.clientX >= r.left
		);
	});
	// Clicks on the toolbar must not reach the desktop surface underneath.
	for (const b of tools.querySelectorAll('button'))
		b.addEventListener('pointerdown', (e) => e.stopPropagation());

	if (!document.fullscreenEnabled) fsBtn.style.display = 'none';
	fsBtn.addEventListener('click', () => {
		if (document.fullscreenElement) document.exitFullscreen().catch(fullscreen);
		else
			document.documentElement
				.requestFullscreen({ navigationUI: 'hide' })
				.catch(fullscreen);
	});
	// In fullscreen, capture browser shortcuts (Cmd/Ctrl+W, T, L, ...) for the desktop.
	// Keyboard Lock is Chromium-only and only allowed for the top-level page (an
	// embedding page has to lock on our behalf); hold Esc to leave fullscreen.
	document.addEventListener('fullscreenchange', () => {
		const kl = navigator.keyboard;
		// Refused in a frame the embedding page has not let lock the keyboard.
		if (document.fullscreenElement)
			kl.lock().catch((e: unknown) => console.warn('[voxl] keyboard lock', e));
		else kl.unlock();
		fsBtn.classList.toggle('on', !!document.fullscreenElement);
		onFullscreenChange();
	});
}
