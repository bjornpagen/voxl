// Wire protocol between the browser and the relay (src/relay/), and the messages the page
// and its worker exchange. One WebSocket carries everything: binary messages start with a
// type byte, text messages are JSON from the relay and short comma-separated commands from
// the page.
import wire from './wire.json' with { type: 'json' };

/** Type byte of binary messages, as web/src/wire.json has them: the one source the relay
 * and the daemon check theirs against. Relay and server send video, audio, cursors and
 * clipboard images; the page sends clipboard images and the microphone (Opus, or s16le
 * PCM where the browser has no Opus encoder), and `took`, a message of that one byte, to
 * the relay alone: it has finished with the video update or the Opus packet it was given
 * last, and the relay gives it the next only then. */
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

/** The session's hello: its codec and settings. The desktop's size is the keyframe's: the
 * page lays out from the pictures it decodes. */
export interface Hello {
	t: 'hello';
	/** The stream's codec as WebCodecs names it (`av01.P.LLT.DD`: AV1's profile, level, tier
	 * and bit depth, as the daemon configured its encoder). */
	codec: string;
	files: FilesConfig;
	resize: Resize;
}

/** Dynamic resolution, the one way a desktop is sized, as every hello says it: the server
 * makes the desktop the viewer's window's size, at `scale` desktop pixels per CSS pixel. */
export interface Resize {
	readonly scale: number;
}

/** JSON text messages from the relay: src/hub.rs's `ServerMessage` as the server sends
 * them, and the session's end with its cause. */
export type ServerMessage =
	| Hello
	| { t: 'clip'; d: string }
	| { t: 'mic'; on: boolean }
	| { t: 'close'; why: string };

/** What a key did. */
export type KeyAct = 'down' | 'repeat' | 'up';
const KEY_ACT: Record<KeyAct, number> = { down: 1, repeat: 2, up: 0 };

/** The words the text commands begin with, by the command's name: wire.json, their one
 * owner, which the daemon's own roster is held to. */
const WORD = wire.command;

/** Text commands to the server: one for each word there is, each written as its word and
 * what it carries, comma-separated. */
export const cmd = {
	move: (x: number, y: number) => `${WORD.move},${x},${y}`,
	button: (button: number, down: boolean) =>
		`${WORD.button},${button},${down ? 1 : 0}`,
	/** Scroll in desktop pixels; the server turns it into smooth scrolling or wheel clicks. */
	scrollPx: (dx: number, dy: number) =>
		`${WORD.scrollPx},${dx.toFixed(1)},${dy.toFixed(1)}`,
	/** A key went down, repeated while held (this viewer's own autorepeat) or came up. */
	key: (keysym: number, act: KeyAct) => `${WORD.key},${keysym},${KEY_ACT[act]}`,
	releaseAll: WORD.releaseAll,
	clipboard: (text: string) => `${WORD.clipboard},${text}`,
	video: (on: boolean) => `${WORD.video},${on ? 1 : 0}`,
	audio: (on: boolean) => `${WORD.audio},${on ? 1 : 0}`,
	mic: (on: boolean) => `${WORD.mic},${on ? 1 : 0}`,
	/** Ask for a desktop of this size (dynamic resolution). */
	resize: (w: number, h: number) => `${WORD.resize},${w},${h}`,
} satisfies Readonly<Record<keyof typeof WORD, unknown>>;

/** How an update is coded, by its flags byte: from the frames before it (0), or from
 * nothing (1), which a viewer starts on. src/wire.rs's `Coding` and video-header.json spell
 * the two the same; a flags byte that is neither is no header's. */
export const CODINGS = ['delta', 'key'] as const;
export type Coding = (typeof CODINGS)[number];

/** The header of a video message: its type byte, then the 9 bytes of the header src/wire.rs
 * defines (`Header`: flags, version, pipe), before the AV1 temporal unit. Checked against
 * video-header.json, as the server's reader and writer are: `node
 * web/src/video-header.test.ts`. */
export interface VideoHeader {
	coding: Coding;
	/** The capture the picture was coded from, as the daemon numbers them. The page reads
	 * it and makes nothing of it: what it took is told by the lane message's own name,
	 * which the relay keeps and the page never sees. */
	version: number;
	/** Server-side pipeline time (capture to send), in 0.1 ms. */
	pipe: number;
}

/** A video message's header, or why the message is no update: it is shorter than a header,
 * its flags are no coding's, or no picture follows the header (the daemon sends no update
 * that coded nothing). The server's reader refuses the same, in the same order
 * (src/wire.rs's `Header::read`), and no such message is the relay's. */
export type ReadHeader =
	| { readonly ok: true; readonly header: VideoHeader }
	| { readonly ok: false; readonly why: string };

export const VIDEO_HEADER_BYTES = 10;

export function parseVideoHeader(b: Uint8Array): ReadHeader {
	if (b.length < VIDEO_HEADER_BYTES)
		return { ok: false, why: `a video message of ${b.length} bytes` };
	const coding = CODINGS.at(b[1]);
	if (coding === undefined)
		return { ok: false, why: `a video header with flags ${b[1]}` };
	if (b.length === VIDEO_HEADER_BYTES)
		return { ok: false, why: 'a video update without a picture' };
	const u32 = (i: number) =>
		((b[i] << 24) | (b[i + 1] << 16) | (b[i + 2] << 8) | b[i + 3]) >>> 0;
	return { ok: true, header: { coding, version: u32(2), pipe: u32(6) } };
}

/** A message from the relay, read once, where it arrives. */
export type FromRelay =
	/** A text message: the daemon's hello, clipboard or microphone word, or the relay's
	 * word of why the session ends. */
	| { readonly k: 'said'; readonly m: ServerMessage }
	/** A video update: its header, and its AV1 temporal unit. */
	| {
			readonly k: 'video';
			readonly header: VideoHeader;
			readonly picture: Uint8Array;
	  }
	/** An Opus packet. */
	| { readonly k: 'audio'; readonly packet: Uint8Array }
	/** A cursor's image, or the desktop's clipboard image: the message whole, as the page
	 * reads it. */
	| { readonly k: 'cursor'; readonly buf: ArrayBuffer }
	| { readonly k: 'clipImage'; readonly buf: ArrayBuffer }
	/** No message of the relay's, and why not. */
	| { readonly k: 'bad'; readonly why: string };

const bad = (why: string): FromRelay => ({ k: 'bad', why });
const said = (m: ServerMessage): FromRelay => ({ k: 'said', m });

/** A text message's JSON object, as it came. */
type Fields = Readonly<Record<string, unknown>>;

const isFields = (v: unknown): v is Fields =>
	typeof v === 'object' && v !== null;

/** The readers of the relay's text messages, by their kind (the JSON's `t`): one for each
 * kind there is, each reading the fields the page uses and no others. */
const TEXT: {
	readonly [K in ServerMessage['t']]: (v: Fields) => FromRelay;
} = {
	hello({ codec, files, resize }) {
		if (typeof codec !== 'string') return bad('a hello without its codec');
		if (
			!isFields(files) ||
			typeof files.dir !== 'string' ||
			typeof files.root !== 'string'
		)
			return bad('a hello without its files');
		if (!isFields(resize) || typeof resize.scale !== 'number')
			return bad('a hello without its resize');
		return said({
			t: 'hello',
			codec,
			files: { dir: files.dir, root: files.root },
			resize: { scale: resize.scale },
		});
	},
	clip: ({ d }) =>
		typeof d === 'string'
			? said({ t: 'clip', d })
			: bad('a clipboard without its text'),
	mic: ({ on }) =>
		typeof on === 'boolean'
			? said({ t: 'mic', on })
			: bad('a microphone word that says neither on nor off'),
	close: ({ why }) =>
		typeof why === 'string'
			? said({ t: 'close', why })
			: bad('a close without its cause'),
};
/** The readers of text messages, looked up by whatever a message's `t` is: a value that
 * is no kind's is the lookup's miss. */
const TEXT_BY_KIND: ReadonlyMap<unknown, (v: Fields) => FromRelay> = new Map(
	Object.entries(TEXT)
);

/** A text message's JSON, read by its kind. */
function readText(text: string): FromRelay {
	let v: unknown;
	try {
		v = JSON.parse(text);
	} catch {
		return bad('text that is not JSON');
	}
	if (!isFields(v)) return bad('text that is no message');
	const read = TEXT_BY_KIND.get(v.t);
	return read === undefined
		? bad(`a text message of kind ${JSON.stringify(v.t)}`)
		: read(v);
}

/** A binary message, read: `data` whole, and its bytes. */
type ReadBinary = (data: ArrayBuffer, b: Uint8Array) => FromRelay;

/** The page's own messages, which the relay never sends. */
const thePages: ReadBinary = (_, b) =>
	bad(`a message of type ${b[0]}, which is the page's to send`);

/** The readers of binary messages, by the name wire.json gives their type byte: one for
 * each type there is. */
const BINARY: { readonly [K in keyof typeof Bin]: ReadBinary } = {
	video(_, b) {
		const read = parseVideoHeader(b);
		return read.ok
			? {
					k: 'video',
					header: read.header,
					picture: b.subarray(VIDEO_HEADER_BYTES),
				}
			: bad(read.why);
	},
	audio: (_, b) => ({ k: 'audio', packet: b.subarray(1) }),
	cursor: (data) => ({ k: 'cursor', buf: data }),
	clipImage: (data) => ({ k: 'clipImage', buf: data }),
	clipImageUp: thePages,
	micOpus: thePages,
	took: thePages,
};

/** `of`'s members by the byte `bytes` gives each name, looked up by whatever a message's
 * first byte is: a byte that is no type's, or none at all, is the lookup's miss. */
function byByte<K extends string, V>(
	bytes: Readonly<Record<K, number>>,
	of: Readonly<Record<K, V>>
): ReadonlyMap<unknown, V> {
	const by = new Map<unknown, V>();
	for (const name in bytes) by.set(bytes[name], of[name]);
	return by;
}
const BINARY_BY_TYPE = byByte(Bin, BINARY);

/** What the relay sent: a text message by its kind, or a binary message by its type
 * byte. A kind or a type there is no reader for is none of the relay's. */
export function readRelay(data: string | ArrayBuffer): FromRelay {
	if (typeof data === 'string') return readText(data);
	const b = new Uint8Array(data);
	const read = BINARY_BY_TYPE.get(b.at(0));
	return read === undefined
		? bad(`a binary message whose type is [${b.subarray(0, 1).join()}]`)
		: read(data, b);
}

// ---- page <-> worker ----

/** Worker -> page. Relay messages other than close are forwarded as is; the session's
 * end is the status `closed`, with its cause. */
export type FromWorker =
	/** A decoded update, handed to the page to present. */
	| { t: 'frame'; frame: VideoFrame }
	/** The decoder now decodes this codec. */
	| { t: 'decodes'; codec: string }
	/** The decoder decodes nothing any more. */
	| { t: 'decodesNothing' }
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

/** Page -> desktop audio worklet, when the page makes it (its `processorOptions`). */
export interface PlayerOptions {
	/** The frames the audio device takes at a time: the context renders that many back to
	 * back, however small its render quantum. */
	deviceFrames: number;
}

/** Worker -> desktop audio worklet: one decoded chunk, planar. */
export interface PcmChunk {
	l: Float32Array;
	r: Float32Array;
}
