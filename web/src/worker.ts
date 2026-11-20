// The worker: the socket to the relay, the AV1 decoder and the Opus decoder, off the page's
// thread. What it decides is in two records: its session with the relay (session.ts), which
// says when a frame was taken, the screen (frames.ts), which holds one update at a time, and
// the presenter (track.ts), which writes each picture to the page's video track. Here they
// are joined to the browser: a WebSocket, WebCodecs, the track, the page and the counters.
import { Frames } from './frames';
import { DESKTOP_AUDIO, frameMicros, VIDEO_HEADER_BYTES } from './protocol';
import { Session, type Told } from './session';
import { Count, holds, Timed, took } from './stats';
import { Track } from './track';
import type { FromWorker, PcmChunk, ToWorker } from './protocol';

declare const self: DedicatedWorkerGlobalScope;

type Init = Extract<ToWorker, { t: 'init' }>;

/** Where the desktop's sound goes: the player's port, the decoder (made at the first
 * packet, and again after one fails), and the next packet's timestamp, in µs. */
interface Player {
	readonly port: MessagePort;
	decoder: { s: 'none' } | { s: 'made'; dec: AudioDecoder };
	at: number;
}

const post = (m: FromWorker, transfer: Transferable[]) =>
	self.postMessage(m, transfer);

/** A time in ms as a decoder's timestamp and the counters keep it: in µs. */
const micros = (ms: number) => Math.round(ms * 1000);

/** Now, in ms on the clock the worker and the page share: each thread's `performance`
 * counts from its own origin, and the origin itself is on one clock for both. */
const now = () => performance.timeOrigin + performance.now();

/** The first message is `init`; everything else is answered by what it starts. */
self.addEventListener('message', function first(e: MessageEvent<ToWorker>) {
	if (e.data.t !== 'init')
		throw new Error(`voxl worker: ${e.data.t} before init`);
	self.removeEventListener('message', first);
	start(e.data);
});

/** What goes to the page with a message, handed over and not copied: an image's buffer. */
function handed(m: Told): Transferable[] {
	switch (m.t) {
		case 'cursor':
		case 'clipImage':
			return [m.buf];
		case 'status':
		case 'log':
		case 'hello':
		case 'clip':
		case 'mic':
			return [];
	}
}

/** A decoder for `player`, whose PCM goes to the player's port. One that fails is closed,
 * and the next packet makes another. */
function audioDecoder(player: Player): AudioDecoder {
	const dec = new AudioDecoder({
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
			player.port.postMessage(chunk, [chunk.l.buffer, chunk.r.buffer]);
		},
		error: (e) => {
			if (player.decoder.s === 'made' && player.decoder.dec === dec)
				player.decoder = { s: 'none' };
			post({ t: 'log', s: `audio: ${e.message}` }, []);
		},
	});
	dec.configure({
		codec: 'opus',
		sampleRate: DESKTOP_AUDIO.sampleRate,
		numberOfChannels: DESKTOP_AUDIO.channels,
	});
	return dec;
}

function start(init: Init) {
	const counts = init.counts;

	// The page's video track, written here: a write that fails is said, and ends as any.
	const writer = init.track.getWriter();
	const track = new Track<VideoFrame>({
		write(frame) {
			writer.write(frame).then(
				() => track.written(),
				(e: unknown) => {
					post({ t: 'log', s: `video track: ${e}` }, []);
					track.written();
				}
			);
		},
		letGo: (frame) => frame.close(),
		size: (w, h) => post({ t: 'size', w, h }, []),
	});

	const session = new Session<WebSocket, Player>({
		dial() {
			const ws = new WebSocket(init.url);
			ws.binaryType = 'arraybuffer';
			ws.addEventListener('open', () => session.opened());
			ws.addEventListener('close', () => session.closed());
			ws.addEventListener('message', (e: MessageEvent<string | ArrayBuffer>) =>
				session.message(now(), e.data)
			);
			return ws;
		},
		send: (ws, d) => ws.send(d),
		hangUp: (ws) => ws.close(),
		tell: (m) => post(m, handed(m)),
		hello: (codec) => frames.hello(codec),
		update(at, header, picture) {
			Atomics.add(counts, Count.Bytes, VIDEO_HEADER_BYTES + picture.length);
			took(counts, Timed.Pipe, header.pipe);
			frames.part(at, header, picture);
		},
		screenClosed() {
			frames.closed();
			track.closed();
		},
		sound(player, packet) {
			const dec =
				player.decoder.s === 'made' ? player.decoder.dec : audioDecoder(player);
			player.decoder = { s: 'made', dec };
			player.at += frameMicros(
				DESKTOP_AUDIO.frameSamples,
				DESKTOP_AUDIO.sampleRate
			);
			dec.decode(
				new EncodedAudioChunk({
					type: 'key',
					timestamp: player.at,
					data: packet,
				})
			);
		},
	});

	// The screen arrives as one AV1 stream; each update's picture goes to the track as it is
	// decoded.
	const frames = new Frames<VideoDecoder, VideoFrame, Uint8Array>({
		// Asked once, at the session's hello: a codec this browser cannot decode ends it.
		check(begun) {
			const { codec } = begun;
			void (async () => {
				let r: VideoDecoderSupport;
				try {
					r = await VideoDecoder.isConfigSupported({
						codec,
						optimizeForLatency: true,
					});
				} catch (e) {
					return session.screenEnded(
						`the codec ${codec} could not be checked: ${e}`
					);
				}
				if (!r.supported) frames.unsupported(begun);
			})();
		},
		decodes: (codec) => post({ t: 'decodes', codec }, []),
		decodesNothing: () => post({ t: 'decodesNothing' }, []),
		// The page takes an update when its picture comes out, so it relies on the decoder
		// putting out one picture for each update, before any further input (frames.ts).
		// The daemon's part holds: each update is one shown frame, with nothing reordered.
		// The decoder's is asked for (`optimizeForLatency`) but is not WebCodecs' promise:
		// the option is a "hint that the selected decoder SHOULD be configured to minimize
		// the number of EncodedVideoChunks that have to be decoded before a VideoFrame is
		// output", and "the underlying codec implementation MAY emit new outputs only when
		// new inputs are provided" (W3C WebCodecs, VideoDecoderConfig and "Internal Pending
		// Output"). Measured on 2026-11-19 in Chrome 157 on an M2 Max, configured as here:
		// 60 of 60 pictures out before the next input, none held, for AV1 in software
		// (Chromium's dav1d wrapper sets max_frame_delay 1 in low-delay mode).
		// A decoder that kept a picture in would leave the session waiting, and
		// `voxlStats.decoder` would read decoding since that update's arrival: flush() is no
		// way out, since it makes the next update have to be a keyframe.
		open(codec) {
			const dec = new VideoDecoder({
				output: (frame) => frames.output(dec, frame),
				error: (e) => frames.error(dec, e.name, e.message),
			});
			dec.configure({
				codec,
				optimizeForLatency: true,
				hardwareAcceleration: 'no-preference',
			});
			return dec;
		},
		// The picture's timestamp is when its update arrived: `present` reads it there.
		// How an update is coded is WebCodecs' word for it too.
		decode(dec, coding, at, data) {
			dec.decode(
				new EncodedVideoChunk({
					type: coding,
					timestamp: micros(at),
					data,
				})
			);
		},
		decoder: (state) => holds(counts, state),
		close: (dec) => dec.close(),
		end: (why) => session.screenEnded(why),
		letGo: (frame) => frame.close(),
		present(frame) {
			Atomics.add(counts, Count.Frames, 1);
			took(counts, Timed.Decode, micros(now()) - frame.timestamp);
			track.present(frame, frame.displayWidth, frame.displayHeight);
			session.decoded();
		},
	});

	/** Acts on a message of the page's. What it returns says only that the kind of message
	 * has its case: one without a case does not compile. */
	function heard(m: ToWorker): 'heard' {
		switch (m.t) {
			case 'init':
				throw new Error('voxl worker: a second init');
			case 'connect':
				session.connect();
				return 'heard';
			case 'send':
			case 'sendBin':
				session.say(m.d);
				return 'heard';
			case 'video':
				session.video(m.on);
				return 'heard';
			case 'audioPort':
				session.sound({ port: m.port, decoder: { s: 'none' }, at: 0 });
				return 'heard';
		}
	}
	self.addEventListener('message', (e: MessageEvent<ToWorker>) =>
		heard(e.data)
	);

	session.connect();
}
