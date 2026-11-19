// Checks the audio player's ring (ring.ts) against a list, over many seeded runs:
//   node web/src/ring.test.ts
// A run makes a ring of a drawn most and gives it chunks of drawn lengths (none, a few
// samples, about a packet beside the ring, a burst of them, more than the ring holds)
// between asks for drawn lengths (a few samples, up to all the ring holds), in a drawn
// order. Every sample is its own
// number, the left channel's the count of samples so far and the right's its negative.
//
// The oracle is a list of the samples waiting: a chunk is appended and the list cut from
// its front to the ring's most; an ask takes its length from the front if the list holds
// that many, and nothing if not. So after every step: the ring plays exactly the list's
// samples, each at most once, in the order they came, both channels together; it plays
// nothing, and leaves the output as it was, exactly when less waits than was asked for;
// and it holds what the list holds, never more than its most.
import { Ring } from './ring.ts';

const RUNS = 5_000;
const STEPS = 200;

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

/** What an output holds before the ring is asked: not a sample. */
const UNTOUCHED = 0.5;

/** How often each clause was reached, over every run. */
const reached = {
	played: 0,
	short: 0,
	shortHoldingSome: 0,
	dropped: 0,
	chunkOverMost: 0,
	full: 0,
	emptied: 0,
	wrapped: 0,
};

function run(seed: number) {
	const draw = random(seed);
	const fail = (why: string) => {
		throw new Error(`seed ${seed}: ${why}`);
	};
	const most = 1 + draw(draw(2) ? 12 : 120);
	const packet = 1 + draw(most);
	const ring = new Ring(most);
	/** The samples waiting, oldest first, and the count of samples that ever came. */
	let waiting: number[] = [];
	let came = 0;
	for (let step = 0; step < STEPS; step++) {
		if (draw(2)) {
			const n = [
				0,
				1 + draw(8),
				packet,
				packet * (1 + draw(5)),
				most + draw(most),
			][draw(5)];
			const l = Float32Array.from({ length: n }, (_, i) => came + i + 1);
			const r = l.map((x) => -x);
			came += n;
			ring.push(l, r);
			if (n > most) reached.chunkOverMost++;
			if (waiting.length + n > most) reached.dropped++;
			if (came > most) reached.wrapped++;
			waiting = [...waiting, ...l].slice(-most);
			if (waiting.length === most) reached.full++;
		} else {
			const n = [1 + draw(8), 1 + draw(most), most][draw(3)];
			const l = new Float32Array(n).fill(UNTOUCHED);
			const r = new Float32Array(n).fill(UNTOUCHED);
			ring.play(l, r);
			if (waiting.length >= n) {
				const front = waiting.slice(0, n);
				waiting = waiting.slice(n);
				if (l.join() !== front.join())
					fail(`step ${step}: played ${l.join()}, not ${front.join()}`);
				if (r.join() !== front.map((x) => -x).join())
					fail(`step ${step}: the right channel is not the left's`);
				reached.played++;
				if (!waiting.length) reached.emptied++;
			} else {
				if (l.some((x) => x !== UNTOUCHED) || r.some((x) => x !== UNTOUCHED))
					fail(`step ${step}: an output written with less than it waiting`);
				reached.short++;
				if (waiting.length) reached.shortHoldingSome++;
			}
		}
		if (ring.fill !== waiting.length)
			fail(`step ${step}: holds ${ring.fill}, not ${waiting.length}`);
	}
}

for (let seed = 1; seed <= RUNS; seed++) run(seed);
for (const [clause, n] of Object.entries(reached))
	if (!n) throw new Error(`no run reached ${clause}`);
console.log(`${RUNS} runs, each clause reached: ${JSON.stringify(reached)}`);
