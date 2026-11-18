// What the remote desktop holds down, and how a browser's key events change it. Keys go
// to the server as X keysyms. A browser can deliver a character with ctrlKey, metaKey,
// altKey or shiftKey set and no keydown of that modifier before it (focus arrived with
// the modifier held, or a native webview leaves it out), so each event's flags are
// reconciled with the modifiers held: the missing ones are pressed before the key
// (inferred) and let go after the chord; a modifier the flags say is up is let go.
// Browser-free, so keyboard.test.ts can run it against a fake of the remote.

export type KeyAct = 'down' | 'repeat' | 'up';

/** Where the keys go. */
export interface Output {
	key(ks: number, act: KeyAct): void;
	releaseAll(): void;
}

/** What the page needs of a browser's KeyboardEvent. */
export interface KeyEvent {
	code: string;
	key: string;
	repeat: boolean;
	isComposing: boolean;
	ctrlKey: boolean;
	metaKey: boolean;
	shiftKey: boolean;
	altKey: boolean;
	/** `getModifierState('AltGraph')`: Ctrl and Alt flags then mean AltGraph, not shortcuts. */
	altGraph: boolean;
}

const NAMED: Record<string, number> = {
	Backspace: 0xff08,
	Tab: 0xff09,
	Enter: 0xff0d,
	Escape: 0xff1b,
	Delete: 0xffff,
	Home: 0xff50,
	ArrowLeft: 0xff51,
	ArrowUp: 0xff52,
	ArrowRight: 0xff53,
	ArrowDown: 0xff54,
	PageUp: 0xff55,
	PageDown: 0xff56,
	End: 0xff57,
	Insert: 0xff63,
	CapsLock: 0xffe5,
	NumLock: 0xff7f,
	ScrollLock: 0xff14,
	Pause: 0xff13,
	PrintScreen: 0xff61,
	ContextMenu: 0xff67,
	AltGraph: 0xfe03,
	' ': 0x20,
};
for (let i = 1; i <= 24; i++) NAMED['F' + i] = 0xffbd + i;

/** Keysym of a printable character (Latin-1 directly, Unicode via 0x01000000 + code point). */
export const charKeysym = (cp: number) =>
	(cp >= 0x20 && cp <= 0x7e) || (cp >= 0xa0 && cp <= 0xff)
		? cp
		: 0x01000000 + cp;

const XK_CONTROL_L = 0xffe3,
	XK_CONTROL_R = 0xffe4,
	XK_SUPER_L = 0xffeb,
	XK_SUPER_R = 0xffec;
const BY_CODE: Record<string, number> = {
	ShiftLeft: 0xffe1,
	ShiftRight: 0xffe2,
	AltLeft: 0xffe9,
	AltRight: 0xffea,
	NumpadEnter: 0xff0d,
};

export type Mod = 'ctrl' | 'meta' | 'shift' | 'alt';
const MODS: readonly Mod[] = ['ctrl', 'meta', 'shift', 'alt'];
const LEFT: Record<Mod, string> = {
	ctrl: 'ControlLeft',
	meta: 'MetaLeft',
	shift: 'ShiftLeft',
	alt: 'AltLeft',
};

/** The modifier a key is, if it is one. AltGraph is a key of its own, not Alt. */
function modOf(code: string, key: string): Mod | null {
	if (key === 'AltGraph') return null;
	if (code.startsWith('Control')) return 'ctrl';
	if (code.startsWith('Meta') || code.startsWith('OS')) return 'meta';
	if (code.startsWith('Shift')) return 'shift';
	if (code.startsWith('Alt')) return 'alt';
	return null;
}

/** How the remote holds a modifier: not at all; by a modifier key the viewer pressed
 * (left or right, kept as it is); or because an event's flags said so, with no such key. */
export type Held = 'up' | 'explicit' | 'inferred';

/** What reconciling a key event's flags does to each modifier: it is let go (`up`) or
 * pressed as inferred. A modifier not listed stays as it is; one pressed by a key stays
 * while its flag is set. The event's own modifier,
 * if it is one, is left to its own press, except an inferred one it replaces. */
export function reconcile(
	e: KeyEvent,
	held: Record<Mod, Held>,
	infer: boolean
): Partial<Record<Mod, 'up' | 'inferred'>> {
	const flags: Record<Mod, boolean> = {
		ctrl: e.ctrlKey,
		meta: e.metaKey,
		shift: e.shiftKey,
		alt: e.altKey,
	};
	// AltGraph often reports Ctrl+Alt, but those flags are not a shortcut.
	const shortcut = (e.ctrlKey || e.metaKey) && !e.altGraph;
	const named = NAMED[e.key] !== undefined;
	const own = modOf(e.code, e.key);
	const moves: Partial<Record<Mod, 'up' | 'inferred'>> = {};
	for (const m of MODS) {
		const active = flags[m] && !(e.altGraph && (m === 'ctrl' || m === 'alt'));
		if (!active) {
			if (held[m] !== 'up') moves[m] = 'up';
		} else if (m === own) {
			if (held[m] === 'inferred') moves[m] = 'up';
		} else if (infer) {
			// A printable Shift or Option result is already in e.key; only a shortcut or a
			// named key (an arrow selecting) needs the modifier.
			const needed = m === 'ctrl' || m === 'meta' || shortcut || named;
			if (held[m] === 'up' && needed) moves[m] = 'inferred';
			else if (held[m] === 'inferred' && !needed) moves[m] = 'up';
		}
	}
	return moves;
}

export class Keys {
	/** Keys the viewer pressed, by code. */
	private down = new Map<string, { ks: number; mod: Mod | null }>();
	/** Modifiers pressed from an event's flags, with their keysyms. */
	private inferred = new Map<Mod, number>();

	private isMac: boolean;
	private out: Output;
	constructor(isMac: boolean, out: Output) {
		this.isMac = isMac;
		this.out = out;
	}

	// The shortcut key is Command on a Mac and Control elsewhere: the desktop (Linux) gets
	// Control for a Mac viewer's Command.
	private modifierKeysym(code: string): number | undefined {
		const isCtrl = code.startsWith('Control');
		const isMeta = code.startsWith('Meta') || code.startsWith('OS');
		if (!isCtrl && !isMeta) return undefined;
		const right = code.endsWith('Right');
		const ctrl = right ? XK_CONTROL_R : XK_CONTROL_L;
		const sup = right ? XK_SUPER_R : XK_SUPER_L;
		if (this.isMac && code.startsWith('Meta')) return ctrl;
		return isCtrl ? ctrl : sup;
	}

	keysym(e: KeyEvent): number {
		const mod = this.modifierKeysym(e.code);
		if (mod !== undefined) return mod;
		if (BY_CODE[e.code] !== undefined) {
			if (e.code === 'AltRight' && e.key === 'AltGraph') return 0xfe03;
			return BY_CODE[e.code];
		}
		const k = e.key;
		if (NAMED[k] !== undefined) return NAMED[k];
		if (!k || k === 'Dead' || k === 'Unidentified' || k === 'Process') return 0;
		if ([...k].length !== 1) return 0;
		return charKeysym(k.codePointAt(0)!);
	}

	/** The keysym of a modifier an event's flags press: its left key. */
	private modKeysym(m: Mod): number {
		return this.modifierKeysym(LEFT[m]) ?? BY_CODE[LEFT[m]];
	}

	private heldOf(): Record<Mod, Held> {
		const held = { ctrl: 'up', meta: 'up', shift: 'up', alt: 'up' } as Record<
			Mod,
			Held
		>;
		for (const m of this.inferred.keys()) held[m] = 'inferred';
		for (const { mod } of this.down.values()) if (mod) held[mod] = 'explicit';
		return held;
	}

	private apply(e: KeyEvent, infer: boolean) {
		for (const [m, to] of Object.entries(
			reconcile(e, this.heldOf(), infer)
		) as [Mod, 'up' | 'inferred'][]) {
			for (const [code, k] of this.down)
				if (to === 'up' && k.mod === m) this.release(code);
			const ks = this.inferred.get(m);
			if (ks !== undefined) {
				this.inferred.delete(m);
				this.out.key(ks, 'up');
			}
			if (to === 'inferred') {
				const press = this.modKeysym(m);
				this.inferred.set(m, press);
				this.out.key(press, 'down');
			}
		}
	}

	/** Presses a key that is not a modifier (a paste held back until the clipboard is on
	 * the desktop). */
	hold(code: string, ks: number) {
		this.down.set(code, { ks, mod: null });
		this.out.key(ks, 'down');
	}
	/** Presses a key the viewer pressed. */
	private strike(e: KeyEvent, ks: number) {
		this.down.set(e.code, { ks, mod: modOf(e.code, e.key) });
		this.out.key(ks, 'down');
	}
	/** Taps a keysym (text that arrives without usable keydowns). */
	tap(ks: number) {
		this.out.key(ks, 'down');
		this.out.key(ks, 'up');
	}
	private release(code: string) {
		const k = this.down.get(code);
		if (!k) return;
		this.down.delete(code);
		this.out.key(k.ks, 'up');
	}

	/** A keydown. `defer` may take over pressing the key (it will call `hold`) once its
	 * modifiers are in place. Returns whether the page consumed the event. */
	keyDown(e: KeyEvent, defer: (e: KeyEvent, ks: number) => boolean): boolean {
		const ks = e.isComposing ? 0 : this.keysym(e);
		if (!ks) {
			this.apply(e, false); // the flags still say which modifiers are up
			return false;
		}
		const held = this.down.get(e.code);
		// A held key's repeats keep the modifiers as they are.
		if (e.repeat && held) {
			this.out.key(held.ks, 'repeat');
			return true;
		}
		this.apply(e, true);
		if (defer(e, ks)) return false;
		this.strike(e, ks);
		return true;
	}

	keyUp(e: KeyEvent) {
		this.release(e.code);
		// macOS swallows keyups of keys released while Cmd is held.
		if (this.isMac && (e.code === 'MetaLeft' || e.code === 'MetaRight'))
			for (const [code, k] of this.down) if (!k.mod) this.release(code);
		this.apply(e, false);
		// The chord is over: what the flags added goes.
		if (!modOf(e.code, e.key))
			for (const [m, ks] of this.inferred) {
				this.inferred.delete(m);
				this.out.key(ks, 'up');
			}
	}

	releaseAll() {
		this.down.clear();
		this.inferred.clear();
		this.out.releaseAll();
	}
}
