// Checks what the remote desktop holds (keys.ts) against generated sequences of a
// browser's key events, over many seeded runs:
//   node web/src/keyboard.test.ts
// The events are keydowns and keyups of characters, named keys, dead keys and the eight
// modifier keys (and AltGraph), with any modifier flags: now accurate, now set with no
// modifier keydown before (focus arrived with it held), now clear while a modifier key is
// down (its keyup was lost). Some are composing; a held key repeats; some keyups are for
// keys never pressed. AltGraph events carry its Ctrl+Alt flags.
//
// The fake remote holds the keysyms it was sent down. What it must hold is stated here
// from the flags, apart from the code under test. Before a key is pressed (not composing,
// with a keysym, not a repeat of a held key), the remote holds a modifier exactly when:
// - the event's flag for it is set, except Ctrl and Alt under AltGraph; and
// - a key of it is down that the flags have not since said is up, or the event is one
//   that needs it: Ctrl or Meta always, Shift or Alt only for a shortcut (Ctrl or Meta
//   set, no AltGraph) or a named key, never for a printable character.
// A repeat of a held key sends that key's repeat alone. The remote is never sent a down
// for a key it holds, nor an up for one it does not. After each run's last keystroke,
// the remote holds no modifier but those of keys still down.
import { Keys, type KeyAct, type KeyEvent, type Mod } from './keys.ts';

const RUNS = 20_000;
const EVENTS = 40;

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

interface Key {
	code: string;
	key: string;
	/** The keysym pressed; 0 for none. */
	ks: number;
	mod: Mod | null;
	named: boolean;
}
const modKey = (code: string, key: string, ks: number, mod: Mod): Key => ({
	code,
	key,
	ks,
	mod,
	named: false,
});
const MODIFIERS: Key[] = [
	modKey('ControlLeft', 'Control', 0xffe3, 'ctrl'),
	modKey('ControlRight', 'Control', 0xffe4, 'ctrl'),
	modKey('MetaLeft', 'Meta', 0xffeb, 'meta'),
	modKey('MetaRight', 'Meta', 0xffec, 'meta'),
	modKey('ShiftLeft', 'Shift', 0xffe1, 'shift'),
	modKey('ShiftRight', 'Shift', 0xffe2, 'shift'),
	modKey('AltLeft', 'Alt', 0xffe9, 'alt'),
	modKey('AltRight', 'Alt', 0xffea, 'alt'),
];
const ALTGR: Key = {
	code: 'AltRight',
	key: 'AltGraph',
	ks: 0xfe03,
	mod: null,
	named: true,
};
const plain = (code: string, key: string, ks: number, named = false): Key => ({
	code,
	key,
	ks,
	mod: null,
	named,
});
const OTHERS: Key[] = [
	plain('KeyA', 'a', 97),
	plain('KeyX', 'X', 88),
	plain('Digit2', '@', 64),
	plain('Digit5', 'é', 0xe9),
	plain('Space', ' ', 0x20, true),
	plain('ArrowLeft', 'ArrowLeft', 0xff51, true),
	plain('Enter', 'Enter', 0xff0d, true),
	plain('KeyE', 'Dead', 0),
	plain('KeyQ', 'Unidentified', 0),
];
/** The last keystroke of a run, which the others never use. */
const LAST = plain('KeyZ', 'z', 122);
const MOD_NAMES: Mod[] = ['ctrl', 'meta', 'shift', 'alt'];
/** The keysyms by which the remote holds a modifier. */
const KEYSYMS: Record<Mod, number[]> = {
	ctrl: [0xffe3, 0xffe4],
	meta: [0xffeb, 0xffec],
	shift: [0xffe1, 0xffe2],
	alt: [0xffe9, 0xffea],
};

const reached = {
	/** A key pressed with a flag set and no key of that modifier down: pressed for it. */
	inferredCtrlMeta: 0,
	inferredShiftAlt: 0,
	/** A printable character with Shift or Alt set alone: no modifier pressed. */
	printableLeftAlone: 0,
	/** Ctrl and Alt flags under AltGraph: neither pressed. */
	altGraphIgnored: 0,
	/** A modifier key down and its flag set: kept, nothing inferred. */
	explicitKept: 0,
	/** A modifier key down whose flag is clear (its keyup was lost): let go. */
	explicitDropped: 0,
	/** A composing event, or one with no keysym: no key, modifiers still let go. */
	composingOrDead: 0,
	/** A repeat of a held key: sent alone. */
	repeatHeld: 0,
	/** A repeat of a key not held: pressed as a key down. */
	repeatNotHeld: 0,
	/** A keyup of a key never pressed. */
	upNotDown: 0,
	/** A modifier inferred, then let go at the key's keyup. */
	retired: 0,
	/** An inferred modifier then pressed explicitly: replaced. */
	replaced: 0,
	/** A modifier key of the right side. */
	rightSide: 0,
};

for (let run = 0; run < RUNS; run++) {
	const draw = random(run + 1);
	const chance = (percent: number) => draw(100) < percent;
	const pick = <T>(xs: readonly T[]) => xs[draw(xs.length)];
	const fail = (what: string): never => {
		throw new Error(`run ${run}: ${what}`);
	};

	// The fake remote.
	const remote = new Set<number>();
	let log: { ks: number; act: KeyAct }[] = [];
	const keys = new Keys(false, {
		key(ks, act) {
			log.push({ ks, act });
			if (act === 'down') {
				if (remote.has(ks)) fail(`down for held ${ks.toString(16)}`);
				remote.add(ks);
			} else if (act === 'up') {
				if (!remote.delete(ks)) fail(`up for ${ks.toString(16)} not held`);
			}
		},
		releaseAll: () => remote.clear(),
	});
	const remoteMods = () =>
		new Set(MOD_NAMES.filter((m) => KEYSYMS[m].some((ks) => remote.has(ks))));

	// The browser: the keys physically down. The page: the keys it has pressed, by code.
	const physical = new Set<Key>();
	const pressed = new Map<string, Key>();
	const pressedMods = () =>
		new Set([...pressed.values()].flatMap((k) => (k.mod ? [k.mod] : [])));

	/** Flags: accurate, lost (a keyup was), or set with no key down; AltGraph's Ctrl+Alt. */
	function flagsOf(altGraph: boolean) {
		const f: Record<Mod, boolean> = {
			ctrl: false,
			meta: false,
			shift: false,
			alt: false,
		};
		for (const k of physical) if (k.mod) f[k.mod] = true;
		for (const m of MOD_NAMES) {
			const r = draw(10);
			if (f[m] && r === 0) f[m] = false;
			else if (!f[m] && r < 2) f[m] = true;
		}
		if (altGraph) f.ctrl = f.alt = true;
		return f;
	}

	/** Sends an event to the code under test and checks what the remote then holds. */
	function send(type: 'down' | 'up', k: Key, repeat = false) {
		const altGraph = physical.has(ALTGR);
		const f = flagsOf(altGraph);
		const composing = chance(8);
		const e: KeyEvent = {
			code: k.code,
			key: k.key,
			repeat,
			isComposing: composing,
			ctrlKey: f.ctrl,
			metaKey: f.meta,
			shiftKey: f.shift,
			altKey: f.alt,
			altGraph,
		};
		/** The flag for a modifier, but not Ctrl or Alt under AltGraph. */
		const flag = (m: Mod) =>
			f[m] && !(altGraph && (m === 'ctrl' || m === 'alt'));
		const drop = () => {
			for (const m of MOD_NAMES)
				if (!flag(m))
					for (const [c, p] of pressed) if (p.mod === m) pressed.delete(c);
		};
		log = [];
		if (type === 'up') {
			if (!pressed.has(k.code)) reached.upNotDown++;
			const inferred = [...remoteMods()].some((m) => !pressedMods().has(m));
			keys.keyUp(e);
			pressed.delete(k.code);
			drop();
			if (!k.mod) {
				for (const m of remoteMods())
					if (!pressedMods().has(m)) fail(`remote still holds inferred ${m}`);
				if (inferred) reached.retired++;
			}
			return;
		}
		const typed = !composing && k.ks !== 0;
		if (typed && repeat && pressed.has(k.code)) {
			keys.keyDown(e, () => false);
			if (log.length !== 1 || log[0].act !== 'repeat' || log[0].ks !== k.ks)
				fail('a repeat of a held key did more than repeat it');
			reached.repeatHeld++;
			return;
		}
		const lostBefore = pressedMods().size;
		drop();
		const explicit = pressedMods();
		if (pressedMods().size < lostBefore) reached.explicitDropped++;
		const replaces = k.mod !== null && flag(k.mod) && !explicit.has(k.mod);
		const inferredBefore = replaces && remoteMods().has(k.mod!);
		keys.keyDown(e, () => false);
		if (!typed) {
			reached.composingOrDead++;
			return;
		}
		if (repeat) reached.repeatNotHeld++;
		pressed.set(k.code, k);
		if (k.mod) {
			if (inferredBefore) reached.replaced++;
			if (k.code.endsWith('Right')) reached.rightSide++;
			return;
		}
		const shortcut = (f.ctrl || f.meta) && !altGraph;
		const want = new Set<Mod>();
		for (const m of MOD_NAMES) {
			const needed = m === 'ctrl' || m === 'meta' || shortcut || k.named;
			if (f[m] && !flag(m)) reached.altGraphIgnored++;
			if (!flag(m)) continue;
			if (explicit.has(m)) {
				want.add(m);
				reached.explicitKept++;
			} else if (needed) {
				want.add(m);
				reached[
					m === 'ctrl' || m === 'meta' ? 'inferredCtrlMeta' : 'inferredShiftAlt'
				]++;
			} else reached.printableLeftAlone++;
		}
		const got = remoteMods();
		if (got.size !== want.size || [...want].some((m) => !got.has(m)))
			fail(
				`remote holds {${[...got]}} for ${k.code} ${JSON.stringify(f)} ` +
					`altGraph ${altGraph}, wants {${[...want]}}`
			);
	}

	/** A key the browser has up, and the page has not pressed (one code, one key). */
	const free = (k: Key) =>
		![...physical].some((p) => p.code === k.code) && !pressed.has(k.code);

	for (let i = 0; i < EVENTS; i++) {
		const r = draw(100);
		const downs = [...physical];
		if (r < 20) {
			const k = pick([...MODIFIERS, ALTGR]);
			if (free(k)) {
				physical.add(k);
				send('down', k);
			}
		} else if (r < 40) {
			if (downs.length) {
				const k = pick(downs);
				physical.delete(k);
				// A lost keyup: the browser sends none.
				if (!(k.mod && chance(30))) send('up', k);
			}
		} else if (r < 70) {
			const k = pick(OTHERS);
			if (free(k)) {
				physical.add(k);
				send('down', k);
			}
		} else if (r < 80) {
			const k = pick(OTHERS);
			if (!physical.has(k)) send('up', k);
		} else if (downs.length) {
			send('down', pick(downs), true);
		}
	}

	// The last keystroke: afterwards the remote holds nothing the flags added.
	const k = LAST;
	physical.add(k);
	send('down', k);
	physical.delete(k);
	send('up', k);
}

for (const [name, n] of Object.entries(reached))
	if (n === 0) throw new Error(`the generator never reached: ${name}`);
console.log(`keyboard: ${RUNS} runs ok`, reached);
