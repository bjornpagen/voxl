// Microphone capture: posts mono Float32 chunks of one of the daemon's frames
// (wire.json's microphone.frameSamples).
import wire from '../wire.json' with { type: 'json' };

const FRAME = wire.microphone.frameSamples;

class MicCapture extends AudioWorkletProcessor {
	private buf = new Float32Array(FRAME);
	private n = 0;

	process(inputs: Float32Array[][]) {
		const ch = inputs[0][0];
		// An input with no channels: nothing connected yet.
		if (ch === undefined) return true;
		for (let i = 0; i < ch.length; i++) {
			this.buf[this.n++] = ch[i];
			if (this.n === FRAME) {
				this.port.postMessage(this.buf, [this.buf.buffer]);
				this.buf = new Float32Array(FRAME);
				this.n = 0;
			}
		}
		return true;
	}
}
registerProcessor('mic-capture', MicCapture);
