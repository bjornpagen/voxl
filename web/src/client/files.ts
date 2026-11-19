// Files panel: browse the shared folder and download (folders as zips), upload files
// and folders with the buttons or by dropping them anywhere on the page. Uses the
// relay's /files HTTP API.
import type { FilesConfig } from '../protocol';
import type { App } from './app';
import { el, input as inputOf, part } from './app';

interface Entry {
	name: string;
	dir: boolean;
	size: number;
	/** Modification time, ms since the epoch. */
	mtime: number;
}
interface Listing {
	path: string;
	entries: Entry[];
}
/** A file to upload, with its folder inside the upload ('' for a plain file). */
interface Picked {
	f: File;
	sub: string;
}
/** A dropped or picked folder: one progress row for all its files. */
interface Group {
	name: string;
	row: HTMLElement;
	files: number;
	bytes: number;
	done: number;
	sent: number;
	failed: number;
}
interface Upload {
	f: File;
	sub: string;
	dir: string;
	row: HTMLElement;
	/** Whether it is a file of its own, with a row of its own, or one of a folder's, whose
	 * row its group's is. */
	of: { readonly s: 'itself' } | { readonly s: 'group'; readonly group: Group };
}

// Icons from the Tembo UI package's icon set (16px with 2px padding).
const svg = (d: string) =>
	`<svg class="i" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${d}</svg>`;
const ICON_DIR = svg(
	'<path d="M3 6V17C3 18.1046 3.89543 19 5 19H19C20.1046 19 21 18.1046 21 17V9C21 7.89543 20.1046 7 19 7H12.5352C12.2008 7 11.8886 6.8329 11.7031 6.5547L10.5937 4.8906C10.2228 4.3342 9.59834 4 8.92963 4H5C3.89543 4 3 4.89543 3 6Z"/>'
);
const ICON_FILE = svg(
	'<path d="M12.1716 3H7C5.89543 3 5 3.89543 5 5V19C5 20.1046 5.89543 21 7 21H17C18.1046 21 19 20.1046 19 19V9.82843C19 9.29799 18.7893 8.78929 18.4142 8.41421L13.5858 3.58579C13.2107 3.21071 12.702 3 12.1716 3Z"/><path d="M13 3.5V7C13 8.10457 13.8954 9 15 9H18.5"/>'
);
const ICON_DOWNLOAD = svg(
	'<path d="M12 4V15"/><path d="M7 10.5L12 15.5L17 10.5"/><path d="M5 20H19"/>'
);
const ICON_CHEVRON = svg(
	'<path d="M10 16L13.2929 12.7071C13.6834 12.3166 13.6834 11.6834 13.2929 11.2929L10 8"/>'
);

const api = (op: string, q: Record<string, string>) =>
	`files/${op}?${new URLSearchParams(q)}`;
const label = (p: string) =>
	p === '' ? 'Home' : p.slice(p.lastIndexOf('/') + 1);
const join = (a: string, b: string) => (a ? a + '/' : '') + b;
const size = (n: number) =>
	n < 1024
		? `${n} B`
		: n < 1 << 20
			? `${(n / 1024).toFixed(1)} KB`
			: n < 1 << 30
				? `${(n / 1048576).toFixed(1)} MB`
				: `${(n / 1073741824).toFixed(2)} GB`;
function when(t: number) {
	const d = new Date(t),
		now = new Date();
	if (d.toDateString() === now.toDateString())
		return d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
	return d.toLocaleDateString([], {
		month: 'short',
		day: 'numeric',
		year: d.getFullYear() === now.getFullYear() ? undefined : 'numeric',
	});
}

/** A row says how its transfer goes. */
function status(row: HTMLElement, text: string) {
	part(row, '.f-meta').textContent = text;
}
/** A row says how its transfer ended: done, or in error. */
function ended(row: HTMLElement, text: string, how: 'done' | 'err') {
	status(row, text);
	row.classList.add(how);
}
/** What a drag carries, if it carries files: null for a drag of anything else, which is
 * not the panel's. */
function filesOf(e: DragEvent): DataTransfer | null {
	const carried = e.dataTransfer;
	return carried !== null && [...carried.types].includes('Files')
		? carried
		: null;
}

/** Every entry of a dropped folder: readEntries returns at most ~100 per call. */
async function readAll(
	reader: FileSystemDirectoryReader
): Promise<FileSystemEntry[]> {
	const batch = await new Promise<FileSystemEntry[]>((res, rej) =>
		reader.readEntries(res, rej)
	);
	return batch.length ? [...batch, ...(await readAll(reader))] : [];
}

/** All files under a dropped entry, with their folder paths relative to its parent. */
async function walk(entry: FileSystemEntry): Promise<Picked[]> {
	if (entry instanceof FileSystemFileEntry) {
		const f = await new Promise<File>((res, rej) => entry.file(res, rej));
		const sub = entry.fullPath
			.split('/')
			.filter(Boolean)
			.slice(0, -1)
			.join('/');
		return [{ f, sub }];
	}
	if (!(entry instanceof FileSystemDirectoryEntry)) return [];
	const children = await readAll(entry.createReader());
	return (await Promise.all(children.map(walk))).flat();
}

/** A request's answer: what was asked, or why not, in words for the panel. */
type Answer<T> = { s: 'ok'; value: T } | { s: 'refused'; why: string };

/** Where the panel is: nowhere before a hello names the folder to open, then the folder
 * shown. */
type At =
	| { readonly s: 'unnamed' }
	| { readonly s: 'named'; readonly path: string };

/** JSON that is an object: its fields, as they came. */
type Fields = Readonly<Record<string, unknown>>;
const isFields = (v: unknown): v is Fields =>
	typeof v === 'object' && v !== null;

/** What the computer answers with that the page cannot read. */
const unreadable = <T>(what: string): Answer<T> => ({
	s: 'refused',
	why: `The computer answered with ${what} the page cannot read.`,
});

/** A listing's entry, read; null for JSON that is none. */
function readEntry(json: unknown): Entry | null {
	if (!isFields(json)) return null;
	const { name, dir, size: bytes, mtime } = json;
	return typeof name === 'string' &&
		typeof dir === 'boolean' &&
		typeof bytes === 'number' &&
		typeof mtime === 'number'
		? { name, dir, size: bytes, mtime }
		: null;
}

/** A folder's listing, as `voxl files list` writes it. */
function readListing(json: unknown): Answer<Listing> {
	if (!isFields(json) || typeof json.path !== 'string')
		return unreadable('a listing');
	if (!Array.isArray(json.entries)) return unreadable('a listing');
	const entries: Entry[] = [];
	for (const one of json.entries) {
		const entry = readEntry(one);
		if (entry === null) return unreadable('a listing');
		entries.push(entry);
	}
	return { s: 'ok', value: { path: json.path, entries } };
}

/** Where an upload was saved, as `voxl files put` writes it. */
interface Saved {
	path: string;
}
function readSaved(json: unknown): Answer<Saved> {
	return isFields(json) && typeof json.path === 'string'
		? { s: 'ok', value: { path: json.path } }
		: unreadable("an upload's place");
}

/** A request's answer from its status and body: a 200's JSON as `read` reads it, or a
 * refusal's `error`. */
function answer<T>(
	code: number,
	body: string,
	read: (json: unknown) => Answer<T>
): Answer<T> {
	let json: unknown;
	try {
		json = JSON.parse(body);
	} catch {
		return {
			s: 'refused',
			why: `The computer answered ${code} without JSON.`,
		};
	}
	if (code === 200) return read(json);
	return {
		s: 'refused',
		why:
			isFields(json) && 'error' in json
				? String(json.error)
				: `Failed (${code})`,
	};
}

/** A fetch's answer; one that never reached the computer is refused too. */
async function ask<T>(
	request: () => Promise<Response>,
	read: (json: unknown) => Answer<T>
): Promise<Answer<T>> {
	let r: Response;
	try {
		r = await request();
	} catch {
		return {
			s: 'refused',
			why: 'The computer did not respond. Try again in a moment.',
		};
	}
	return answer(r.status, await r.text(), read);
}

/** The files a file input holds: it always holds a list, empty until something is picked. */
function chosen(input: HTMLInputElement): FileList {
	if (input.files === null)
		throw new Error(`voxl: #${input.id} is not a file input`);
	return input.files;
}

export function createFiles(app: App) {
	const panel = el('files');
	const list = el('files-list');
	const crumbs = el('files-crumbs');
	const uploads = el('files-uploads');
	const input = inputOf('files-input');
	const dirInput = inputOf('files-input-dir');
	const drop = el('drop');
	const dropDir = el('drop-dir');
	const uploadBtn = el('files-upload');
	const uploadDirBtn = el('files-upload-dir');

	/** The folder the panel is at, relative to the shared root ('' is the root): none
	 * before the first hello names the folder to open. The panel opens, and transfers go,
	 * only once there is one. */
	let at: At = { s: 'unnamed' };
	/** Does `act` at the folder the panel is at, if a hello has named one; before that
	 * there is nowhere to act. */
	function atFolder(act: (path: string) => void): At['s'] {
		switch (at.s) {
			case 'unnamed':
				return at.s;
			case 'named':
				act(at.path);
				return at.s;
		}
	}
	let seq = 0;
	let shown = '';

	function empty(title: string, text: string) {
		list.innerHTML = `<div class="f-empty">${ICON_DIR}<b></b><span></span></div>`;
		part(list, 'b').textContent = title;
		part(list, '.f-empty span').textContent = text;
	}

	/** The first hello names the folder to open; a later one leaves the panel where it
	 * is. */
	function setup(f: FilesConfig) {
		app.tools.classList.add('files');
		const path = at.s === 'named' ? at.path : f.dir;
		at = { s: 'named', path };
		dropDir.textContent = label(path);
	}
	function open() {
		// Opening lists the folder as it is now: files that appeared while closed (a
		// browser's downloads on the desktop) show then, or on Refresh.
		atFolder((path) => {
			panel.classList.add('open');
			load(path);
		});
	}
	/** Closing clears the uploads that finished; failed ones stay to be read. */
	function close() {
		panel.classList.remove('open');
		for (const row of uploads.querySelectorAll('.f-up.done')) row.remove();
	}

	/** Opens `path`: lists it, or says why it cannot be. */
	const load = (path: string) =>
		show(path, (why) => {
			shown = '';
			empty('Can’t open this folder', why);
		});
	/** Lists `path` again, as it is now; one that cannot be listed stays as it is shown. */
	const refresh = (path: string) => show(path, () => {});
	/** Lists `path`, the newest asking's answer the one shown: `refused` is told why not. */
	async function show(path: string, refused: (why: string) => void) {
		const n = ++seq;
		const a = await ask(
			() => fetch(api('list', { path }), { cache: 'no-store' }),
			readListing
		);
		if (n !== seq) return;
		if (a.s === 'refused') return refused(a.why);
		const d = a.value;
		at = { s: 'named', path: d.path };
		dropDir.textContent = label(d.path);
		const key = d.path + '\n' + JSON.stringify(d.entries);
		if (key !== shown) {
			shown = key;
			render(d.path, d.entries);
		}
	}

	function render(folder: string, entries: Entry[]) {
		crumbs.innerHTML = '';
		const parts = folder ? folder.split('/') : [];
		const crumb = (text: string, path: string) => {
			const a = document.createElement('a');
			a.textContent = text;
			a.addEventListener('click', () => load(path));
			crumbs.append(a);
		};
		crumb('Home', '');
		parts.forEach((p, i) => {
			crumbs.insertAdjacentHTML('beforeend', ICON_CHEVRON);
			crumb(p, parts.slice(0, i + 1).join('/'));
		});
		const top = list.scrollTop;
		list.innerHTML = '';
		// Folders by name, then files newest first (downloads land at the top).
		entries.sort((a, b) => {
			if (a.dir !== b.dir) return a.dir ? -1 : 1;
			if (!a.dir && a.mtime !== b.mtime) return b.mtime - a.mtime;
			return a.name.localeCompare(b.name);
		});
		if (!entries.length)
			empty(
				'No files yet',
				'Drop files anywhere on the screen, or use Upload, to add them here.'
			);
		for (const en of entries) {
			const row = document.createElement('div');
			row.className = 'f-row' + (en.dir ? ' dir' : '');
			row.innerHTML = `<span class="f-ico">${en.dir ? ICON_DIR : ICON_FILE}</span><span class="f-name"></span><span class="f-meta"></span>`;
			if (en.dir) {
				const b = document.createElement('button');
				b.className = 'ib sm f-dl';
				b.type = 'button';
				b.title = `Download ${en.name} as a zip`;
				b.setAttribute('aria-label', b.title);
				b.innerHTML = ICON_DOWNLOAD;
				b.addEventListener('click', (e) => {
					e.stopPropagation();
					download(join(folder, en.name));
				});
				part(row, '.f-meta').before(b);
			}
			part(row, '.f-name').textContent = en.name;
			part(row, '.f-meta').textContent = en.dir
				? when(en.mtime)
				: `${size(en.size)} · ${when(en.mtime)}`;
			row.title = en.dir ? `Open ${en.name}` : `Download ${en.name}`;
			const path = join(folder, en.name);
			row.addEventListener('click', () =>
				en.dir ? load(path) : download(path)
			);
			list.append(row);
		}
		list.scrollTop = top;
	}

	function download(path: string) {
		const a = document.createElement('a');
		a.href = api('download', { path });
		a.download = '';
		document.body.append(a);
		a.click();
		a.remove();
	}

	// Uploads run one at a time, each to the folder that was open when it was added.
	// A folder gets one row for all of its files.
	const queue: Upload[] = [];
	let busy = false;
	function upload(picked: Picked[]) {
		atFolder((dir) => uploadTo(dir, picked));
	}
	function uploadTo(dir: string, picked: Picked[]) {
		open();
		const groups = new Map<string, Group>();
		/** The group of the folder `top`, made at its first file. */
		const groupOf = (top: string): Group => {
			const made = groups.get(top);
			if (made !== undefined) return made;
			const group = {
				name: top,
				row: newRow(top),
				files: 0,
				bytes: 0,
				done: 0,
				sent: 0,
				failed: 0,
			};
			groups.set(top, group);
			return group;
		};
		for (const { f, sub } of picked) {
			const top = sub.split('/')[0];
			if (top === '') {
				queue.push({
					f,
					sub,
					dir,
					row: newRow(f.name),
					of: { s: 'itself' },
				});
				continue;
			}
			const group = groupOf(top);
			group.files++;
			group.bytes += f.size;
			queue.push({
				f,
				sub,
				dir,
				row: group.row,
				of: { s: 'group', group },
			});
		}
		for (const g of groups.values()) groupStatus(g, 0);
		pump();
	}
	function newRow(name: string) {
		const row = document.createElement('div');
		row.className = 'f-up';
		row.innerHTML =
			'<div class="f-up-top"><span class="f-name"></span><span class="f-meta">Waiting…</span></div><div class="f-bar"><i></i></div>';
		part(row, '.f-name').textContent = name;
		uploads.prepend(row);
		return row;
	}
	/** A group's row, with `current` bytes of the file now going sent. */
	function groupStatus(g: Group, current: number) {
		const pct = g.bytes ? ((g.sent + current) / g.bytes) * 100 : 100;
		part(g.row, '.f-bar i').style.width = `${pct}%`;
		const n = `${g.done} of ${g.files} file${g.files === 1 ? '' : 's'}`;
		status(g.row, `${n} · ${Math.floor(pct)}% of ${size(g.bytes)}`);
	}
	function groupFinished(g: Group, dir: string) {
		if (g.done + g.failed < g.files) return;
		if (g.failed) ended(g.row, `${g.failed} of ${g.files} files failed`, 'err');
		else ended(g.row, `Uploaded to ${label(dir)}`, 'done');
	}
	function pump() {
		if (busy) return;
		const u = queue.shift();
		// An empty queue: nothing waits.
		if (u === undefined) return;
		busy = true;
		const bar = part(u.row, '.f-bar i');
		const xhr = new XMLHttpRequest();
		const finish = () => {
			busy = false;
			pump();
		};
		const q: Record<string, string> = { path: u.dir, name: u.f.name };
		if (u.sub) q.sub = u.sub;
		xhr.open('PUT', api('upload', q));
		// Required by the server: browsers only send custom headers cross-origin after a
		// CORS preflight, which it never answers, so other sites can't upload.
		xhr.setRequestHeader('X-Voxl-Upload', '1');
		xhr.upload.addEventListener('progress', (e) => {
			if (e.lengthComputable) progressed(u, bar, e.loaded, e.total);
		});
		xhr.addEventListener('load', () => {
			saved(u, answer(xhr.status, xhr.responseText, readSaved));
			finish();
		});
		xhr.addEventListener('error', () => {
			lost(u);
			finish();
		});
		xhr.send(u.f);
	}
	/** `loaded` of `total` bytes of an upload are sent. */
	function progressed(
		u: Upload,
		bar: HTMLElement,
		loaded: number,
		total: number
	): 'shown' {
		switch (u.of.s) {
			case 'group':
				groupStatus(u.of.group, loaded);
				return 'shown';
			case 'itself':
				bar.style.width = `${(loaded / total) * 100}%`;
				status(
					u.row,
					`${Math.floor((loaded / total) * 100)}% of ${size(total)}`
				);
				return 'shown';
		}
	}
	/** An upload was answered: saved, or refused. */
	function saved(u: Upload, a: Answer<Saved>): 'shown' {
		if (a.s === 'ok' && at.s === 'named' && u.dir === at.path) refresh(u.dir);
		switch (u.of.s) {
			case 'group': {
				const g = u.of.group;
				if (a.s === 'ok') {
					g.done++;
					g.sent += u.f.size;
				} else g.failed++;
				groupStatus(g, 0);
				groupFinished(g, u.dir);
				return 'shown';
			}
			case 'itself':
				if (a.s === 'ok') {
					const name = label(a.value.path);
					if (name !== u.f.name) part(u.row, '.f-name').textContent = name;
					ended(u.row, `Uploaded to ${label(u.dir)}`, 'done');
				} else ended(u.row, a.why, 'err');
				return 'shown';
		}
	}
	/** An upload's connection was lost. */
	function lost(u: Upload): 'shown' {
		switch (u.of.s) {
			case 'group': {
				const g = u.of.group;
				g.failed++;
				groupFinished(g, u.dir);
				return 'shown';
			}
			case 'itself':
				ended(u.row, 'Connection lost', 'err');
				return 'shown';
		}
	}

	el('files-btn').addEventListener('click', () =>
		panel.classList.contains('open') ? close() : open()
	);
	el('files-close').addEventListener('click', close);
	el('files-refresh').addEventListener('click', () => atFolder(load));
	uploadBtn.addEventListener('click', () => input.click());
	uploadDirBtn.addEventListener('click', () => dirInput.click());
	input.addEventListener('change', () => {
		upload([...chosen(input)].map((f) => ({ f, sub: '' })));
		input.value = '';
	});
	dirInput.addEventListener('change', () => {
		// webkitRelativePath is "Folder/sub/file.txt".
		upload(
			[...chosen(dirInput)].map((f) => ({
				f,
				sub: f.webkitRelativePath.split('/').slice(0, -1).join('/'),
			}))
		);
		dirInput.value = '';
	});
	// Esc closes the panel unless the remote screen has focus (it goes to the desktop then).
	document.addEventListener('keydown', (e) => {
		if (e.key === 'Escape' && e.target !== app.kbd && e.target !== app.surface)
			close();
	});

	// Always swallow dropped files: otherwise the browser navigates away to open them.
	let depth = 0;
	window.addEventListener('dragenter', (e) => {
		if (filesOf(e) === null) return;
		e.preventDefault();
		if (at.s === 'named' && ++depth === 1) drop.classList.add('show');
	});
	window.addEventListener('dragover', (e) => {
		const carried = filesOf(e);
		if (carried === null) return;
		e.preventDefault();
		carried.dropEffect = at.s === 'named' ? 'copy' : 'none';
	});
	window.addEventListener('dragleave', (e) => {
		if (filesOf(e) !== null && --depth <= 0) {
			depth = 0;
			drop.classList.remove('show');
		}
	});
	window.addEventListener('drop', (e) => {
		const carried = filesOf(e);
		if (carried === null) return;
		e.preventDefault();
		depth = 0;
		drop.classList.remove('show');
		if (at.s === 'named') dropped(carried);
	});
	/** Files and folders were dropped on the page. */
	function dropped(carried: DataTransfer) {
		// Entries must be taken during the event; the walk can finish afterwards.
		const entries: FileSystemEntry[] = [];
		const plain: Picked[] = [];
		for (const it of carried.items) {
			if (it.kind !== 'file') continue;
			const entry = it.webkitGetAsEntry();
			if (entry !== null && entry.isDirectory) entries.push(entry);
			else {
				const f = it.getAsFile();
				if (f !== null) plain.push({ f, sub: '' });
			}
		}
		if (!entries.length) return upload(plain);
		Promise.all(entries.map(walk)).then(
			(found) => upload([...plain, ...found.flat()]),
			(err: unknown) => {
				open();
				ended(newRow('Dropped folder'), `Can’t read it: ${err}`, 'err');
			}
		);
	}

	return { setup };
}
