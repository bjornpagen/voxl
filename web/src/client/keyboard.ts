// Keyboard, IME and paste input. Keys are sent as X keysyms; the server maps them to
// keycodes, so the browser's layout decides what each key types.
import { Bin, cmd } from '../protocol';
import type { App } from './app';
import { charKeysym, codePoints, Keys, type KeyEvent } from '../keys';
import type { Clipboard } from './clipboard';

/** A newline's code point. */
const NEWLINE = 0x0a;
const XK_BACKSPACE = 0xff08,
	XK_RETURN = 0xff0d,
	XK_DELETE = 0xffff;

async function toPng(file: File): Promise<ArrayBuffer> {
	if (file.type === 'image/png') return file.arrayBuffer();
	const bmp = await createImageBitmap(file);
	const c = new OffscreenCanvas(bmp.width, bmp.height);
	const context = c.getContext('2d');
	if (context === null) throw new Error('voxl: a canvas without a 2D context');
	context.drawImage(bmp, 0, 0);
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
		const dt = e.clipboardData;
		// A paste that carries no clipboard pastes nothing: the shortcut goes as it is.
		if (dt === null) return flushPaste();
		const text = dt.getData('text/plain');
		if (text) {
			clipboard.noteLocal(text);
			send(cmd.clipboard(text));
			flushPaste();
			return;
		}
		const image = [...dt.items].find(
			(i) => i.kind === 'file' && i.type.startsWith('image/')
		);
		// No image either, or one the browser gives no file for: nothing to paste.
		if (image === undefined) return flushPaste();
		const file = image.getAsFile();
		if (file === null) return flushPaste();
		// The paste shortcut waits for the image to reach the remote clipboard.
		toPng(file)
			.then((png) => app.sendBin(Bin.clipImageUp, new Uint8Array(png)))
			.catch((err) => console.warn('[voxl] image paste failed', err))
			.finally(flushPaste);
	});

	// Text that arrives without usable keydowns (IME, dead-key composition, mobile
	// keyboards, insertText) is typed as individual keysyms.
	function typeText(text: string) {
		for (const cp of codePoints(text))
			keys.tap(cp === NEWLINE ? XK_RETURN : charKeysym(cp));
	}
	kbd.addEventListener('compositionend', (e) => {
		if (e.data) typeText(e.data);
		kbd.value = '';
	});
	// On-screen keyboards often deliver text and edits only as input events: what each
	// kind of edit the page acts on types. An edit of any other kind types nothing.
	// What an edit carries is its text, or none (the DOM's null) for one that is no text.
	const edits = new Map<string, (data: string | null) => void>([
		[
			'insertText',
			(data) => {
				if (data !== null) typeText(data);
			},
		],
		['deleteContentBackward', () => keys.tap(XK_BACKSPACE)],
		['deleteContentForward', () => keys.tap(XK_DELETE)],
		['insertLineBreak', () => keys.tap(XK_RETURN)],
		['insertParagraph', () => keys.tap(XK_RETURN)],
	]);
	kbd.addEventListener('input', (e) => {
		if (!(e instanceof InputEvent) || e.isComposing) return;
		const edit = edits.get(e.inputType);
		if (edit !== undefined) edit(e.data);
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
