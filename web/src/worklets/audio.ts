// The desktop audio's player. Receives planar Float32 [L, R] chunks on a MessagePort, one
// an Opus packet, as the stream delivers them, and plays them from a ring (ring.ts).
import type { PcmChunk, PlayerOptions } from '../protocol';
import { Ring } from '../ring';
import wire from '../wire.json' with { type: 'json' };

/** One Opus packet's samples (wire.json). */
const FRAME = wire.desktopAudio.frameSamples;

/**
 * The packets the ring holds, beside what the device takes at a time.
 *
 * @limitKind derived, and measured against the player before it
 * @owner the page's desktop audio player
 * @unit Opus packets, each `FRAME` samples (10 ms)
 * @scope the most the ring holds: this many packets and the device's frames
 * @derivation a stream brings a packet late and the next one with it, so two together is
 * the least burst a stream makes, and a ring of one packet drops sound at every hiccup.
 * Each packet more is a packet's time more of lag for a gap saved, and dropping beats lag.
 * @evidence headless Chrome 157 on an M2 Max, whose device takes 256 frames at a time; a
 * tone in 10 ms packets from a fake relay, 9 s a run, 2026-11-18. Packets in pairs every
 * 20 ms, three runs: silent 0.03 to 0.06 % of the time, 3 to 5 ms dropped; the player
 * before (it filled to one packet, and past two packets and a render quantum dropped back
 * to one) was silent 19.4 % and dropped 1757 ms. Packets evenly: no silence and no drop, 18.9 ms held on average,
 * 25.3 ms at most. A 200 ms stall every 2 s: silent 8.8 % (the stalls), 746 ms dropped
 * (the sound that came stale), never more than 25.3 ms held.
 * @revalidate a change of the packet's length; a device that takes more than two packets
 * at a time; the sound on a real link, which this run did not have
 */
const PACKETS = 2;

class PcmPlayer extends AudioWorkletProcessor<PlayerOptions> {
	/** The most waits: the packets, and what the device takes at a time (it renders that
	 * many frames back to back, so less beside the packets would leave each take short). */
	private readonly ring: Ring;

	constructor(options: { readonly processorOptions: PlayerOptions }) {
		super(options);
		this.ring = new Ring(
			PACKETS * FRAME + options.processorOptions.deviceFrames
		);
		this.port.addEventListener(
			'message',
			(e: MessageEvent<{ port: MessagePort }>) => {
				const port = e.data.port;
				port.addEventListener('message', (m: MessageEvent<PcmChunk>) =>
					this.ring.push(m.data.l, m.data.r)
				);
				// Unlike assigning onmessage, addEventListener doesn't start a port.
				port.start();
			}
		);
		this.port.start();
	}

	/** Plays a render quantum, or silence (the output as it came) while less waits. */
	process(_: Float32Array[][], outputs: Float32Array[][]) {
		const out = outputs[0];
		this.ring.play(out[0], out[1]);
		return true;
	}
}
registerProcessor('pcm-player', PcmPlayer);
