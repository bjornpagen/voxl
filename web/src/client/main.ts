// Main thread: layout, input capture, cursor, clipboard, files, microphone and audio.
// Video decode/paint and the socket live in the worker (worker.ts).
import { cmd } from '../protocol';
import { Count, COUNTS } from '../protocol';
import type { FromWorker, Stats, ToWorker } from '../protocol';
import type { App } from './app';
import { el } from './app';
import { createAudio } from './audio';
import { createClipboard } from './clipboard';
import { createCursor } from './cursor';
import { createFiles } from './files';
import { setupKeyboard } from './keyboard';
import { createMic } from './mic';
import { setupPointer } from './pointer';
import { createResize } from './resize';
import { setupToolbar } from './toolbar';
import { createVideo } from './video';
import { createZoom } from './zoom';

/** What the page cannot run without, each by the name the refusal gives it. WebCodecs and
 * a secure context (the relay's 127.0.0.1 is one) for the stream; MediaStreamTrackGenerator
 * to present it; ClipboardItem for clipboard images; Keyboard Lock for the desktop's
 * shortcuts in fullscreen; cross-origin isolation (which the relay's headers give) for the
 * counters the worker shares with the page. */
const REQUIRED: [string, () => boolean][] = [
	['a secure context (HTTPS or localhost)', () => window.isSecureContext],
	['WebCodecs video decoding', () => 'VideoDecoder' in window],
	['WebCodecs audio decoding', () => 'AudioDecoder' in window],
	['WebCodecs audio encoding', () => 'AudioEncoder' in window],
	['MediaStreamTrackGenerator', () => 'MediaStreamTrackGenerator' in window],
	['ClipboardItem', () => 'ClipboardItem' in window],
	['the Keyboard Lock API', () => 'keyboard' in navigator],
	['cross-origin isolation', () => window.crossOriginIsolated],
];

function main() {
	const statusEl = el('status');

	// What the page needs, checked once: a browser without one of them is refused, by name,
	// rather than run with that part missing.
	const missing = REQUIRED.filter(([, has]) => !has()).map(([name]) => name);
	if (missing.length) {
		statusEl.textContent = `voxl needs a browser with ${missing.join(', ')}. Use a current Chrome or Edge on a desktop, at 127.0.0.1.`;
		return;
	}

	const wsUrl = new URL('ws', location.href);
	wsUrl.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
	const worker = new Worker('worker.js');
	const post = (m: ToWorker, transfer: Transferable[] = []) =>
		worker.postMessage(m, transfer);
	const app: App = {
		worker,
		post,
		send: (d) => {
			post({ t: 'send', d });
		},
		sendBin: (type, bytes) => {
			const msg = new Uint8Array(bytes.byteLength + 1);
			msg[0] = type;
			msg.set(bytes, 1);
			post({ t: 'sendBin', d: msg.buffer }, [msg.buffer]);
		},
		surface: el('surface'),
		kbd: el<HTMLTextAreaElement>('kbd'),
		tools: el('tools'),
		// navigator.platform is deprecated and empty in some browsers; the user agent names
		// Apple's platforms too.
		isMac: /Mac|iPhone|iPad/.test(
			`${navigator.platform} ${navigator.userAgent}`
		),
		inIframe: window !== window.top,
		desk: { s: 'none' },
		view: { scale: 1 },
	};

	const cursor = createCursor(app);
	const zoom = createZoom(app);
	const video = createVideo(app, () => {
		// The zoom is fitted first: the trackpad pointer is drawn at it (touch.ts).
		zoom.fit();
		cursor.apply();
	});
	const clipboard = createClipboard(app);
	const audio = createAudio(app);
	// Browsers only allow audio and clipboard writes after a user gesture.
	const onInteract = () => {
		audio.start();
		clipboard.flush();
	};
	setupPointer(app, cursor, zoom, onInteract);
	setupKeyboard(app, clipboard, onInteract);
	setupToolbar(app, video.layout);
	const files = createFiles(app);
	const mic = createMic(app);
	const resize = createResize(app);

	const counts = new Uint32Array(new SharedArrayBuffer(COUNTS * 4));
	post({ t: 'init', url: wsUrl.href, counts });
	/** What the decoder decodes, as the worker last said. */
	let codec: string | null = null;
	// Read by the bench tools (measure.mjs) and handy in the console: the worker's counters
	// since the last read, as rates and means, computed when read.
	let last = { at: performance.now(), counts: counts.slice() };
	Object.defineProperty(window, 'voxlStats', {
		get(): Stats {
			const now = { at: performance.now(), counts: counts.slice() };
			const d = (i: number) => (now.counts[i] - last.counts[i]) >>> 0;
			const dt = (now.at - last.at) / 1000;
			last = now;
			const frames = d(Count.Frames);
			const updates = d(Count.Updates);
			return {
				fps: frames / dt,
				kbps: (d(Count.Bytes) * 8) / 1000 / dt,
				decodeMs: frames ? d(Count.DecodeUs) / 1000 / frames : null,
				pipeMs: updates ? d(Count.PipeTenths) / 10 / updates : null,
				codec,
				passed: d(Count.Passed),
			};
		},
	});

	// Stop video when nobody can see it.
	let videoOn: boolean | null = null;
	function updateVisibility(force = false) {
		const on =
			!document.hidden && window.innerWidth > 0 && window.innerHeight > 0;
		if (on !== videoOn || force) {
			videoOn = on;
			post({ t: 'video', on });
		}
	}
	document.addEventListener('visibilitychange', () => {
		updateVisibility();
		if (document.hidden) app.send(cmd.releaseAll);
	});
	window.addEventListener('resize', () => {
		video.layout();
		updateVisibility();
	});
	updateVisibility();

	worker.addEventListener('message', (e: MessageEvent<FromWorker>) => {
		const m = e.data;
		switch (m.t) {
			case 'frame':
				video.present(m.frame);
				break;
			case 'hello': {
				app.desk = { s: 'shown', w: m.w, h: m.h };
				video.layout();
				files.setup(m.files);
				mic.setup();
				resize.setup(m.resize);
				statusEl.style.display = 'none';
				break;
			}
			case 'status':
				if (m.s === 'closed') {
					app.desk = { s: 'none' };
					mic.stop();
					resize.closed();
					statusEl.style.display = '';
					statusEl.classList.add('ended');
					statusEl.textContent = `Ended: ${m.why}. Click to connect again.`;
				}
				if (m.s === 'open') {
					statusEl.classList.remove('ended');
					statusEl.textContent = 'Connecting…';
					updateVisibility(true);
				}
				break;
			case 'cursor':
				cursor.set(m.buf);
				break;
			case 'clip':
				clipboard.onRemoteText(m.d);
				break;
			case 'clipImage':
				clipboard.onRemoteImage(m.buf);
				break;
			case 'mic':
				if (!m.on) mic.stop();
				break;
			case 'codec':
				codec = m.codec;
				break;
			case 'log':
				console.warn('[voxl]', m.s);
				break;
		}
	});

	// A session that ended is opened again when the viewer asks, never by itself.
	statusEl.addEventListener('click', () => {
		if (statusEl.classList.contains('ended')) post({ t: 'connect' });
	});

	// Autoplay may be allowed (iframe allow="autoplay"); try right away.
	audio.start();
}

main();
