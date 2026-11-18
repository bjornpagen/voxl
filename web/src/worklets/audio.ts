// PCM player. Receives planar Float32 [L, R] chunks on a MessagePort, already paced by the
// relay, and plays them through a ring of a packet or two.
import type { PcmChunk } from '../protocol';
import wire from '../wire.json' with { type: 'json' };

/** One Opus packet's samples (wire.json). */
const FRAME = wire.desktopAudio.frameSamples;

/** The samples the player renders at a time: Web Audio's render quantum (Web Audio API,
 * "render quantum size", 128). Published. */
const QUANTUM = 128;

/** What the buffer plays from once it primes: one packet. The relay gives packets at their
 * deadlines, as evenly as the daemon sent them, so the path's jitter is taken out before
 * they get here (at the cost of that spread, on every packet); what is left is the decoder
 * putting a packet out as one. Derived. */
const TARGET = FRAME;
/** The most the buffer holds before it drops the oldest: the packet playing, the next one
 * decoded, and a render quantum the player has not yet taken. Derived. */
const MOST = TARGET + FRAME + QUANTUM;
/** The ring's room: the most it holds and one more packet. Derived. */
const ROOM = MOST + FRAME;

class PcmPlayer extends AudioWorkletProcessor {
	private l = new Float32Array(ROOM);
	private r = new Float32Array(ROOM);
	private rd = 0;
	private wr = 0;
	private fill = 0;
	private priming = true;

	constructor() {
		super();
		this.port.addEventListener(
			'message',
			(e: MessageEvent<{ port: MessagePort }>) => {
				const port = e.data.port;
				port.addEventListener('message', (m: MessageEvent<PcmChunk>) =>
					this.push(m.data)
				);
				// Unlike assigning onmessage, addEventListener doesn't start a port.
				port.start();
			}
		);
		this.port.start();
	}

	private push({ l, r }: PcmChunk) {
		for (let i = 0; i < l.length; i++) {
			this.l[this.wr] = l[i];
			this.r[this.wr] = r[i];
			this.wr = (this.wr + 1) % ROOM;
		}
		this.fill += l.length;
		// Too much queued: drop the oldest so latency stays bounded.
		if (this.fill > MOST) {
			const drop = this.fill - TARGET;
			this.rd = (this.rd + drop) % ROOM;
			this.fill -= drop;
		}
	}

	process(_: Float32Array[][], outputs: Float32Array[][]) {
		const out = outputs[0];
		const n = out[0].length;
		if (this.priming) {
			if (this.fill < TARGET) return true;
			this.priming = false;
		}
		if (this.fill < n) {
			this.priming = true;
			return true;
		}
		for (let i = 0; i < n; i++) {
			out[0][i] = this.l[this.rd];
			out[1][i] = this.r[this.rd];
			this.rd = (this.rd + 1) % ROOM;
		}
		this.fill -= n;
		return true;
	}
}
registerProcessor('pcm-player', PcmPlayer);
