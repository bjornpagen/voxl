// Checks the worker's presenter (track.ts) against a fake track and page, over many seeded
// runs:
//   node web/src/track.test.ts
// A run hands the presenter pictures, each its own number, of sizes drawn from a few (so
// a size often repeats, and sometimes changes); between them the fake track ends the write
// it holds, at any time, and now and then the session closes. Pictures often come several
// to a write, as a decoder outruns a slow track.
//
// The oracle is a list kept apart: the picture the track holds, and the one waiting. After
// every step:
// - the track holds at most one write, and a picture is written exactly when it came with
//   no write in flight, or when the write before it ended and it was the newest to have
//   come meanwhile;
// - a picture is let go exactly when a newer one came while it waited; every picture is
//   written or let go at most once, and at the end of a run, with every write ended, each
//   exactly once: none is kept;
// - the pictures written are in the order they came;
// - the page is told a size exactly when a picture's size differs from the last it was
//   told this session, or is the session's first, before that picture is written.
import { Track } from './track.ts';

const RUNS = 20_000;
const STEPS = 120;

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

const SIZES = [
	[1280, 800],
	[1280, 801],
	[3024, 1896],
] as const;

/** Clauses some run must reach. */
const reached = {
	writtenAtOnce: 0,
	waited: 0,
	replaced: 0,
	writtenAfter: 0,
	sizeChanged: 0,
	sizeSame: 0,
	sizeAfterClose: 0,
	endedIdle: 0,
};

const same = (a: unknown, b: unknown) =>
	JSON.stringify(a) === JSON.stringify(b);

function run(seed: number) {
	const draw = random(seed);
	const fail = (why: string) => {
		throw new Error(`seed ${seed}: ${why}`);
	};
	/** What the fake world was told since the step began. */
	let wrote: number[] = [];
	let letGone: number[] = [];
	let sizes: string[] = [];
	const track = new Track<number>({
		write: (p) => wrote.push(p),
		letGo: (p) => letGone.push(p),
		size: (w, h) => sizes.push(`${w}x${h}`),
	});
	/** The oracle: the picture the track holds, the one waiting, and the size told. */
	let held: number | null = null;
	let waiting: number | null = null;
	let told: string | null = null;
	const fate = new Map<number, 'written' | 'let go'>();
	let lastWritten = -1;
	let next = 0;
	const step = (event: number) => {
		wrote = [];
		letGone = [];
		sizes = [];
		const wantWrote: number[] = [];
		const wantLetGo: number[] = [];
		const wantSizes: string[] = [];
		if (event < 5) {
			const picture = next++;
			const [w, h] = SIZES[draw(8) === 0 ? draw(SIZES.length) : 0];
			const size = `${w}x${h}`;
			if (told !== size) {
				if (told === null) reached.sizeAfterClose++;
				else reached.sizeChanged++;
				wantSizes.push(size);
				told = size;
			} else reached.sizeSame++;
			if (held === null) {
				held = picture;
				wantWrote.push(picture);
				reached.writtenAtOnce++;
			} else {
				if (waiting === null) reached.waited++;
				else {
					wantLetGo.push(waiting);
					reached.replaced++;
				}
				waiting = picture;
			}
			track.present(picture, w, h);
		} else if (event < 9) {
			if (held === null) reached.endedIdle++;
			held = waiting;
			waiting = null;
			if (held !== null) {
				wantWrote.push(held);
				reached.writtenAfter++;
			}
			track.written();
		} else {
			told = null;
			track.closed();
		}
		if (!same(wrote, wantWrote))
			fail(`wrote ${JSON.stringify(wrote)}, not ${JSON.stringify(wantWrote)}`);
		if (!same(letGone, wantLetGo))
			fail(
				`let go ${JSON.stringify(letGone)}, not ${JSON.stringify(wantLetGo)}`
			);
		if (!same(sizes, wantSizes))
			fail(`told ${JSON.stringify(sizes)}, not ${JSON.stringify(wantSizes)}`);
		for (const p of wrote) {
			if (fate.has(p)) fail(`picture ${p} written after it was ${fate.get(p)}`);
			if (p <= lastWritten) fail(`picture ${p} written after ${lastWritten}`);
			lastWritten = p;
			fate.set(p, 'written');
		}
		for (const p of letGone) {
			if (fate.has(p)) fail(`picture ${p} let go after it was ${fate.get(p)}`);
			fate.set(p, 'let go');
		}
	};
	// Pictures come in bursts or singly; the track is quick or slow.
	const quick = draw(2) === 0;
	for (let i = 0; i < STEPS; i++) step(draw(quick ? 14 : 10) % 10);
	// Every write ends: nothing is kept.
	step(5);
	step(5);
	for (let p = 0; p < next; p++)
		if (!fate.has(p)) fail(`picture ${p} neither written nor let go`);
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
