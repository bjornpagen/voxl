// What the page's modules share: the worker, the main elements and the desktop geometry.
import type { ToWorker } from '../protocol';

export interface App {
	worker: Worker;
	/** Send a text command (see `cmd` in protocol.ts). */
	send(command: string): void;
	/** Send a binary message: `type` byte, then `bytes`. */
	sendBin(type: number, bytes: Uint8Array): void;
	post(m: ToWorker, transfer?: Transferable[]): void;
	surface: HTMLElement;
	/** Hidden textarea that receives keyboard, IME and paste input. */
	kbd: HTMLTextAreaElement;
	tools: HTMLElement;
	isMac: boolean;
	inIframe: boolean;
	/** The desktop as the session's hello describes it. */
	desk: Desk;
	/** On-screen CSS pixels per desktop pixel, before any pinch zoom (zoom.ts). */
	view: { scale: number };
}

/** The desktop: none before a session's hello and after the session ends; otherwise its
 * size in pixels. */
export type Desk =
	| { readonly s: 'none' }
	| {
			readonly s: 'shown';
			readonly w: number;
			readonly h: number;
	  };

export function el<T extends HTMLElement = HTMLElement>(id: string): T {
	const e = document.getElementById(id);
	if (!e) throw new Error(`voxl: #${id} is missing from index.html`);
	return e as T;
}
