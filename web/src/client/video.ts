// Presenting decoded frames and fitting the desktop to the window.
import type { App } from './app';

/**
 * Decoded frames go to a <video> through a MediaStreamTrackGenerator: the compositor scans
 * YUV out directly. The page makes the track and its element; the track's writable stream
 * is handed to the worker (`track`, in `init`), which writes each picture as it is decoded
 * (track.ts), so no picture crosses this thread.
 */
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

	return { track: gen.writable, layout };
}
