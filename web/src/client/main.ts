// Main thread: layout, input capture, cursor, clipboard, files, microphone and audio.
// Video decode/paint and the socket live in the worker (worker.ts).
import { cmd } from '../protocol';
import type { FromWorker, ToWorker } from '../protocol';
import { Meter, type Stats } from '../stats';
import type { App } from './app';
import { el, textarea } from './app';
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
 * to present it (its stream is handed to the worker, which writes it); ClipboardItem for clipboard images; Keyboard Lock for the desktop's
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
	const post = (m: ToWorker, transfer: Transferable[]) =>
		worker.postMessage(m, transfer);
	const app: App = {
		worker,
		post,
		send: (d) => {
			post({ t: 'send', d }, []);
		},
		sendBin: (type, bytes) => {
			const msg = new Uint8Array(bytes.byteLength + 1);
			msg[0] = type;
			msg.set(bytes, 1);
			post({ t: 'sendBin', d: msg.buffer }, [msg.buffer]);
		},
		surface: el('surface'),
		kbd: textarea('kbd'),
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

	const meter = new Meter(performance.now());
	post(
		{ t: 'init', url: wsUrl.href, counts: meter.counts, track: video.track },
		[video.track]
	);
	/** What the decoder decodes, as the worker last said. */
	let decodes: { s: 'nothing' } | { s: 'codec'; codec: string } = {
		s: 'nothing',
	};
	// Read by the bench tools (measure.mjs) and handy in the console: the worker's counters
	// since the last read, as rates and means, computed when read.
	Object.defineProperty(window, 'voxlStats', {
		get: (): Stats => ({
			...meter.read(performance.now()),
			codec: decodes.s === 'codec' ? decodes.codec : null,
			decoder: meter.decoder(performance.timeOrigin + performance.now()),
		}),
	});

	// Stop video when nobody can see it. What the worker was last told of it: nothing yet
	// (at the start, and when a session opens), or whether the picture is wanted.
	let told: { s: 'nothing' } | { s: 'video'; on: boolean } = { s: 'nothing' };
	function tellVideo() {
		const on =
			!document.hidden && window.innerWidth > 0 && window.innerHeight > 0;
		if (told.s === 'video' && told.on === on) return;
		told = { s: 'video', on };
		post({ t: 'video', on }, []);
	}
	document.addEventListener('visibilitychange', () => {
		tellVideo();
		if (document.hidden) app.send(cmd.releaseAll);
	});
	window.addEventListener('resize', () => {
		video.layout();
		tellVideo();
	});
	tellVideo();

	/** Acts on the session's status, as `heard` does on every message. */
	function status(m: Extract<FromWorker, { t: 'status' }>): 'heard' {
		switch (m.s) {
			case 'closed':
				app.desk = { s: 'none' };
				mic.stop();
				resize.closed();
				statusEl.style.display = '';
				statusEl.classList.add('ended');
				statusEl.textContent = `Ended: ${m.why}. Click to connect again.`;
				return 'heard';
			case 'open':
				statusEl.classList.remove('ended');
				statusEl.textContent = 'Connecting…';
				// A new session has been told nothing.
				told = { s: 'nothing' };
				tellVideo();
				return 'heard';
		}
	}

	/** Acts on a message of the worker's. What it returns says only that the kind of
	 * message has its case: one without a case does not compile. */
	function heard(m: FromWorker): 'heard' {
		switch (m.t) {
			// The desktop is the size of the pictures it sends: a keyframe of another size
			// (a resize) lays the page out again. The pictures go to the track from the
			// worker, and never come here.
			case 'size':
				app.desk = { s: 'shown', w: m.w, h: m.h };
				video.layout();
				return 'heard';
			case 'hello':
				files.setup(m.files);
				mic.setup();
				resize.setup(m.resize);
				statusEl.style.display = 'none';
				return 'heard';
			case 'status':
				return status(m);
			case 'cursor':
				cursor.set(m.buf);
				return 'heard';
			case 'clip':
				clipboard.onRemoteText(m.d);
				return 'heard';
			case 'clipImage':
				clipboard.onRemoteImage(m.buf);
				return 'heard';
			case 'mic':
				if (!m.on) mic.stop();
				return 'heard';
			case 'decodes':
				decodes = { s: 'codec', codec: m.codec };
				return 'heard';
			case 'decodesNothing':
				decodes = { s: 'nothing' };
				return 'heard';
			case 'log':
				console.warn('[voxl]', m.s);
				return 'heard';
		}
	}
	worker.addEventListener('message', (e: MessageEvent<FromWorker>) =>
		heard(e.data)
	);

	// A session that ended is opened again when the viewer asks, never by itself.
	statusEl.addEventListener('click', () => {
		if (statusEl.classList.contains('ended')) post({ t: 'connect' }, []);
	});

	// Autoplay may be allowed (iframe allow="autoplay"); try right away.
	audio.start();
}

main();
