//! Screenshots and recordings, for agents and scripts: asked over the daemon's control
//! socket by `voxl screenshot` and `voxl record` (run over ssh, as `voxl attach` is).
//!
//! A recording is the VP9 stream voxl already encodes, rewrapped as MP4: the recorder
//! joins as one more viewer, whose way out is the recording itself (`Sender::recording`),
//! so it costs almost no CPU and bounds no real viewer. Like the stream, it holds frames
//! only when the screen changes, and it doesn't include the cursor (viewers draw it).
//! The desktop's audio is in it too, as the Opus packets the desktop sends, a track of the
//! MP4 beside the video; where the desktop is silent, or has no audio, the track has a
//! gap, or (with no sound at all) isn't there.

use crate::hub::{Hub, Out};
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

/// The frames and audio on `rx` until `until`, `max`, or a keyframe of another size than the
/// first's (the stream restarted at a new size): the MP4.
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
    let mut tracks = mp4::Tracks {
        frames: Vec::new(),
        audio: Vec::new(),
    };
    // The picture's size and level, read at the first frame, a keyframe: a keyframe of
    // another size (a restart) ends the recording before either could change.
    let mut size = (0, 0, mp4::Vp9 { level: 0 });
    // One message: taken into the recording, or the recording's end.
    let mut take = |m: Out| -> Taken {
        match m {
            Out::Bin(b) => {
                let (h, picture) = match Header::read(&b) {
                    Ok(Video::Picture(h, picture)) => (h, picture),
                    // Not a picture: the stream sends no empty frame.
                    Ok(Video::Empty(_)) | Err(crate::video::Short) => return Taken::On,
                };
                match (tracks.frames.is_empty(), crate::vp9::keyframe_size(picture)) {
                    // A recording starts on a keyframe, as every viewer does.
                    (true, None) => return Taken::On,
                    (true, Some((w, h))) => {
                        // A frame comes only from a built stream, which the hub holds.
                        let Some(stream) = *hub.stream.lock() else {
                            return Taken::On;
                        };
                        size = (
                            w,
                            h,
                            mp4::Vp9 {
                                level: stream.level(),
                            },
                        );
                    }
                    (false, Some(wh)) if wh != (size.0, size.1) => return Taken::End,
                    (false, _) => {}
                }
                // Time the frame was captured: now, less the pipeline time in its header.
                let ms = (start.elapsed().as_millis() as u64).saturating_sub(h.pipe.millis());
                let ms = tracks.frames.last().map_or(ms, |l| ms.max(l.ms + 1));
                tracks.frames.push(mp4::Frame {
                    ms,
                    key: h.key,
                    data: b.slice(Header::LEN..),
                });
                Taken::On
            }
            // A packet follows the one before by the silence the encoder skipped between
            // them, so the audio keeps its own clock; the first, by the time it came,
            // less its own length.
            Out::Audio { packet, silent } => {
                let ms = match tracks.audio.last() {
                    Some(l) => l.ms + mp4::AUDIO_MS * (1 + u64::from(silent)),
                    None => (start.elapsed().as_millis() as u64).saturating_sub(mp4::AUDIO_MS),
                };
                tracks.audio.push(mp4::AudioPacket { ms, data: packet });
                Taken::On
            }
            Out::Text(_) | Out::Attached => Taken::On,
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
    tokio::task::spawn_blocking(move || mp4::write(&tracks, size.0, size.1, size.2, end))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::ServerMessage;

    /// Video frame `id`: a keyframe of `w` x `h`, or a delta, and bytes standing for its
    /// VP9 (a keyframe's uncompressed header as far as its size, then `id`).
    fn video(id: u32, key: Option<(u16, u16)>) -> Out {
        let mut m = vec![u8::from(key.is_some())];
        m.extend_from_slice(&[0; 4]);
        match key {
            // Profile 0, a keyframe shown; the sync code; BT.601 at studio range; the size.
            Some((w, h)) => {
                m.extend_from_slice(&[0x82, 0x49, 0x83, 0x42]);
                let size = u64::from(w - 1) << 16 | u64::from(h - 1);
                // Four bits of colour, then 32 of size: 36 bits, in five bytes.
                m.extend_from_slice(&(size << 4).to_be_bytes()[3..]);
            }
            None => m.extend_from_slice(&[0x86, 0x00]),
        }
        m.extend_from_slice(&id.to_be_bytes());
        Out::Bin(bytes::Bytes::from(m))
    }

    /// A hub whose stream is built, as a recording's frames come only from one.
    fn hub() -> Arc<Hub> {
        let hub = Hub::new().unwrap();
        let shape = crate::vp9::Shape {
            width: 64,
            height: 64,
            fps: 60,
        };
        *hub.stream.lock() =
            Some(crate::vp9::Stream::new(shape, std::num::NonZeroUsize::MIN).unwrap());
        hub
    }

    /// The frames an MP4 holds: its sample count.
    pub(crate) fn samples(mp4: &[u8]) -> u32 {
        let stsz = mp4.windows(4).position(|w| w == b"stsz").unwrap();
        u32::from_be_bytes(mp4[stsz + 12..stsz + 16].try_into().unwrap())
    }

    /// A recording holds the frames from its first keyframe to the first keyframe of
    /// another size (a capture restart), whatever else a viewer is told (a hello, a
    /// clipboard that says anything, the microphone), and whatever keyframes of its own size
    /// come (other viewers starting).
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
        let hub = hub();
        let mut rng = crate::xorshift::Xorshift::new(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2_000 {
            let (tx, rx) = mpsc::unbounded_channel();
            let (mut held, mut ended) = (0, false);
            let mut first: Option<(u16, u16)> = None;
            for id in 0..rng.next() % 12 {
                let m = match rng.next() % 7 {
                    0 | 1 => video(id as u32, None),
                    2 => {
                        let size = [(64, 64), (64, 64), (128, 96)][rng.below(3) as usize];
                        match (first, ended) {
                            (None, _) => first = Some(size),
                            (Some(f), false) if f != size => ended = true,
                            (Some(_), _) => {}
                        }
                        video(id as u32, Some(size))
                    }
                    3 => Out::Text(hub.hello()),
                    4 => Out::Text(ServerMessage::Clip {
                        d: CLIPS[(rng.next() % CLIPS.len() as u64) as usize].into(),
                    }),
                    _ => Out::Text(ServerMessage::Mic {
                        on: rng.next() & 1 == 1,
                    }),
                };
                if let (Out::Bin(_), Some(_), false) = (&m, first, ended) {
                    held += 1;
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

    /// The desktop's audio is a track of its own beside the video, whatever order the
    /// messages come in: each packet beginning the time of the one before and the silence
    /// the encoder skipped between them, in the order sent, the audio the recording began
    /// before its first frame left out (a suffix of what was sent); and with no audio sent,
    /// no audio track. Over generated runs of 0 to 12 packets among 0 to 12 frames more,
    /// each after 0 to 3 frames of silence; the runs with and without audio and silence
    /// are counted.
    #[tokio::test]
    async fn a_recording_carries_the_audio_on_the_clock_its_packets_say() {
        let mut rng = crate::xorshift::Xorshift::new(0x510e_527f_ade6_82d1);
        let hub = hub();
        // Without audio; with audio and no silence; with silence.
        let mut reached = [0u32; 3];
        for _ in 0..1_000 {
            let (tx, rx) = mpsc::unbounded_channel();
            tx.send(video(0, Some((64, 64)))).unwrap();
            let mut sent: Vec<(Vec<u8>, u32)> = Vec::new();
            for id in 1..1 + rng.next() % 12 {
                tx.send(video(id as u32, None)).unwrap();
            }
            for _ in 0..rng.next() % 13 {
                let packet: Vec<u8> = (0..1 + rng.below(30))
                    .map(|_| rng.below(256) as u8)
                    .collect();
                let silent = rng.below(4) as u32;
                tx.send(Out::Audio {
                    packet: packet.clone().into(),
                    silent,
                })
                .unwrap();
                sent.push((packet, silent));
                if rng.next() & 1 == 1 {
                    tx.send(video(99, None)).unwrap();
                }
            }
            reached[match (sent.len(), sent.iter().any(|(_, s)| *s > 0)) {
                (0, _) => 0,
                (_, false) => 1,
                _ => 2,
            }] += 1;
            drop(tx);
            let (_keep, stop) = oneshot::channel();
            let until = Until::Stopped(stop);
            let mp4 = collect(&hub, rx, until, Duration::from_secs(3600))
                .await
                .unwrap();
            let (tracks, _) = mp4::tests::read(&mp4);
            let Some(audio) = tracks.get(1) else {
                // None kept: none sent, or all before the first frame.
                assert_eq!(tracks.len(), 1);
                continue;
            };
            assert_eq!(&audio.kind, b"soun");
            let kept = &sent[sent.len() - audio.samples.len()..];
            for (i, ((begin, data), (packet, silent))) in audio.samples.iter().zip(kept).enumerate()
            {
                assert_eq!(data, packet);
                if let Some((before, _)) = i.checked_sub(1).map(|j| audio.samples[j]) {
                    let ticks = mp4::AUDIO_MS * (1 + u64::from(*silent)) * audio.scale / 1000;
                    assert_eq!(begin - before, ticks);
                }
            }
        }
        assert!(reached.iter().all(|&n| n > 0), "cases reached: {reached:?}");
    }
}
