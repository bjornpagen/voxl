// Owns the WebSocket to the relay, decodes VP9 with WebCodecs and hands each update to the
// page the moment it is decoded, which presents it through <video>. A session that ends stays
// ended until the page asks for another: nothing reconnects by itself.
import { Frames, type LaneKind } from './frames';
import {
	Bin,
	cmd,
	DESKTOP_AUDIO,
	frameMicros,
	parseVideoHeader,
	VIDEO_HEADER_BYTES,
} from './protocol';
import { Count } from './protocol';
import type { FromWorker, PcmChunk, ServerMessage, ToWorker } from './protocol';

declare const self: DedicatedWorkerGlobalScope;

type Init = Extract<ToWorker, { t: 'init' }>;

/** The session: none yet (or the last one closed), or a socket to the relay. */
type Session = { s: 'none' } | { s: 'live'; ws: WebSocket; ending: Ending };

/** Why a live session will end: the relay's own words, once it said them before closing
 * the socket; until then the socket's closing is its own cause. */
type Ending = { s: 'unsaid' } | { s: 'said'; why: string };

/** Desktop audio: not asked for yet, or played through `port`, with a decoder made at the
 * first packet and again after one fails. */
type Audio =
	| { s: 'off' }
	| { s: 'on'; port: MessagePort; decoder: AudioDecoder | 'none' };

const post = (m: FromWorker, transfer: Transferable[] = []) =>
	self.postMessage(m, transfer);

/** The decoder's timestamp of an update's picture: when the update arrived, in µs. */
const micros = (ms: number) => Math.round(ms * 1000);

/** The first message is `init`; everything else is answered by what it starts. */
self.addEventListener('message', function first(e: MessageEvent<ToWorker>) {
	if (e.data.t !== 'init')
		throw new Error(`voxl worker: ${e.data.t} before init`);
	self.removeEventListener('message', first);
	start(e.data);
});

function start(init: Init) {
	let session: Session = { s: 'none' };
	/** The session's lane kind, as the relay said it before the stream's hello; none until
	 * it says. */
	let lane: LaneKind | null = null;
	let audio: Audio = { s: 'off' };
	let audioTs = 0;
	let wantVideo = true;
	const counts = init.counts;
	/** What the decoder decodes, as the page was last told. */
	let told: string | null = null;
	/** After the record is told something: the page's codec follows it. */
	function recorded() {
		if (frames.codec !== told) {
			told = frames.codec;
			post({ t: 'codec', codec: told });
		}
	}

	/** A message to the relay, if a session is open; with none, there is no one to tell. */
	function send(d: string | ArrayBuffer) {
		if (session.s === 'live' && session.ws.readyState === WebSocket.OPEN)
			session.ws.send(d);
	}

	/** Ends the live session for `why`, as if the relay had said it. */
	function end(why: string) {
		if (session.s !== 'live') return;
		session.ending = { s: 'said', why };
		session.ws.close();
	}

	function connect() {
		if (session.s === 'live') return;
		lane = null;
		const ws = new WebSocket(init.url);
		const live: Session = { s: 'live', ws, ending: { s: 'unsaid' } };
		session = live;
		ws.binaryType = 'arraybuffer';
		ws.addEventListener('open', () => {
			post({ t: 'status', s: 'open' });
			send(cmd.video(wantVideo));
			send(cmd.audio(audio.s === 'on'));
		});
		ws.addEventListener('close', () => {
			const why =
				live.ending.s === 'said'
					? live.ending.why
					: 'the connection to the relay closed';
			session = { s: 'none' };
			post({ t: 'status', s: 'closed', why });
			frames.closed();
			recorded();
		});
		ws.addEventListener('message', (e: MessageEvent<string | ArrayBuffer>) => {
			if (typeof e.data === 'string') return onText(e.data);
			const b = new Uint8Array(e.data);
			switch (b[0]) {
				case Bin.video:
					return onVideo(b);
				case Bin.audio:
					return onAudio(b);
				case Bin.cursor:
					return post({ t: 'cursor', buf: e.data }, [e.data]);
				case Bin.clipImage:
					return post({ t: 'clipImage', buf: e.data }, [e.data]);
				default:
					return end(`the relay sent a message of type ${b[0]}`);
			}
		});
	}

	function onText(s: string) {
		let m: ServerMessage;
		try {
			m = JSON.parse(s);
		} catch {
			return end('the relay sent text that is not JSON');
		}
		if (m.t === 'close') return end(m.why);
		if (m.t === 'lane') {
			lane = m.kind;
			return;
		}
		if (m.t === 'hello') {
			if (lane === null) return end('the relay sent a hello before its lane');
			const codec = m.codec;
			const stream = frames.hello(lane, codec);
			recorded();
			// Checked once a stream: a codec this browser cannot decode ends the session.
			void (async () => {
				let r: VideoDecoderSupport;
				try {
					r = await VideoDecoder.isConfigSupported({
						codec,
						optimizeForLatency: true,
					});
				} catch (e) {
					return end(`the codec ${codec} could not be checked: ${e}`);
				}
				if (!r.supported) frames.unsupported(stream);
			})();
		}
		post(m);
	}

	// The screen arrives as one VP9 stream; each update is shown as it is decoded.
	const frames = new Frames<VideoDecoder, VideoFrame, Uint8Array>({
		open(codec) {
			const dec = new VideoDecoder({
				output: (frame) => {
					frames.output(dec, frame);
					recorded();
				},
				error: (e) => {
					frames.error(dec, e.name, e.message);
					recorded();
				},
			});
			dec.configure({
				codec,
				optimizeForLatency: true,
				hardwareAcceleration: 'no-preference',
			});
			return dec;
		},
		decode(dec, key, at, data) {
			dec.decode(
				new EncodedVideoChunk({
					type: key ? 'key' : 'delta',
					timestamp: micros(at),
					data,
				})
			);
		},
		close(dec) {
			dec.close();
		},
		passed() {
			Atomics.add(counts, Count.Passed, 1);
		},
		end(why) {
			post({ t: 'log', s: `video: ${why}` });
			end(`the screen cannot be shown: ${why}`);
		},
		letGo(frame) {
			frame.close();
		},
		show(frame) {
			Atomics.add(counts, Count.Frames, 1);
			Atomics.add(
				counts,
				Count.DecodeUs,
				micros(performance.now()) - frame.timestamp
			);
			post({ t: 'frame', frame }, [frame]);
		},
	});

	function onVideo(b: Uint8Array) {
		const read = parseVideoHeader(b);
		if (!read.ok)
			return end(`the relay sent a video header with flags ${read.flags}`);
		const h = read.header;
		Atomics.add(counts, Count.Bytes, b.length);
		Atomics.add(counts, Count.Updates, 1);
		Atomics.add(counts, Count.PipeTenths, h.pipe);
		frames.part(performance.now(), h.key, b.subarray(VIDEO_HEADER_BYTES));
		recorded();
	}

	function onAudio(b: Uint8Array) {
		if (audio.s !== 'on') return;
		const on = audio;
		if (on.decoder === 'none') {
			const port = on.port;
			const decoder = new AudioDecoder({
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
				// A decoder that failed is closed; the next packet makes another.
				error: (e) => {
					if (on.decoder === decoder) on.decoder = 'none';
					post({ t: 'log', s: `audio: ${e.message}` });
				},
			});
			decoder.configure({
				codec: 'opus',
				sampleRate: DESKTOP_AUDIO.sampleRate,
				numberOfChannels: DESKTOP_AUDIO.channels,
			});
			on.decoder = decoder;
		}
		audioTs += frameMicros(
			DESKTOP_AUDIO.frameSamples,
			DESKTOP_AUDIO.sampleRate
		);
		on.decoder.decode(
			new EncodedAudioChunk({
				type: 'key',
				timestamp: audioTs,
				data: b.subarray(1),
			})
		);
	}

	self.addEventListener('message', (e: MessageEvent<ToWorker>) => {
		const m = e.data;
		switch (m.t) {
			case 'init':
				throw new Error('voxl worker: a second init');
			case 'connect':
				return connect();
			case 'send':
			case 'sendBin':
				return send(m.d);
			case 'video':
				if (wantVideo !== m.on) {
					wantVideo = m.on;
					send(cmd.video(m.on));
				}
				return;
			case 'audioPort':
				audio = { s: 'on', port: m.port, decoder: 'none' };
				send(cmd.audio(true));
				return;
		}
	});

	connect();
}
