//! Screenshots and recordings, for agents and scripts: asked over the daemon's control
//! socket by `voxl screenshot` and `voxl record` (run over ssh, as `voxl attach` is).
//!
//! A recording is the H.264 stream voxl already encodes, rewrapped as MP4: the recorder
//! joins as one more viewer, whose way out is the recording itself (`Sender::recording`),
//! so it costs almost no CPU and bounds no real viewer. Like the stream, it holds frames
//! only when the screen changes, and it doesn't include the cursor (viewers draw it).

use crate::hub::{Hub, Out, ServerMessage};
use crate::mp4;
use crate::video::{Header, Video};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// The screen as captured (BGRX, 4 bytes a pixel, rows packed), for a screenshot.
pub struct Snapshot {
    pub width: usize,
    pub height: usize,
    pub bgrx: Vec<u8>,
}

impl Snapshot {
    /// RGB (8 bits per channel), exactly as on screen.
    fn to_rgb(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.width * self.height * 3];
        let (rgb, bgrx) = (out.as_chunks_mut::<3>().0, self.bgrx.as_chunks::<4>().0);
        for (o, [b, g, r, _]) in rgb.iter_mut().zip(bgrx) {
            *o = [*r, *g, *b];
        }
        out
    }

    /// As a PNG.
    pub fn png(&self) -> Result<Vec<u8>, String> {
        let (w, h) = (self.width as u32, self.height as u32);
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()
            .and_then(|mut w| w.write_image_data(&self.to_rgb()))
            .map_err(|e| e.to_string())?;
        Ok(out)
    }
}

/// The screen now, as a PNG: the capture thread copies it at its next look, and the PNG
/// is made off that thread.
pub async fn screenshot(hub: &Hub) -> Result<Vec<u8>, String> {
    let (tx, rx) = oneshot::channel();
    hub.snapshots.lock().push(tx);
    hub.wake();
    let shot = rx.await.map_err(|_| "the capture has ended".to_string())?;
    tokio::task::spawn_blocking(move || shot.png())
        .await
        .map_err(|e| e.to_string())?
}

/// Whether a recording goes on after a message.
enum Taken {
    On,
    End,
}

/// What ends a recording, besides `--max-record-seconds` and a restart of the stream.
pub enum Until {
    /// So long after it starts.
    Elapsed(Duration),
    /// When this resolves: the asker's stream closed.
    Stopped(oneshot::Receiver<()>),
}

/// Records until `until`, `max` at the most: the MP4.
pub async fn record(hub: &Arc<Hub>, until: Until, max: Duration) -> Result<Vec<u8>, String> {
    let (tx, rx) = mpsc::unbounded_channel();
    let client = hub.add_recorder(tx);
    let res = collect(hub, rx, until, max).await;
    hub.remove_client(client.id);
    res
}

/// The frames on `rx` until `until`, `max`, or (once there is a frame) a hello (the
/// stream restarted): the MP4.
async fn collect(
    hub: &Hub,
    mut rx: mpsc::UnboundedReceiver<Out>,
    until: Until,
    max: Duration,
) -> Result<Vec<u8>, String> {
    let start = Instant::now();
    let (length, mut stop) = match until {
        Until::Elapsed(d) => (d.min(max), None),
        Until::Stopped(stop) => (max, Some(stop)),
    };
    let deadline = tokio::time::sleep(length);
    tokio::pin!(deadline);
    let stopped = async {
        match &mut stop {
            Some(stop) => {
                // Sent or dropped, the asker is done.
                if stop.await.is_err() {
                    log::debug!("a recording's asker went without its stop");
                }
            }
            None => std::future::pending().await,
        }
    };
    tokio::pin!(stopped);
    let mut frames: Vec<mp4::Frame> = Vec::new();
    // The picture's size, read at the first frame: a restart (a new size) ends the
    // recording before it could change.
    let mut size = (0, 0);
    // One message: taken into the recording, or the recording's end.
    let mut take = |m: Out| -> Taken {
        match m {
            Out::Bin(b) => {
                let h = match Header::read(&b) {
                    Ok(Video::Picture(h, _)) => h,
                    // Not a picture: the stream sends no empty frame.
                    Ok(Video::Empty(_)) | Err(crate::video::Short) => return Taken::On,
                };
                if frames.is_empty() {
                    use std::sync::atomic::Ordering::Relaxed;
                    size = (hub.width.load(Relaxed), hub.height.load(Relaxed));
                }
                // Time the frame was captured: now, less the pipeline time in its header.
                let ms = (start.elapsed().as_millis() as u64).saturating_sub(h.pipe.millis());
                let ms = frames.last().map_or(ms, |l: &mp4::Frame| ms.max(l.ms + 1));
                frames.push(mp4::Frame {
                    ms,
                    key: h.key,
                    data: b.slice(Header::LEN..),
                });
                Taken::On
            }
            // A restart of the stream ends this recording.
            Out::Text(ServerMessage::Hello(_)) if !frames.is_empty() => Taken::End,
            Out::Text(_) | Out::Attached { .. } | Out::Lanes(_) => Taken::On,
        }
    };
    let asked_to_end = loop {
        tokio::select! {
            _ = &mut stopped => break true,
            _ = &mut deadline => break true,
            m = rx.recv() => match m.map(&mut take) {
                Some(Taken::On) => {}
                None | Some(Taken::End) => break false,
            },
        }
    };
    // Asked to end (stopped, or its time up): what the stream sent before that is in the
    // recording, though the end was seen first.
    if asked_to_end {
        while let Ok(m) = rx.try_recv() {
            if let Taken::End = take(m) {
                break;
            }
        }
    }
    let end = start.elapsed().as_millis() as u64;
    tokio::task::spawn_blocking(move || mp4::write(&frames, size.0, size.1, end))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Video frame `id`: a keyframe, its SPS and PPS as mp4.rs's own test writes them.
    fn video(id: u32) -> Out {
        let mut m = vec![1];
        m.extend_from_slice(&id.to_be_bytes());
        m.extend_from_slice(&[0; 4]);
        m.extend_from_slice(&[
            0, 0, 0, 1, 0x67, 100, 0, 40, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65, 0xaa,
        ]);
        Out::Bin(bytes::Bytes::from(m))
    }

    /// The frames an MP4 holds: its sample count.
    pub(crate) fn samples(mp4: &[u8]) -> u32 {
        let stsz = mp4.windows(4).position(|w| w == b"stsz").unwrap();
        u32::from_be_bytes(mp4[stsz + 12..stsz + 16].try_into().unwrap())
    }

    /// A recording holds the frames that came before the first hello (a capture restart)
    /// after its first frame, whatever else a viewer is told, and whatever the desktop's
    /// clipboard says.
    #[tokio::test]
    async fn a_recording_ends_at_a_restart_and_nothing_else() {
        const CLIPS: [&str; 7] = [
            "hello",
            "close",
            "\"hello\"",
            "say \"close",
            "hello there",
            "",
            "x",
        ];
        let hub = Hub::new().unwrap();
        let mut rng = crate::xorshift::Xorshift::new(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2_000 {
            let (tx, rx) = mpsc::unbounded_channel();
            let (mut held, mut ended) = (0, false);
            for id in 0..rng.next() % 12 {
                let m = match rng.next() % 6 {
                    0..=2 => video(id as u32),
                    3 => Out::Text(hub.hello()),
                    4 => Out::Text(ServerMessage::Clip {
                        d: CLIPS[(rng.next() % CLIPS.len() as u64) as usize].into(),
                    }),
                    _ => Out::Text(ServerMessage::Mic {
                        on: rng.next() & 1 == 1,
                    }),
                };
                match &m {
                    Out::Bin(_) if !ended => held += 1,
                    Out::Text(ServerMessage::Hello(_)) if held > 0 => ended = true,
                    _ => {}
                }
                tx.send(m).unwrap();
            }
            drop(tx);
            let (_keep, stop) = oneshot::channel();
            let until = Until::Stopped(stop);
            let mp4 = collect(&hub, rx, until, Duration::from_secs(3600)).await;
            match held {
                0 => assert!(mp4.is_err()),
                n => assert_eq!(samples(&mp4.unwrap()), n),
            }
        }
    }
}
