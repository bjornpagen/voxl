// Microphone: capture -> Opus (WebCodecs) -> server, which feeds the VM's microphone.
import { Bin, cmd, frameMicros, MICROPHONE } from '../protocol';
import type { App } from './app';
import { el } from './app';

const RATE = MICROPHONE.sampleRate;
/** A chunk's length, as the capture worklet cuts them: one of the daemon's frames. */
const CHUNK_MICROS = frameMicros(MICROPHONE.frameSamples, RATE);

/** The microphone's Opus: one channel at the daemon's rate, in its frames, tuned for
 * speech, at 32 kbit/s: inside the 28-40 kbit/s RFC 7587 §3.1.1 recommends for fullband
 * speech. Published. */
const OPUS = {
	codec: 'opus',
	sampleRate: RATE,
	numberOfChannels: 1,
	bitrate: 32_000,
	opus: { frameDuration: CHUNK_MICROS, application: 'voip' },
} as AudioEncoderConfig;

/** The microphone: off, or captured from `stream` through `ctx`. */
type Mic =
	| { s: 'off' }
	| { s: 'on'; stream: MediaStream; ctx: AudioContext; enc: AudioEncoder };

export function createMic(app: App) {
	const btn = el<HTMLButtonElement>('mic-btn');
	let mic: Mic = { s: 'off' };
	let ts = 0;

	function show() {
		const on = mic.s === 'on';
		btn.classList.toggle('on', on);
		btn.setAttribute('aria-pressed', String(on));
		if (!btn.classList.contains('err'))
			btn.title = on ? 'Turn microphone off' : 'Turn microphone on';
		// While the mic is live the toolbar stays up as a recording indicator.
		app.tools.classList.toggle('mic-on', on);
	}

	/** A session's hello: its desktop takes the microphone. */
	function setup() {
		app.tools.classList.add('mic');
	}

	/** Shows why the microphone stopped. */
	function failed(why: string) {
		stop();
		btn.classList.add('err');
		btn.title = why;
		show();
	}

	async function opusEncoder(): Promise<AudioEncoder> {
		if (!(await AudioEncoder.isConfigSupported(OPUS)).supported)
			throw new Error('this browser encodes no Opus for speech');
		const enc = new AudioEncoder({
			output: (chunk) => {
				const b = new Uint8Array(chunk.byteLength);
				chunk.copyTo(b);
				app.sendBin(Bin.micOpus, b);
			},
			error: (err) => failed(`The microphone's encoder failed: ${err.message}`),
		});
		enc.configure(OPUS);
		return enc;
	}

	function onChunk(f: Float32Array) {
		if (mic.s !== 'on') return;
		const data = new AudioData({
			format: 'f32-planar',
			sampleRate: RATE,
			numberOfFrames: f.length,
			numberOfChannels: 1,
			timestamp: ts,
			data: f,
		});
		ts += CHUNK_MICROS;
		mic.enc.encode(data);
		data.close();
	}

	async function start() {
		btn.classList.remove('err');
		let stream: MediaStream;
		try {
			stream = await navigator.mediaDevices.getUserMedia({
				audio: {
					channelCount: 1,
					echoCancellation: true,
					noiseSuppression: true,
					autoGainControl: true,
				},
			});
		} catch (err) {
			return failed(
				err instanceof DOMException && err.name === 'NotAllowedError'
					? 'Microphone access was blocked'
					: 'No microphone available'
			);
		}
		const ctx = new AudioContext({
			sampleRate: RATE,
			latencyHint: 'interactive',
		});
		let enc: AudioEncoder;
		try {
			await ctx.audioWorklet.addModule('mic.js');
			enc = await opusEncoder();
			const node = new AudioWorkletNode(ctx, 'mic-capture', {
				numberOfInputs: 1,
				numberOfOutputs: 0,
				channelCount: 1,
				channelCountMode: 'explicit',
			});
			node.port.addEventListener('message', (e: MessageEvent<Float32Array>) =>
				onChunk(e.data)
			);
			node.port.start();
			ctx.createMediaStreamSource(stream).connect(node);
			if (ctx.state !== 'running') await ctx.resume();
		} catch (err) {
			stream.getTracks().forEach((t) => t.stop());
			ctx.close().catch((e: unknown) => console.warn('[voxl] microphone', e));
			return failed(`The microphone could not start: ${err}`);
		}
		mic = { s: 'on', stream, ctx, enc };
		app.send(cmd.mic(true));
		show();
	}

	function stop() {
		if (mic.s === 'off') return;
		const { stream, ctx, enc } = mic;
		mic = { s: 'off' };
		app.send(cmd.mic(false));
		stream.getTracks().forEach((t) => t.stop());
		ctx.close().catch((e: unknown) => console.warn('[voxl] microphone', e));
		if (enc.state !== 'closed') enc.close();
		show();
	}

	btn.addEventListener('click', () => (mic.s === 'on' ? stop() : start()));
	return { setup, stop };
}
