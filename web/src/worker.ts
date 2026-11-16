// Owns the WebSocket, decodes H.264 with WebCodecs and paints straight onto an
// OffscreenCanvas the moment each frame is decoded (no rAF queueing), or hands the
// frames to the page when it presents them through <video>.
import { Frames } from './frames';
import { Bin, cmd, parseVideoHeader, VIDEO_HEADER_BYTES } from './protocol';
import type {
	FromWorker,
	PcmChunk,
	ServerMessage,
	Stats,
	Stripe,
	StripeFrame,
	ToWorker,
} from './protocol';

declare const self: DedicatedWorkerGlobalScope;

const noDraw = /nodraw/.test(self.location.search);
let presentToMain = false;
let ws: WebSocket | null = null;
let url = '';
let canvas: OffscreenCanvas | null = null;
let ctx: OffscreenCanvasRenderingContext2D | null = null;
let audioDecoder: AudioDecoder | null = null;
let audioPort: MessagePort | null = null;
let audioTs = 0;
let wantVideo = true;
let wantAudio = false;

let layout: Stripe[] = [];
const st = {
	frames: 0,
	bytes: 0,
	decodeMs: 0,
	pipeMs: 0,
	lastReport: performance.now(),
};

// Firefox throws if getContext('2d') is called twice on one canvas; reuse it.
// Fall back to default attrs if the ones below are rejected.
function get2dContext(
	c: OffscreenCanvas
): OffscreenCanvasRenderingContext2D | null {
	if (ctx) return ctx;
	try {
		return c.getContext('2d', { alpha: false, desynchronized: true });
	} catch {
		try {
			return c.getContext('2d');
		} catch {
			return null;
		}
	}
}

function send(s: string) {
	if (ws && ws.readyState === WebSocket.OPEN) ws.send(s);
}
function post(m: FromWorker, transfer: Transferable[] = []) {
	self.postMessage(m, transfer);
}

function connect() {
	const sock = new WebSocket(url);
	ws = sock;
	sock.binaryType = 'arraybuffer';
	sock.addEventListener('open', () => {
		post({ t: 'status', s: 'open' });
		send(cmd.video(wantVideo));
		send(cmd.audio(wantAudio));
	});
	sock.addEventListener('close', () => {
		post({ t: 'status', s: 'closed' });
		frames.closed();
		setTimeout(connect, 1000);
	});
	sock.addEventListener('message', (e: MessageEvent<string | ArrayBuffer>) => {
		if (typeof e.data === 'string') return onText(e.data);
		const b = new Uint8Array(e.data);
		switch (b[0]) {
			case Bin.Video:
				onVideo(sock, b);
				break;
			case Bin.Audio:
				onAudio(b);
				break;
			case Bin.Cursor:
				post({ t: 'cursor', buf: e.data }, [e.data]);
				break;
			case Bin.ClipImage:
				post({ t: 'clipImage', buf: e.data }, [e.data]);
				break;
		}
	});
}

function onText(s: string) {
	let m: ServerMessage;
	try {
		m = JSON.parse(s);
	} catch {
		return;
	}
	if (m.t === 'ping') {
		send(cmd.pong(m.s));
		return;
	}
	if (m.t === 'close') {
		try {
			ws?.close();
		} catch {}
		return;
	}
	if (m.t === 'hello' && canvas) {
		if (canvas.width !== m.w || canvas.height !== m.h) {
			canvas.width = m.w;
			canvas.height = m.h;
		}
		ctx = get2dContext(canvas);
		layout = m.stripes;
		frames.hello(layout.length);
	}
	post(m);
}

const hex2 = (x: number) => x.toString(16).padStart(2, '0');

// The screen arrives as independent H.264 stripes, one decoder each. A frame is a set of
// stripe updates sharing an id; frames are presented whole and in order.
const frames = new Frames<VideoDecoder, StripeFrame, Uint8Array>({
	/** avc1.PPCCLL from the first SPS in an Annex-B keyframe. */
	codec(key) {
		for (let i = 0; i + 4 < key.length; i++) {
			if (
				key[i] === 0 &&
				key[i + 1] === 0 &&
				key[i + 2] === 1 &&
				(key[i + 3] & 0x1f) === 7
			) {
				return 'avc1.' + hex2(key[i + 4]) + hex2(key[i + 5]) + hex2(key[i + 6]);
			}
		}
		throw new TypeError('a keyframe without an SPS');
	},
	open(i, codec) {
		// A decoder is opened after the hello that cut the screen it decodes a stripe of.
		const y = layout[i][0];
		const dec = new VideoDecoder({
			output: (frame) => frames.output(i, frame.timestamp, { i, y, frame }),
			error: (e) => frames.error(dec, e.name, e.message),
		});
		dec.configure({
			codec,
			optimizeForLatency: true,
			hardwareAcceleration: 'no-preference',
		});
		return dec;
	},
	decode(dec, key, frame, data) {
		dec.decode(
			new EncodedVideoChunk({
				type: key ? 'key' : 'delta',
				timestamp: frame,
				data,
			})
		);
	},
	close(dec) {
		dec.close();
	},
	ask() {
		send(cmd.keyframe);
	},
	log(i, why) {
		post({ t: 'log', s: `stripe ${i}: ${why}` });
	},
	letGo(picture) {
		picture.frame.close();
	},
	show(f) {
		const parts = f.pictures;
		if (parts.length) {
			if (presentToMain)
				post(
					{ t: 'frames', parts },
					parts.map((g) => g.frame)
				);
			else
				for (const g of parts) {
					if (ctx && !noDraw) ctx.drawImage(g.frame, 0, g.y);
					g.frame.close();
				}
		}
		const now = performance.now();
		send(cmd.ack(f.id, now - f.last));
		st.frames++;
		st.decodeMs += now - f.first;
	},
});

function onVideo(sock: WebSocket, b: Uint8Array) {
	const h = parseVideoHeader(b);
	st.bytes += b.length;
	const part = frames.admit(h);
	if (!part) {
		// Its frame could never be acknowledged, and the server would hold the picture for
		// it for good: the connection starts over, from a hello.
		post({ t: 'log', s: `a part of stripe ${h.stripe}, outside the cut` });
		sock.close();
		return;
	}
	if (frames.part(performance.now(), part, b.subarray(VIDEO_HEADER_BYTES)))
		st.pipeMs += h.pipe / 10;
}

function onAudio(b: Uint8Array) {
	const port = audioPort;
	if (!port || typeof AudioDecoder === 'undefined') return;
	if (!audioDecoder) {
		audioDecoder = new AudioDecoder({
			output: (ad) => {
				const n = ad.numberOfFrames;
				const chunk: PcmChunk = {
					l: new Float32Array(n),
					r: new Float32Array(n),
				};
				ad.copyTo(chunk.l, { planeIndex: 0, format: 'f32-planar' });
				if (ad.numberOfChannels > 1)
					ad.copyTo(chunk.r, { planeIndex: 1, format: 'f32-planar' });
				else chunk.r.set(chunk.l);
				ad.close();
				port.postMessage(chunk, [chunk.l.buffer, chunk.r.buffer]);
			},
			error: () => {
				audioDecoder = null;
			},
		});
		audioDecoder.configure({
			codec: 'opus',
			sampleRate: 48000,
			numberOfChannels: 2,
		});
	}
	audioTs += 10000;
	try {
		audioDecoder.decode(
			new EncodedAudioChunk({
				type: 'key',
				timestamp: audioTs,
				data: b.subarray(1),
			})
		);
	} catch {}
}

setInterval(() => {
	const now = performance.now();
	const dt = (now - st.lastReport) / 1000;
	const n = st.frames;
	const stats: Stats = {
		t: 'stats',
		fps: n / dt,
		kbps: (st.bytes * 8) / 1000 / dt,
		decodeMs: n ? st.decodeMs / n : 0,
		pipeMs: n ? st.pipeMs / n : 0,
		codec: frames.codec,
		stripes: layout.length,
		dropped: frames.dropped,
		queue: frames.waiting,
	};
	post(stats);
	st.frames = 0;
	st.bytes = 0;
	st.decodeMs = 0;
	st.pipeMs = 0;
	st.lastReport = now;
}, 1000);

self.addEventListener('message', (e: MessageEvent<ToWorker>) => {
	const m = e.data;
	switch (m.t) {
		case 'init':
			canvas = m.canvas;
			url = m.url;
			presentToMain = m.presentToMain;
			ctx = get2dContext(canvas);
			connect();
			break;
		case 'send':
			send(m.d);
			break;
		case 'sendBin':
			if (ws && ws.readyState === WebSocket.OPEN) ws.send(m.d);
			break;
		case 'video':
			if (wantVideo !== m.on) {
				wantVideo = m.on;
				send(cmd.video(m.on));
			}
			break;
		case 'audioPort':
			audioPort = m.port;
			wantAudio = true;
			send(cmd.audio(true));
			break;
	}
});
