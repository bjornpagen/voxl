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
	files: FilesConfig;
	/** Dynamic resolution: desktop pixels per CSS pixel of the viewer's window. */
	resize?: { scale: number };
}

/** How the relay reaches the server for video and audio (its lanes description):
 * datagrams, streams on one connection, or a connection a stream. */
export type LanesKind = 'datagram' | 'streams' | 'connections';

/** JSON text messages from the relay: src/hub.rs's `ServerMessage` as the server sends
 * them, the lanes the session has, and the session's end with its cause. */
export type ServerMessage =
	| Hello
	| { t: 'clip'; d: string }
	| { t: 'mic'; on: boolean }
	| { t: 'lanes'; kind: LanesKind }
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
	keyframe: 'kf',
} as const;

/** The header of a video message: the relay's type byte, then the 9 bytes src/video.rs's
 * `Header` writes (flags, frame, pipe). Checked against video-header.json, as the
 * server's reader is: `node web/src/video-header.test.ts`. */
export interface VideoHeader {
	key: boolean;
	/** The update sharpens what it codes. */
	sharpens: boolean;
	frame: number;
	/** Server-side pipeline time (capture to send), in 0.1 ms. */
	pipe: number;
}

export function parseVideoHeader(b: Uint8Array): VideoHeader {
	const u32 = (i: number) =>
		((b[i] << 24) | (b[i + 1] << 16) | (b[i + 2] << 8) | b[i + 3]) >>> 0;
	return {
		key: (b[1] & 1) === 1,
		sharpens: (b[1] & 2) === 2,
		frame: u32(2),
		pipe: u32(6),
	};
}
export const VIDEO_HEADER_BYTES = 10;

// ---- page <-> worker ----

/** The counters the worker keeps and the page reads when asked (`window.voxlStats`),
 * shared between them: each a running total, wrapping at 2^32, so the reader takes the
 * difference since its last read. */
export const Count = {
	/** Updates shown. */
	Frames: 0,
	/** Updates arrived. */
	Updates: 1,
	/** Bytes of updates arrived. */
	Bytes: 2,
	/** Arrival to shown of the updates shown, in µs. */
	DecodeUs: 3,
	/** The server's capture to send of the updates arrived, in tenths of a ms. */
	PipeTenths: 4,
	/** Updates that came out of no decoder: not a running rate, the record's own total. */
	Dropped: 5,
} as const;
export const COUNTS = 6;

/** What `window.voxlStats` reads. */
export interface Stats {
	fps: number;
	kbps: number;
	/** Mean arrival to shown, and the server's capture to send, since the last read; null
	 * when none came since. */
	decodeMs: number | null;
	pipeMs: number | null;
	/** What the decoder decodes; null while there is none (before a keyframe). */
	codec: string | null;
	dropped: number;
}

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
