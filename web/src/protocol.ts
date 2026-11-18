// Wire protocol between the browser and the relay (src/relay/), and the messages the page
// and its worker exchange. One WebSocket carries everything: binary messages start with a
// type byte, text messages are JSON from the relay and short comma-separated commands from
// the page.
import wire from './wire.json' with { type: 'json' };

/** Type byte of binary messages, as web/src/wire.json has them: the one source the relay
 * and the daemon check theirs against. Relay and server send video, audio, cursors and
 * clipboard images; the page sends clipboard images and the microphone (Opus, or s16le
 * PCM where the browser has no Opus encoder). */
export const Bin = wire.bin;

/** Desktop audio as the daemon encodes it: Opus at this rate and channel count, one
 * packet a frame of this many samples (wire.json). */
export const DESKTOP_AUDIO = wire.desktopAudio;

/** The microphone as the daemon decodes it: mono at this rate, a frame of this many
 * samples (wire.json). */
export const MICROPHONE = wire.microphone;

/** Microseconds of a frame of `samples` at `rate`. */
export const frameMicros = (samples: number, rate: number) =>
	(samples * 1_000_000) / rate;

export interface FilesConfig {
	/** Folder (relative to the shared root) the panel opens and uploads into. */
	dir: string;
	/** The shared root on the server, which the relay hands `voxl files`. */
	root: string;
}

export interface Hello {
	t: 'hello';
	w: number;
	h: number;
	/** The stream's codec as WebCodecs names it (`vp09.PP.LL.DD`: VP9's profile, level and
	 * bit depth, as the daemon configured its encoder). */
	codec: string;
	files: FilesConfig;
	/** Dynamic resolution: desktop pixels per CSS pixel of the viewer's window. */
	resize?: { scale: number };
}

/** JSON text messages from the relay: src/hub.rs's `ServerMessage` as the server sends
 * them, and the session's end with its cause. */
export type ServerMessage =
	| Hello
	| { t: 'clip'; d: string }
	| { t: 'mic'; on: boolean }
	/** The session's lane: datagrams (a keyframe may miss) or a stream (every frame comes). */
	| { t: 'lane'; kind: 'datagram' | 'stream' }
	| { t: 'close'; why: string };

/** What a key did. */
export type KeyAct = 'down' | 'repeat' | 'up';
const KEY_ACT: Record<KeyAct, number> = { down: 1, repeat: 2, up: 0 };

/** Text commands to the server. */
export const cmd = {
	move: (x: number, y: number) => `m,${x},${y}`,
	button: (button: number, down: boolean) => `b,${button},${down ? 1 : 0}`,
	/** Scroll in desktop pixels; the server turns it into smooth scrolling or wheel clicks. */
	scroll: (dx: number, dy: number) => `sp,${dx.toFixed(1)},${dy.toFixed(1)}`,
	/** A key went down, repeated while held (this viewer's own autorepeat) or came up. */
	key: (keysym: number, act: KeyAct) => `k,${keysym},${KEY_ACT[act]}`,
	releaseAll: 'r',
	clipboard: (text: string) => `c,${text}`,
	video: (on: boolean) => `v,${on ? 1 : 0}`,
	audio: (on: boolean) => `au,${on ? 1 : 0}`,
	mic: (on: boolean) => `mi,${on ? 1 : 0}`,
	/** Ask for a desktop of this size (dynamic resolution). */
	resize: (w: number, h: number) => `sz,${w},${h}`,
} as const;

/** The header of a video message: the relay's type byte, then the 13 bytes src/video.rs's
 * `Header` writes (flags, frame, from, pipe), before the VP9 frame. Checked against
 * video-header.json, as the server's reader is: `node web/src/video-header.test.ts`. */
export interface VideoHeader {
	key: boolean;
	frame: number;
	/** The number of the keyframe the update descends from (a keyframe's own). */
	from: number;
	/** Server-side pipeline time (capture to send), in 0.1 ms. */
	pipe: number;
}

/** The flags byte's one bit: the update is a keyframe. */
const KEY = 1;

/** A video message's header, or the flags byte it carries when that sets a bit other than
 * the keyframe's: no such message is the relay's. */
export type ReadHeader =
	| { readonly ok: true; readonly header: VideoHeader }
	| { readonly ok: false; readonly flags: number };

export function parseVideoHeader(b: Uint8Array): ReadHeader {
	const flags = b[1];
	if ((flags & ~KEY) !== 0) return { ok: false, flags };
	const u32 = (i: number) =>
		((b[i] << 24) | (b[i + 1] << 16) | (b[i + 2] << 8) | b[i + 3]) >>> 0;
	return {
		ok: true,
		header: { key: flags === KEY, frame: u32(2), from: u32(6), pipe: u32(10) },
	};
}
export const VIDEO_HEADER_BYTES = 14;

// ---- page <-> worker ----

/** Worker -> page. Relay messages other than close are forwarded as is; the session's
 * end is the status `closed`, with its cause. */
export type FromWorker =
	/** A decoded update, handed to the page to present. */
	| { t: 'frame'; frame: VideoFrame }
	/** The decoder now decodes this codec, or none. */
	| { t: 'codec'; codec: string | null }
	| { t: 'status'; s: 'open' }
	| { t: 'status'; s: 'closed'; why: string }
	| { t: 'cursor'; buf: ArrayBuffer }
	| { t: 'clipImage'; buf: ArrayBuffer }
	| { t: 'log'; s: string }
	| Exclude<ServerMessage, { t: 'close' }>;

/** Page -> worker. */
export type ToWorker =
	/** `counts` is the page's meter's (stats.ts), which the worker adds to. */
	| { t: 'init'; url: string; counts: Uint32Array }
	/** Open a new session after the last one ended (the viewer asked). */
	| { t: 'connect' }
	| { t: 'send'; d: string }
	| { t: 'sendBin'; d: ArrayBuffer }
	| { t: 'video'; on: boolean }
	| { t: 'audioPort'; port: MessagePort };

/** Worker -> desktop audio worklet: one decoded chunk, planar. */
export interface PcmChunk {
	l: Float32Array;
	r: Float32Array;
}
