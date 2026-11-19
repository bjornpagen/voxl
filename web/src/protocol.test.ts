// Checks the reader of what the relay sends (protocol.ts's `readRelay`) over generated
// messages:
//   node web/src/protocol.test.ts
// A message is drawn as what it means and written as the relay would write it: a video
// update (its type byte, flags, version and pipe time, big-endian, then a picture), an
// Opus packet, a cursor's or a clipboard's image, a hello (with its resize scale, which
// every hello has, and with fields the page does not use), a clipboard's text, a
// microphone word, or the relay's close. Then, half the time, it is spoiled in one drawn way: its type
// byte changed to one the relay never sends (the page's own among them), its flags given
// another bit, its header cut short, its picture left out, its JSON cut, its kind
// renamed, or a field it needs removed or given another type.
//
// The oracle is the meaning drawn: a message as written reads back as exactly what it
// was drawn as (the picture's and the packet's bytes, the image's own buffer, the text's
// fields and no others), and a spoiled one reads as no message of the relay's.
import {
	Bin,
	CODINGS,
	readRelay,
	VIDEO_HEADER_BYTES,
	type FromRelay,
	type ServerMessage,
} from './protocol.ts';

const RUNS = 200_000;

/** A small seeded generator (mulberry32): runs repeat from their seed alone. */
function random(seed: number) {
	let a = seed >>> 0;
	return (n: number) => {
		a = (a + 0x6d2b79f5) >>> 0;
		let t = a;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) % n;
	};
}

/** How often each clause was reached, over every run. */
const reached = {
	video: 0,
	audio: 0,
	image: 0,
	hello: 0,
	clip: 0,
	mic: 0,
	close: 0,
	noType: 0,
	empty: 0,
	flags: 0,
	short: 0,
	noPicture: 0,
	notJson: 0,
	noMessage: 0,
	otherKind: 0,
	fieldGone: 0,
	fieldOther: 0,
};

const fail = (seed: number, why: string): never => {
	throw new Error(`seed ${seed}: ${why}`);
};

/** How a binary message is spoiled, or not: drawn from here, so each way is a case. */
const BINARY_SPOILS = [
	'type',
	'empty',
	'flags',
	'short',
	'noPicture',
	'none',
	'none',
	'none',
] as const;

/** The kinds of text message drawn. */
const KINDS = ['hello', 'clip', 'mic', 'close'] as const;

/** How a text message is spoiled, or not. */
const TEXT_SPOILS = [
	'cut',
	'notMessage',
	'otherKind',
	'fieldGone',
	'fieldOther',
	'none',
	'none',
	'none',
	'none',
	'none',
] as const;

/** The type bytes the relay sends. */
const SENT = new Set<number>([Bin.video, Bin.audio, Bin.cursor, Bin.clipImage]);

function binary(seed: number, draw: (n: number) => number) {
	const bytes = (n: number) => Uint8Array.from({ length: n }, () => draw(256));
	const kind = draw(4);
	// An update has a picture; anything else may carry nothing.
	const body = bytes(kind === 0 ? 1 + draw(40) : draw(40));
	let message: Uint8Array;
	let check: (got: FromRelay, buf: ArrayBuffer) => boolean;
	if (kind === 0) {
		const coding = CODINGS[draw(CODINGS.length)];
		const version = draw(2 ** 32);
		const pipe = draw(2 ** 32);
		message = new Uint8Array(VIDEO_HEADER_BYTES + body.length);
		const view = new DataView(message.buffer);
		view.setUint8(0, Bin.video);
		view.setUint8(1, CODINGS.indexOf(coding));
		view.setUint32(2, version);
		view.setUint32(6, pipe);
		message.set(body, VIDEO_HEADER_BYTES);
		check = (got) =>
			got.k === 'video' &&
			got.header.coding === coding &&
			got.header.version === version &&
			got.header.pipe === pipe &&
			Object.keys(got.header).length === 3 &&
			got.picture.join() === body.join();
		reached.video++;
	} else if (kind === 1) {
		message = Uint8Array.from([Bin.audio, ...body]);
		check = (got) => got.k === 'audio' && got.packet.join() === body.join();
		reached.audio++;
	} else {
		const k = kind === 2 ? 'cursor' : 'clipImage';
		message = Uint8Array.from([Bin[k], ...body]);
		check = (got, buf) => got.k === k && got.buf === buf;
		reached.image++;
	}
	// Spoiled, half the time, in a way that fits the message: whether it was.
	const spoiled = ((): boolean => {
		switch (BINARY_SPOILS[draw(BINARY_SPOILS.length)]) {
			case 'type': {
				const type = draw(256);
				if (SENT.has(type)) return false;
				message[0] = type;
				reached.noType++;
				return true;
			}
			case 'empty':
				message = new Uint8Array(0);
				reached.empty++;
				return true;
			case 'flags':
				if (kind !== 0) return false;
				message[1] |= 2 << draw(7);
				reached.flags++;
				return true;
			case 'short':
				if (kind !== 0) return false;
				message = message.slice(0, 1 + draw(VIDEO_HEADER_BYTES - 1));
				reached.short++;
				return true;
			case 'noPicture':
				if (kind !== 0) return false;
				message = message.slice(0, VIDEO_HEADER_BYTES);
				reached.noPicture++;
				return true;
			case 'none':
				return false;
		}
	})();
	const buf = message.buffer.slice(
		message.byteOffset,
		message.byteOffset + message.byteLength
	);
	const got = readRelay(buf);
	if (spoiled ? got.k !== 'bad' : !check(got, buf))
		fail(seed, `${message.join()} read as ${JSON.stringify(got)}`);
}

function text(seed: number, draw: (n: number) => number) {
	const word = () =>
		Array.from({ length: draw(8) }, () => 'a/é" \\7{'[draw(8)]).join('');
	/** The message as the daemon writes it, the fields the reader must find in it, and what
	 * it means. */
	let written: Record<string, unknown>;
	let needed: string[];
	let meant: ServerMessage;
	switch (KINDS[draw(KINDS.length)]) {
		case 'hello': {
			const files = { root: word(), dir: word() };
			const codec = `vp09.00.${10 + draw(52)}.08`;
			const scale = (1 + draw(300)) / 100;
			meant = {
				t: 'hello',
				codec,
				files: { dir: files.dir, root: files.root },
				resize: { scale },
			};
			written = {
				t: 'hello',
				codec,
				audio: draw(2) === 1,
				files,
				resize: { scale },
			};
			needed = ['codec', 'files', 'resize'];
			reached.hello++;
			break;
		}
		case 'clip':
			meant = { t: 'clip', d: word() };
			written = { ...meant };
			needed = ['d'];
			reached.clip++;
			break;
		case 'mic':
			meant = { t: 'mic', on: draw(2) === 1 };
			written = { ...meant };
			needed = ['on'];
			reached.mic++;
			break;
		case 'close':
			meant = { t: 'close', why: word() };
			written = { ...meant };
			needed = ['why'];
			reached.close++;
			break;
	}
	let json = JSON.stringify(written);
	const field = needed[draw(needed.length)];
	// Spoiled, or not: whether it was.
	const spoiled = ((): boolean => {
		switch (TEXT_SPOILS[draw(TEXT_SPOILS.length)]) {
			case 'cut':
				json = json.slice(0, 1 + draw(json.length - 1));
				reached.notJson++;
				return true;
			case 'notMessage':
				json = ['null', '7', '"hello"', '[]', 'true'][draw(5)];
				reached.noMessage++;
				return true;
			case 'otherKind':
				json = JSON.stringify({
					...written,
					t: ['lane', 'Hello', 7, null][draw(4)],
				});
				reached.otherKind++;
				return true;
			case 'fieldGone': {
				const { [field]: _gone, ...rest } = written;
				json = JSON.stringify(rest);
				reached.fieldGone++;
				return true;
			}
			case 'fieldOther':
				json = JSON.stringify({
					...written,
					[field]: [7, null, [], {}][draw(4)],
				});
				reached.fieldOther++;
				return true;
			case 'none':
				return false;
		}
	})();
	const got = readRelay(json);
	if (spoiled) {
		if (got.k !== 'bad') fail(seed, `${json} read as ${JSON.stringify(got)}`);
		return;
	}
	if (got.k !== 'said' || JSON.stringify(got.m) !== JSON.stringify(meant))
		fail(seed, `${json} read as ${JSON.stringify(got)}`);
}

for (let seed = 1; seed <= RUNS; seed++) {
	const draw = random(seed);
	if (draw(2)) binary(seed, draw);
	else text(seed, draw);
}
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
