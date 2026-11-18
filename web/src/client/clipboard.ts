// Clipboard from the desktop to the browser. Browsers only allow clipboard writes
// while the page has focus, so a pending write waits for the next interaction.
import type { App } from './app';

export function createClipboard(app: App) {
	let last: string | null = null;
	let pending: string | Blob | null = null;

	function flush() {
		// Nothing to write, or not allowed now: a frame without focus may not write.
		if (pending === null || (app.inIframe && !document.hasFocus())) return;
		void write(pending);
	}
	async function write(c: string | Blob) {
		try {
			if (typeof c === 'string') await navigator.clipboard.writeText(c);
			else
				await navigator.clipboard.write([
					new ClipboardItem({ 'image/png': c }),
				]);
			if (pending === c) pending = null;
		} catch (e) {
			// Chromium refuses a clipboard write while the document has no focus
			// (NotAllowedError, "Document is not focused"): it stays pending, and goes at the
			// next interaction or focus.
			if (!(e instanceof DOMException && e.name === 'NotAllowedError')) throw e;
		}
	}
	window.addEventListener('focus', flush);

	return {
		flush,
		onRemoteText(text: string) {
			if (text === last) return;
			last = text;
			pending = text;
			flush();
		},
		/** Clipboard image message: type byte, then PNG. */
		onRemoteImage(buf: ArrayBuffer) {
			pending = new Blob([new Uint8Array(buf, 1)], { type: 'image/png' });
			flush();
		},
		/** Text the user pasted locally, so it isn't echoed back when the desktop reports it. */
		noteLocal(text: string) {
			last = text;
		},
	};
}

export type Clipboard = ReturnType<typeof createClipboard>;
