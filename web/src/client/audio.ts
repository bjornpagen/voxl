// Desktop audio: the worker decodes Opus and posts PCM straight to the player worklet
// over a MessageChannel, so audio never waits on the main thread.
import { DESKTOP_AUDIO } from '../protocol';
import type { App } from './app';

/** Desktop audio's player: not started, starting, playing through `ac`, or unavailable
 * (the player could not start). */
type Player =
	| { s: 'idle' }
	| { s: 'starting' }
	| { s: 'playing'; ac: AudioContext }
	| { s: 'unavailable' };

const resume = (ac: AudioContext) =>
	ac.resume().catch((e: unknown) => console.warn('[voxl] audio', e));

export function createAudio(app: App) {
	let player: Player = { s: 'idle' };

	/** Starts (or resumes) playback; browsers only allow it after a user gesture. */
	async function start() {
		switch (player.s) {
			case 'playing':
				if (player.ac.state === 'suspended') await resume(player.ac);
				return;
			case 'starting':
			case 'unavailable':
				return;
			case 'idle':
		}
		player = { s: 'starting' };
		const ac = new AudioContext({
			latencyHint: 'interactive',
			sampleRate: DESKTOP_AUDIO.sampleRate,
		});
		try {
			await ac.audioWorklet.addModule('audio.js');
		} catch (err) {
			console.warn('[voxl] audio unavailable', err);
			player = { s: 'unavailable' };
			return;
		}
		const node = new AudioWorkletNode(ac, 'pcm-player', {
			outputChannelCount: [2],
		});
		node.connect(ac.destination);
		const ch = new MessageChannel();
		node.port.postMessage({ port: ch.port2 }, [ch.port2]);
		app.post({ t: 'audioPort', port: ch.port1 }, [ch.port1]);
		player = { s: 'playing', ac };
		if (ac.state !== 'running') await resume(ac);
	}
	return { start };
}
