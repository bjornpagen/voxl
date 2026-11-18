// Checks the worker's header reader against web/src/video-header.json: messages as the
// page gets them, the relay's type byte first, then the bytes src/video.rs's `Header`
// writes and its tests check against the same file:
//   node web/src/video-header.test.ts
import fixture from './video-header.json' with { type: 'json' };
import {
	parseVideoHeader,
	VIDEO_HEADER_BYTES,
	type VideoHeader,
} from './protocol.ts';

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
	const got = parseVideoHeader(message);
	for (const name of new Set([...Object.keys(fields), ...Object.keys(got)])) {
		const value = got[name as keyof VideoHeader];
		const expected = fields[name as keyof typeof fields];
		if (value !== expected)
			throw new Error(`${header}: ${name} reads ${value}, not ${expected}`);
	}
	const rest = message.subarray(VIDEO_HEADER_BYTES).join();
	if (rest !== bytes(picture).join())
		throw new Error(`${header}: the picture reads ${rest}`);
}
