// Toolbar (top right, on hover; bottom right and always visible when a touchscreen is the
// main pointer): on-screen keyboard, trackpad mode, files, microphone and fullscreen buttons.
import type { App } from './app';
import { button } from './app';

/** Why the browser refused fullscreen (it needs a user gesture). */
const fullscreen = (e: unknown) => console.warn('[voxl] fullscreen', e);

export function setupToolbar(app: App, onFullscreenChange: () => void) {
	const { tools, kbd } = app;
	const kbBtn = button('kb-btn');
	const fsBtn = button('fs-btn');

	// Any touchscreen gets the touch buttons (keyboard, trackpad mode). One that is the main
	// pointer (a phone, a tablet) keeps the toolbar always visible; with a mouse as well (a
	// laptop) the toolbar shows while a pointer or finger is over the corner it takes.
	if (matchMedia('(any-pointer: coarse)').matches) tools.classList.add('touch');
	if (matchMedia('(pointer: coarse)').matches)
		tools.classList.add('touch-first', 'show');
	else
		for (const type of ['pointermove', 'pointerdown'] as const)
			document.addEventListener(type, (p) => {
				const r = tools.getBoundingClientRect();
				tools.classList.toggle(
					'show',
					p.clientY <= r.bottom && p.clientX >= r.left
				);
			});
	// Clicks on the toolbar must not reach the desktop surface underneath.
	for (const b of tools.querySelectorAll('button'))
		b.addEventListener('pointerdown', (e) => e.stopPropagation());

	// The soft keyboard is raised by this button alone, and only it lowers it again; the
	// desktop never raises it (keyboard.ts).
	kbBtn.addEventListener('click', () => {
		if (document.activeElement === kbd && kbd.dataset.soft) {
			kbd.blur();
			delete kbd.dataset.soft;
		} else {
			kbd.dataset.soft = '1';
			kbd.focus({ preventScroll: true });
		}
	});

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
