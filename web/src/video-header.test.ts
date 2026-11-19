// Checks the worker's header reader against web/src/video-header.json: messages as the
// page gets them, the relay's type byte first, then the bytes of src/wire.rs's `Header`,
// whose tests check its reader and writer against the same file:
//   node web/src/video-header.test.ts
import fixture from './video-header.json' with { type: 'json' };
import { CODINGS, parseVideoHeader, VIDEO_HEADER_BYTES } from './protocol.ts';

const bytes = (hex: string) => {
	const digits = hex.replaceAll(' ', '');
	return Uint8Array.from({ length: digits.length / 2 }, (_, i) =>
		parseInt(digits.slice(2 * i, 2 * i + 2), 16)
	);
};

// Every field the fixture names and every field the reader returns: one it leaves out
// reads as undefined, one the fixture does not name is expected as undefined.
for (const { header, picture, ...fields } of fixture) {
	const message = new Uint8Array([...bytes(header), ...bytes(picture)]);
	const read = parseVideoHeader(message);
	if (!read.ok) throw new Error(`${header}: refused: ${read.why}`);
	const got = new Map<string, unknown>(Object.entries(read.header));
	const named = new Map<string, unknown>(Object.entries(fields));
	for (const name of new Set([...named.keys(), ...got.keys()]))
		if (got.get(name) !== named.get(name))
			throw new Error(
				`${header}: ${name} reads ${got.get(name)}, not ${named.get(name)}`
			);
	const rest = message.subarray(VIDEO_HEADER_BYTES).join();
	if (rest !== bytes(picture).join())
		throw new Error(`${header}: the picture reads ${rest}`);
}

// Over every value of the flags byte: a header is read exactly when the byte is a coding's,
// and then as that coding.
for (let flags = 0; flags < 256; flags++) {
	const message = new Uint8Array(VIDEO_HEADER_BYTES + 1);
	message[1] = flags;
	const read = parseVideoHeader(message);
	const coding = flags < CODINGS.length ? CODINGS[flags] : 'none';
	const got = read.ok ? read.header.coding : 'none';
	if (got !== coding) throw new Error(`flags ${flags} read as ${got}`);
}

// A message is an update only with a whole header and a picture after it, at every length
// up to a header's and past it.
for (let n = 0; n <= VIDEO_HEADER_BYTES + 3; n++) {
	const read = parseVideoHeader(new Uint8Array(n));
	if (read.ok !== n > VIDEO_HEADER_BYTES)
		throw new Error(`a message of ${n} bytes read: ${read.ok}`);
}
