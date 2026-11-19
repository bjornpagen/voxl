// Presenting decoded frames and fitting the desktop to the window.
import type { App } from './app';

/**
 * Decoded frames go to a <video> through MediaStreamTrackGenerator: the compositor scans
 * YUV out directly. At most one write is in flight; a newer frame replaces one waiting
 * behind it.
 */
interface Track {
	writer: WritableStreamDefaultWriter<VideoFrame>;
	queue: Queue;
}

/** What waits behind the write in flight: no write in flight, a write with nothing behind
 * it, or a frame. */
type Queue =
	| { s: 'idle' }
	| { s: 'writing' }
	| { s: 'next'; frame: VideoFrame };

function write(t: Track, frame: VideoFrame) {
	t.queue = queued(t, frame);
}

/** The queue once `frame` has come. */
function queued(t: Track, frame: VideoFrame): Queue {
	switch (t.queue.s) {
		case 'next':
			t.queue.frame.close();
			return { s: 'next', frame };
		case 'writing':
			return { s: 'next', frame };
		case 'idle':
			t.writer
				.write(frame)
				.catch((e: unknown) => console.warn('[voxl] video track:', e))
				.finally(() => {
					const q = t.queue;
					t.queue = { s: 'idle' };
					if (q.s === 'next') write(t, q.frame);
				});
			return { s: 'writing' };
	}
}

export function createVideo(app: App, onLayout: () => void) {
	const gen = new MediaStreamTrackGenerator({ kind: 'video' });
	const video = document.createElement('video');
	video.className = 'stream';
	video.muted = true;
	video.autoplay = true;
	video.playsInline = true;
	video.disablePictureInPicture = true;
	video.srcObject = new MediaStream([gen]);
	app.surface.appendChild(video);
	video.play().catch((e: unknown) => console.warn('[voxl] video:', e));
	const track: Track = {
		writer: gen.writable.getWriter(),
		queue: { s: 'idle' },
	};

	function present(frame: VideoFrame) {
		write(track, frame);
	}

	function layout() {
		const desk = app.desk;
		if (desk.s === 'none') return;
		const { w: dw, h: dh } = desk;
		const dpr = window.devicePixelRatio;
		// The largest size that fits the window, snapped to whole device pixels.
		const fit = Math.min(window.innerWidth / dw, window.innerHeight / dh);
		const devH = Math.max(1, Math.floor(fit * dh * dpr));
		app.view.scale = devH / dpr / dh;
		app.surface.style.width = `${dw * app.view.scale}px`;
		app.surface.style.height = `${dh * app.view.scale}px`;
		onLayout();
	}

	return { present, layout };
}
