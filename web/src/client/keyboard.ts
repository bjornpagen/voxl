// Keyboard, IME and paste input. Keys are sent as X keysyms; the server maps them to
// keycodes, so the browser's layout decides what each key types.
import { Bin, cmd } from '../protocol';
import type { App } from './app';
import { charKeysym, Keys, type KeyEvent } from '../keys';
import type { Clipboard } from './clipboard';

const XK_BACKSPACE = 0xff08,
	XK_RETURN = 0xff0d,
	XK_DELETE = 0xffff;

async function toPng(file: File): Promise<ArrayBuffer> {
	if (file.type === 'image/png') return file.arrayBuffer();
	const bmp = await createImageBitmap(file);
	const c = new OffscreenCanvas(bmp.width, bmp.height);
	c.getContext('2d')!.drawImage(bmp, 0, 0);
	return (await c.convertToBlob({ type: 'image/png' })).arrayBuffer();
}

const keyEvent = (e: KeyboardEvent): KeyEvent => ({
	code: e.code,
	key: e.key,
	repeat: e.repeat,
	isComposing: e.isComposing,
	ctrlKey: e.ctrlKey,
	metaKey: e.metaKey,
	shiftKey: e.shiftKey,
	altKey: e.altKey,
	altGraph: e.getModifierState('AltGraph'),
});

export function setupKeyboard(
	app: App,
	clipboard: Clipboard,
	onInteract: () => void
) {
	const { kbd, surface, send, isMac } = app;
	const keys = new Keys(isMac, {
		key: (ks, act) => send(cmd.key(ks, act)),
		releaseAll: () => send(cmd.releaseAll),
	});
	/** The paste shortcut held back until the browser's paste event (or the key's release)
	 * has put the local clipboard on the desktop; none while no paste is pending. */
	let pendingPaste: { code: string; ks: number } | null = null;
	function releaseAll() {
		pendingPaste = null; // a paste that lands after the release must not press a key
		keys.releaseAll();
	}
	function flushPaste() {
		if (!pendingPaste) return;
		const p = pendingPaste;
		pendingPaste = null;
		keys.hold(p.code, p.ks);
	}

	function onKeyDown(e: KeyboardEvent) {
		if (!e.isComposing) onInteract();
		const consumed = keys.keyDown(keyEvent(e), (k, ks) => {
			const accel = isMac ? k.metaKey : k.ctrlKey;
			if (!accel || k.altKey || (k.key !== 'v' && k.key !== 'V') || k.repeat)
				return false;
			// Let the browser deliver a paste event to the hidden textarea first, so the
			// remote clipboard holds the local contents before the shortcut lands.
			pendingPaste = { code: k.code, ks };
			return true;
		});
		if (consumed) e.preventDefault();
	}
	function onKeyUp(e: KeyboardEvent) {
		e.preventDefault();
		if (pendingPaste && pendingPaste.code === e.code) flushPaste();
		keys.keyUp(keyEvent(e));
	}
	for (const target of [kbd, surface]) {
		target.addEventListener('keydown', onKeyDown);
		target.addEventListener('keyup', onKeyUp);
	}

	kbd.addEventListener('paste', (e) => {
		e.preventDefault();
		// A paste event always carries its clipboard.
		const dt = e.clipboardData!;
		const text = dt.getData('text/plain');
		if (text) {
			clipboard.noteLocal(text);
			send(cmd.clipboard(text));
			flushPaste();
			return;
		}
		const file = [...dt.items]
			.find((i) => i.kind === 'file' && i.type.startsWith('image/'))
			?.getAsFile();
		if (!file) {
			flushPaste();
			return;
		}
		// The paste shortcut waits for the image to reach the remote clipboard.
		toPng(file)
			.then((png) => app.sendBin(Bin.clipImageUp, new Uint8Array(png)))
			.catch((err) => console.warn('[voxl] image paste failed', err))
			.finally(flushPaste);
	});

	// Text that arrives without usable keydowns (IME, dead-key composition, mobile
	// keyboards, insertText) is typed as individual keysyms.
	function typeText(text: string) {
		for (const ch of text)
			keys.tap(ch === '\n' ? XK_RETURN : charKeysym(ch.codePointAt(0)!));
	}
	kbd.addEventListener('compositionend', (e) => {
		if (e.data) typeText(e.data);
		kbd.value = '';
	});
	kbd.addEventListener('input', (e) => {
		const ie = e as InputEvent;
		if (ie.isComposing) return;
		// On-screen keyboards often deliver text and edits only as input events.
		if (ie.inputType === 'insertText' && ie.data) typeText(ie.data);
		else if (ie.inputType === 'deleteContentBackward') keys.tap(XK_BACKSPACE);
		else if (ie.inputType === 'deleteContentForward') keys.tap(XK_DELETE);
		else if (
			ie.inputType === 'insertLineBreak' ||
			ie.inputType === 'insertParagraph'
		)
			keys.tap(XK_RETURN);
		kbd.value = '';
	});
	window.addEventListener('blur', releaseAll);
	// A finger on the desktop must not raise the on-screen keyboard: only the toolbar's
	// keyboard button does (it sets kbd.dataset.soft). Mice and pens still focus it.
	// (tembo/voxl#95 by alexknowshtml)
	let lastTouch = false;
	surface.addEventListener(
		'pointerdown',
		(e) => {
			lastTouch = e.pointerType === 'touch';
		},
		{ capture: true }
	);
	surface.addEventListener('focus', () => {
		if (lastTouch && !kbd.dataset.soft) return;
		kbd.focus({ preventScroll: true });
	});
	if (!app.inIframe) kbd.focus({ preventScroll: true });

	return { releaseAll };
}
