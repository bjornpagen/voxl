// Checks the worker's header reader against web/src/video-header.json, the messages
// src/video.rs's tests and bench/linkviewer.py check theirs against too:
//   node web/src/video-header.test.ts
import fixture from './video-header.json' with { type: 'json' };
import { parseVideoHeader, VIDEO_HEADER_BYTES } from './protocol.ts';

const bytes = (hex: string) => {
	const digits = hex.replaceAll(' ', '');
	return Uint8Array.from({ length: digits.length / 2 }, (_, i) =>
		parseInt(digits.slice(2 * i, 2 * i + 2), 16)
	);
};

for (const want of fixture) {
	const message = new Uint8Array([
		...bytes(want.header),
		...bytes(want.picture),
	]);
	const got = parseVideoHeader(message);
	for (const [field, value] of Object.entries(got)) {
		const expected = want[field as keyof typeof want];
		if (value !== expected)
			throw new Error(
				`${want.header}: ${field} reads ${value}, not ${expected}`
			);
	}
	const picture = message.subarray(VIDEO_HEADER_BYTES).join();
	if (picture !== bytes(want.picture).join())
		throw new Error(`${want.header}: the picture reads ${picture}`);
}
