// What the page's modules share: the worker, the main elements and the desktop geometry.
import type { ToWorker } from '../protocol';

export interface App {
	worker: Worker;
	/** Send a text command (see `cmd` in protocol.ts). */
	send(command: string): void;
	/** Send a binary message: `type` byte, then `bytes`. */
	sendBin(type: number, bytes: Uint8Array): void;
	/** Posts the worker a message, handing over `transfer`. */
	post(m: ToWorker, transfer: Transferable[]): void;
	surface: HTMLElement;
	/** Hidden textarea that receives keyboard, IME and paste input. */
	kbd: HTMLTextAreaElement;
	tools: HTMLElement;
	isMac: boolean;
	inIframe: boolean;
	/** The desktop as the pictures decoded show it. */
	desk: Desk;
	/** On-screen CSS pixels per desktop pixel, before any pinch zoom (zoom.ts). */
	view: { scale: number };
}

/** The desktop: none before a session's first picture and after the session ends;
 * otherwise its size in pixels, the size of the pictures decoded. */
export type Desk =
	| { readonly s: 'none' }
	| {
			readonly s: 'shown';
			readonly w: number;
			readonly h: number;
	  };

/** The element of this id, which index.html has. */
export function el(id: string): HTMLElement {
	const e = document.getElementById(id);
	if (e === null) throw new Error(`voxl: #${id} is missing from index.html`);
	return e;
}

/** The input, the text area or the button of this id, which index.html has. */
export function input(id: string): HTMLInputElement {
	const e = el(id);
	if (!(e instanceof HTMLInputElement))
		throw new Error(`voxl: #${id} is not an input in index.html`);
	return e;
}
export function textarea(id: string): HTMLTextAreaElement {
	const e = el(id);
	if (!(e instanceof HTMLTextAreaElement))
		throw new Error(`voxl: #${id} is not a text area in index.html`);
	return e;
}
export function button(id: string): HTMLButtonElement {
	const e = el(id);
	if (!(e instanceof HTMLButtonElement))
		throw new Error(`voxl: #${id} is not a button in index.html`);
	return e;
}

/** The one element `selector` names under `under`, in markup the page itself wrote. */
export function part(under: ParentNode, selector: string): HTMLElement {
	const e = under.querySelector(selector);
	if (!(e instanceof HTMLElement))
		throw new Error(`voxl: no ${selector} where the page wrote one`);
	return e;
}

/** A canvas's 2D context, which a canvas that was given no other has. */
export function context2d(canvas: HTMLCanvasElement): CanvasRenderingContext2D {
	const context = canvas.getContext('2d');
	if (context === null) throw new Error('voxl: a canvas without a 2D context');
	return context;
}
