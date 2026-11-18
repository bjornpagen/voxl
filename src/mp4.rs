//! Minimal MP4 writer for recordings: the VP9 voxl already sends, rewrapped without
//! re-encoding (VP9 in MP4 as the VP Codec ISO Media File Format Binding,
//! https://www.webmproject.org/vp9/mp4/, places it: a `vp09` sample entry with a `vpcC`, each
//! frame one sample), and the desktop's Opus packets as a second track. Frames arrive only when
//! the screen changes, so each sample lasts until the next one (variable frame rate);
//! packets are sent only for sound, so the audio track has gaps, which its sample
//! durations hold.

use crate::sound::{DESKTOP, RATE};

/// One encoded frame: time since the recording started (ms), keyframe, the VP9 frame.
pub struct Frame {
    pub ms: u64,
    pub key: bool,
    pub data: bytes::Bytes,
}

/// One Opus packet of the desktop's audio: when its frame begins (ms since the recording
/// started, the zero of `Frame::ms`), and the packet.
pub struct AudioPacket {
    pub ms: u64,
    pub data: bytes::Bytes,
}

/// What a recording holds, on one timeline (ms since it started): its frames, and the
/// audio packets in the order they were sent, each beginning at least `AUDIO_MS` after
/// the one before. No audio is a recording without an audio track.
pub struct Tracks {
    pub frames: Vec<Frame>,
    pub audio: Vec<AudioPacket>,
}

/// The time an Opus packet of the desktop's audio lasts, in ms: a whole number, so a
/// packet's place in the audio track is a whole number of ms too.
pub const AUDIO_MS: u64 = DESKTOP.frame as u64 * 1000 / RATE as u64;
const _: () = assert!(DESKTOP.frame * 1000 == AUDIO_MS as usize * RATE as usize);

/// The audio track's time unit, in ticks a second: the sample rate, as Opus in MP4 says.
const AUDIO_SCALE: u32 = RATE;

/// Opus's pre-skip, in samples at `RATE`: the encoder's look-ahead, which its
/// low-delay application (voxl's) sets at 2.5 ms (opus_encoder.c: `Fs/400`).
const PRE_SKIP: u16 = (RATE / 400) as u16;

/// The movie's time unit, in ticks a second: milliseconds, the unit each frame's time is
/// kept in (`Frame::ms`).
const TIMESCALE: u32 = 1000;

fn bx(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(body);
    v
}

/// A full box: version 0 and `flags`, then the body.
fn fbx(name: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = flags.to_be_bytes().to_vec();
    b.extend_from_slice(body);
    bx(name, &b)
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// Identity transformation matrix of movie and track headers.
const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

/// The track box of the audio `packets`, which begin `lead_ms` after the movie does and
/// last `ms` in all, its chunk `at` in the file: each sample lasts until the next packet
/// begins, the last a packet's time, so a silence is a sample that lasts through it.
fn audio_trak(packets: &[&AudioPacket], lead_ms: u64, ms: u64, at: u32) -> Vec<u8> {
    let ticks = |ms: u64| (ms * u64::from(AUDIO_SCALE) / 1000) as u32;
    let n = packets.len() as u32;
    let durations: Vec<u32> = (0..packets.len())
        .map(|i| {
            let next = packets
                .get(i + 1)
                .map_or(packets[i].ms + AUDIO_MS, |p| p.ms);
            ticks(next - packets[i].ms)
        })
        .collect();
    let mut stts: Vec<(u32, u32)> = Vec::new();
    for &d in &durations {
        match stts.last_mut() {
            Some((c, last)) if *last == d => *c += 1,
            _ => stts.push((1, d)),
        }
    }
    let stts = fbx(
        b"stts",
        0,
        &[
            u32s(&[stts.len() as u32]),
            stts.iter().flat_map(|&(c, d)| u32s(&[c, d])).collect(),
        ]
        .concat(),
    );
    let sizes: Vec<u32> = packets.iter().map(|p| p.data.len() as u32).collect();
    let stsz = fbx(b"stsz", 0, &[u32s(&[0, n]), u32s(&sizes)].concat());
    let stsc = fbx(b"stsc", 0, &u32s(&[1, 1, n, 1]));
    let stco = fbx(b"stco", 0, &u32s(&[1, at]));

    // Opus in MP4: stereo, 16-bit, `RATE`; its `dOps` says the channels, pre-skip and rate.
    let mut dops = vec![0, DESKTOP.channels];
    dops.extend_from_slice(&PRE_SKIP.to_be_bytes());
    dops.extend_from_slice(&RATE.to_be_bytes());
    dops.extend_from_slice(&0i16.to_be_bytes()); // output gain
    dops.push(0); // channel mapping family 0: mono or stereo
    let mut opus = vec![0u8; 6];
    opus.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    opus.extend_from_slice(&[0u8; 8]);
    opus.extend_from_slice(&u16::from(DESKTOP.channels).to_be_bytes());
    opus.extend_from_slice(&16u16.to_be_bytes()); // bits a sample
    opus.extend_from_slice(&[0u8; 4]);
    opus.extend_from_slice(&(AUDIO_SCALE << 16).to_be_bytes());
    opus.extend_from_slice(&bx(b"dOps", &dops));
    let stsd = fbx(b"stsd", 0, &[u32s(&[1]), bx(b"Opus", &opus)].concat());
    let stbl = bx(b"stbl", &[stsd, stts, stsz, stsc, stco].concat());

    let dinf = bx(
        b"dinf",
        &fbx(b"dref", 0, &[u32s(&[1]), fbx(b"url ", 1, &[])].concat()),
    );
    let minf = bx(b"minf", &[fbx(b"smhd", 0, &[0u8; 4]), dinf, stbl].concat());
    let media: u64 = durations.iter().map(|&d| u64::from(d)).sum();
    let mut hdlr = u32s(&[0]);
    hdlr.extend_from_slice(b"soun");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"voxl\0");
    let mut mdhd = u32s(&[0, 0, AUDIO_SCALE, media as u32]);
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und"
    let mdia = bx(
        b"mdia",
        &[fbx(b"mdhd", 0, &mdhd), fbx(b"hdlr", 0, &hdlr), minf].concat(),
    );

    let mut tkhd = u32s(&[0, 0, 2, 0, (lead_ms + ms) as u32, 0, 0]);
    tkhd.extend_from_slice(&[0u8; 4]); // layer, alternate group
    tkhd.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0, reserved
    tkhd.extend_from_slice(&u32s(&MATRIX));
    tkhd.extend_from_slice(&u32s(&[0, 0])); // no picture
                                            // The track's place in the movie: empty until its first packet, then the packets.
    let mut edits = Vec::new();
    if lead_ms > 0 {
        edits.push((lead_ms as u32, -1i32));
    }
    edits.push((ms as u32, 0));
    let mut elst = u32s(&[edits.len() as u32]);
    for (duration, media_time) in edits {
        elst.extend_from_slice(&duration.to_be_bytes());
        elst.extend_from_slice(&media_time.to_be_bytes());
        elst.extend_from_slice(&[0, 1, 0, 0]); // rate 1.0
    }
    let edts = bx(b"edts", &fbx(b"elst", 0, &elst));
    bx(b"trak", &[fbx(b"tkhd", 3, &tkhd), edts, mdia].concat())
}

/// What the `vpcC` of a VP9 stream says: the level it is coded at, times ten (profile 0 and
/// 8 bits are voxl's one stream's).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vp9 {
    pub level: u8,
}

/// The VP Codec Configuration Box of a stream at `level` (the binding's §2.2.3): version 1;
/// profile 0; the level; 8 bits, 4:2:0 with chroma between luma rows (code 0), studio range;
/// BT.709 primaries (code 1) and the sRGB transfer (code 13), as X11 draws the screen, and
/// BT.601's matrix (code 6), as libyuv's ARGBToI420 converts it (ISO/IEC 23091-2's codes);
/// no initialisation data, as VP9 has none.
fn vpcc(Vp9 { level }: Vp9) -> Vec<u8> {
    let mut b = vec![1, 0, 0, 0]; // version 1, flags 0
    b.extend_from_slice(&[0, level, 8 << 4, 1, 13, 6]);
    b.extend_from_slice(&0u16.to_be_bytes());
    bx(b"vpcC", &b)
}

/// Writes `tracks` (the first frame must be a keyframe) of a VP9 stream `codec` as an MP4 of
/// `width`x`height` lasting until `end_ms`. The movie begins at its first frame: audio
/// before it is left out, and the audio after it keeps its place, silences and all.
pub fn write(
    tracks: &Tracks,
    width: u32,
    height: u32,
    codec: Vp9,
    end_ms: u64,
) -> Result<Vec<u8>, String> {
    let frames = &tracks.frames;
    let first = frames.first().ok_or("no frames were recorded")?;
    if !first.key {
        return Err("recording doesn't start with a keyframe".into());
    }

    // Samples: each frame as it is, copied once, straight into the output, at the end.
    let (mut sizes, mut durations, mut sync) = (Vec::new(), Vec::new(), Vec::new());
    for (i, f) in frames.iter().enumerate() {
        sizes.push(f.data.len() as u32);
        // A frame lasts until the next, and the last until the recording's end.
        let next = match frames.get(i + 1) {
            Some(n) => n.ms,
            None => end_ms,
        };
        durations.push(next.saturating_sub(f.ms).max(1) as u32);
        if f.key {
            sync.push(i as u32 + 1);
        }
    }
    let total: u64 = durations.iter().map(|&d| d as u64).sum();
    let n = frames.len() as u32;
    let video_len: u64 = sizes.iter().map(|&s| s as u64).sum();

    // The audio from the first frame on, in order and apart by at least a packet.
    let audio: Vec<&AudioPacket> = tracks.audio.iter().filter(|p| p.ms >= first.ms).collect();
    if audio.windows(2).any(|w| w[1].ms < w[0].ms + AUDIO_MS) {
        return Err("audio packets overlap".into());
    }
    let audio_len: u64 = audio.iter().map(|p| p.data.len() as u64).sum();
    // Where the audio begins and how long it lasts, in the movie's ms, as the video's
    // frames are (their zero is the first frame).
    let audio_span = audio.first().zip(audio.last()).map(|(f, l)| {
        let lead = f.ms - first.ms;
        (lead, l.ms + AUDIO_MS - f.ms)
    });
    let movie = total.max(audio_span.map_or(0, |(lead, ms)| lead + ms));
    let mdat_len = video_len + audio_len;

    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2mp41");
    if mdat_len + 8 > u32::MAX as u64 {
        return Err("recording too large".into());
    }

    // The visual sample entry (ISO/IEC 14496-12 §12.1.3), then its `vpcC`.
    let mut vp09 = vec![0u8; 6];
    vp09.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    vp09.extend_from_slice(&[0u8; 16]);
    vp09.extend_from_slice(&(width as u16).to_be_bytes());
    vp09.extend_from_slice(&(height as u16).to_be_bytes());
    vp09.extend_from_slice(&u32s(&[0x48_0000, 0x48_0000, 0]));
    vp09.extend_from_slice(&1u16.to_be_bytes()); // frame count
    vp09.extend_from_slice(&[0u8; 32]); // compressor name
    vp09.extend_from_slice(&[0, 0x18, 0xff, 0xff]); // depth, pre-defined
    vp09.extend_from_slice(&vpcc(codec));
    let stsd = fbx(b"stsd", 0, &[u32s(&[1]), bx(b"vp09", &vp09)].concat());

    // Runs of equal durations.
    let mut stts: Vec<(u32, u32)> = Vec::new();
    for &d in &durations {
        match stts.last_mut() {
            Some((c, last)) if *last == d => *c += 1,
            _ => stts.push((1, d)),
        }
    }
    let stts = fbx(
        b"stts",
        0,
        &[
            u32s(&[stts.len() as u32]),
            stts.iter().flat_map(|&(c, d)| u32s(&[c, d])).collect(),
        ]
        .concat(),
    );
    let stss = fbx(
        b"stss",
        0,
        &[u32s(&[sync.len() as u32]), u32s(&sync)].concat(),
    );
    let stsz = fbx(b"stsz", 0, &[u32s(&[0, n]), u32s(&sizes)].concat());
    let stsc = fbx(b"stsc", 0, &u32s(&[1, 1, n, 1]));

    let dinf = bx(
        b"dinf",
        &fbx(b"dref", 0, &[u32s(&[1]), fbx(b"url ", 1, &[])].concat()),
    );
    let vmhd = fbx(b"vmhd", 1, &[0u8; 8]);
    let mut hdlr = u32s(&[0]);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"voxl\0");
    let hdlr = fbx(b"hdlr", 0, &hdlr);
    let mut mdhd = u32s(&[0, 0, TIMESCALE, total as u32]);
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und"
    let mut tkhd = u32s(&[0, 0, 1, 0, total as u32, 0, 0]);
    tkhd.extend_from_slice(&[0u8; 4]); // layer, alternate group
    tkhd.extend_from_slice(&[0u8; 4]); // volume, reserved
    tkhd.extend_from_slice(&u32s(&MATRIX));
    tkhd.extend_from_slice(&u32s(&[width << 16, height << 16]));
    let mut mvhd = u32s(&[0, 0, TIMESCALE, movie as u32, 0x10000]);
    mvhd.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0, reserved
    mvhd.extend_from_slice(&[0u8; 8]);
    mvhd.extend_from_slice(&u32s(&MATRIX));
    mvhd.extend_from_slice(&[0u8; 24]); // pre-defined
    mvhd.extend_from_slice(&u32s(&[if audio.is_empty() { 2 } else { 3 }])); // next track id

    // moov, with each track's one chunk at `video_at` and `audio_at`: its size doesn't
    // depend on them.
    let moov = |video_at: u32, audio_at: u32| {
        let stco = fbx(b"stco", 0, &u32s(&[1, video_at]));
        let stbl = bx(
            b"stbl",
            &[
                stsd.clone(),
                stts.clone(),
                stss.clone(),
                stsz.clone(),
                stsc.clone(),
                stco,
            ]
            .concat(),
        );
        let minf = bx(b"minf", &[vmhd.clone(), dinf.clone(), stbl].concat());
        let mdia = bx(
            b"mdia",
            &[fbx(b"mdhd", 0, &mdhd), hdlr.clone(), minf].concat(),
        );
        let trak = bx(b"trak", &[fbx(b"tkhd", 3, &tkhd), mdia].concat());
        let audio_trak = match audio_span {
            Some((lead, ms)) => audio_trak(&audio, lead, ms, audio_at),
            None => Vec::new(),
        };
        bx(
            b"moov",
            &[fbx(b"mvhd", 0, &mvhd), trak, audio_trak].concat(),
        )
    };
    // moov goes first, so a player can start before the whole file has arrived: the
    // samples start right after ftyp, moov and mdat's header, the video's, then the
    // audio's.
    let video_at = (ftyp.len() + moov(0, 0).len() + 8) as u32;
    let moov = moov(video_at, video_at + video_len as u32);
    let mut out = Vec::with_capacity(video_at as usize + mdat_len as usize);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&(mdat_len as u32 + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for f in frames {
        out.extend_from_slice(&f.data);
    }
    for p in &audio {
        out.extend_from_slice(&p.data);
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A frame's bytes: 1 to 200 of any value (the writer copies a frame as it is).
    fn frame(rng: &mut Xorshift) -> Vec<u8> {
        (0..1 + rng.below(200))
            .map(|_| rng.below(256) as u8)
            .collect()
    }

    /// The box `name` at its first place in `mp4`, and its payload after its 8-byte header.
    fn find<'a>(mp4: &'a [u8], name: &[u8; 4]) -> (usize, &'a [u8]) {
        let at = mp4.windows(4).position(|w| w == name).expect("the box") - 4;
        let len = u32::from_be_bytes(mp4[at..at + 4].try_into().unwrap()) as usize;
        (at, &mp4[at + 8..at + len])
    }

    /// A recording is its frames as MP4 samples: the movie's header before its data, the
    /// chunk offset pointing at the first sample, and the data each frame as it is, in
    /// order, one sample size each; a `vp09` entry whose `vpcC` names the level; the
    /// keyframes the sync samples. Over generated recordings of a keyframe and 0 to 20
    /// frames more, keyframes among them.
    #[test]
    fn a_recording_is_its_frames_as_samples() {
        let mut rng = Xorshift::new(0x9b05_688c_2b3e_6c1f);
        for _ in 0..1_000 {
            let mut ms = 0;
            let mut frames = Vec::new();
            let (mut want_data, mut want_sizes, mut want_sync) =
                (Vec::new(), Vec::new(), Vec::new());
            for i in 0..1 + rng.below(21) {
                let data = frame(&mut rng);
                let key = i == 0 || rng.below(8) == 0;
                want_sizes.push(data.len() as u32);
                want_data.extend_from_slice(&data);
                if key {
                    want_sync.push(i as u32 + 1);
                }
                frames.push(Frame {
                    ms,
                    key,
                    data: data.into(),
                });
                ms += 1 + rng.below(100);
            }
            let end = ms + rng.below(100);
            let tracks = Tracks {
                frames,
                audio: Vec::new(),
            };
            let level = [10, 31, 41, 51, 62][rng.below(5) as usize];
            let out = write(&tracks, 64, 32, Vp9 { level }, end).unwrap();
            let (moov, _) = find(&out, b"moov");
            let (mdat, data) = find(&out, b"mdat");
            assert!(moov < mdat);
            assert_eq!(data, &want_data[..]);
            let (_, stco) = find(&out, b"stco");
            let first = u32::from_be_bytes(stco[8..12].try_into().unwrap()) as usize;
            assert_eq!(first, mdat + 8);
            let words = |b: &[u8]| -> Vec<u32> {
                b.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_be_bytes(*c))
                    .collect()
            };
            let (_, stsz) = find(&out, b"stsz");
            assert_eq!(words(&stsz[12..]), want_sizes);
            let (_, stss) = find(&out, b"stss");
            assert_eq!(words(&stss[8..]), want_sync);
            let (_, vpcc) = find(&out, b"vpcC");
            assert_eq!(vpcc, &[1, 0, 0, 0, 0, level, 0x80, 1, 13, 6, 0, 0]);
        }
    }

    /// The boxes in `data`, side by side: each its name and payload.
    fn children(mut data: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut out = Vec::new();
        while !data.is_empty() {
            let len = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
            out.push((data[4..8].try_into().unwrap(), &data[8..len]));
            data = &data[len..];
        }
        out
    }

    /// The payload of the one box `name` among `boxes`.
    fn only<'a>(boxes: &[([u8; 4], &'a [u8])], name: &[u8; 4]) -> &'a [u8] {
        let mut found = boxes.iter().filter(|(n, _)| n == name);
        let one = found.next().expect("the box").1;
        assert!(found.next().is_none());
        one
    }

    fn be32(b: &[u8]) -> u64 {
        u64::from(u32::from_be_bytes(b[..4].try_into().unwrap()))
    }

    /// A track as a player reads it: its kind, its samples (when each begins, in ticks of
    /// `scale` a second since the movie began, and its bytes), and its durations in ms.
    pub(crate) struct Read<'a> {
        pub kind: [u8; 4],
        pub scale: u64,
        pub samples: Vec<(u64, &'a [u8])>,
        /// What the track's header says, and what its edit list and its media add up to.
        header_ms: u64,
        placed_ms: u64,
    }

    /// The tracks of an MP4 and the movie's duration, read with none of the writer's code:
    /// the sample table's durations and sizes and its chunk's offset, and the edit list's
    /// empty edit as the time before a track's first sample.
    pub(crate) fn read(mp4: &[u8]) -> (Vec<Read<'_>>, u64) {
        let top = children(mp4);
        let moov = children(only(&top, b"moov"));
        let mvhd = only(&moov, b"mvhd");
        let movie_ms = be32(&mvhd[16..]);
        let mut tracks = Vec::new();
        for (_, trak) in moov.iter().filter(|(n, _)| n == b"trak") {
            let trak = children(trak);
            let header_ms = be32(&only(&trak, b"tkhd")[20..]);
            let mdia = children(only(&trak, b"mdia"));
            let scale = be32(&only(&mdia, b"mdhd")[12..]);
            let kind: [u8; 4] = only(&mdia, b"hdlr")[8..12].try_into().unwrap();
            let stbl = children(only(&children(only(&mdia, b"minf")), b"stbl"));
            let stts = only(&stbl, b"stts");
            let stsz = only(&stbl, b"stsz");
            let at = be32(&only(&stbl, b"stco")[8..]) as usize;
            let mut durations = Vec::new();
            for run in stts[8..].chunks(8) {
                durations.extend((0..be32(run)).map(|_| be32(&run[4..])));
            }
            let count = be32(&stsz[8..]) as usize;
            assert_eq!(durations.len(), count);
            let (mut begin, mut at) = (0, at);
            let mut samples = Vec::new();
            // Time before the first sample, in the movie's ms: an empty edit (media time -1).
            let mut lead_ms = 0;
            let mut placed_ms = 0;
            if let Some(edts) = trak.iter().find(|(n, _)| n == b"edts") {
                let elst = only(&children(edts.1), b"elst");
                for e in elst[8..].chunks(12) {
                    placed_ms += be32(e);
                    if be32(&e[4..]) == u64::from(u32::MAX) {
                        lead_ms += be32(e);
                    } else {
                        assert_eq!(be32(&e[4..]), 0, "media starts at its first sample");
                    }
                }
            } else {
                placed_ms = (durations.iter().sum::<u64>() * 1000) / scale;
            }
            begin += lead_ms * scale / 1000;
            for (i, d) in durations.iter().enumerate() {
                let size = be32(&stsz[12 + 4 * i..]) as usize;
                samples.push((begin, &mp4[at..at + size]));
                begin += d;
                at += size;
            }
            assert_eq!(
                placed_ms,
                lead_ms + durations.iter().sum::<u64>() * 1000 / scale,
                "the edit list places the whole media"
            );
            tracks.push(Read {
                kind,
                scale,
                samples,
                header_ms,
                placed_ms,
            });
        }
        (tracks, movie_ms)
    }

    /// A recording is its video and its audio on one timeline: read back, the MP4 holds
    /// exactly the frames given (as samples, each lasting until the next frame) and the
    /// Opus packets that begin at or after the first frame, each in the order given and
    /// beginning when it was given to, silences between them kept as the time between
    /// samples; the tracks' durations are what their samples add up to, and the movie's the
    /// longer. Over generated recordings of 1 to 20 frames and of 0 to 30 packets; the
    /// cases reached are counted, each of the five a recording can be in.
    #[test]
    fn a_recording_is_its_video_and_audio_on_one_timeline() {
        let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
        // No audio, all of it before the first frame, some of it, with a silence, without.
        let mut reached = [0u32; 5];
        for _ in 0..4_000 {
            let mut ms = 1 + rng.below(200);
            let first = ms;
            let mut frames = Vec::new();
            let mut want_video = Vec::new();
            for i in 0..1 + rng.below(20) {
                let sample = frame(&mut rng);
                want_video.push((ms - first, sample.clone()));
                frames.push(Frame {
                    ms,
                    key: i == 0,
                    data: sample.into(),
                });
                ms += 1 + rng.below(100);
            }
            let end = ms + rng.below(100);
            let mut audio = Vec::new();
            let mut at = rng.below(first + 300);
            let silences = rng.below(3) == 0;
            for _ in 0..rng.below(31) {
                let data: Vec<u8> = (0..1 + rng.below(60))
                    .map(|_| rng.below(256) as u8)
                    .collect();
                audio.push(AudioPacket {
                    ms: at,
                    data: data.into(),
                });
                at += AUDIO_MS * (1 + if silences { rng.below(4) } else { 0 });
            }
            let tracks = Tracks { frames, audio };
            let want_audio: Vec<&AudioPacket> =
                tracks.audio.iter().filter(|p| p.ms >= first).collect();
            reached[match (tracks.audio.len(), want_audio.len()) {
                (0, _) => 0,
                (_, 0) => 1,
                (n, k) if n > k => 2,
                _ if want_audio.windows(2).any(|w| w[1].ms > w[0].ms + AUDIO_MS) => 3,
                _ => 4,
            }] += 1;

            let out = write(&tracks, 64, 32, Vp9 { level: 10 }, end).unwrap();
            let (read, movie_ms) = read(&out);
            assert_eq!(read.len(), 1 + usize::from(!want_audio.is_empty()));

            let video = &read[0];
            assert_eq!(&video.kind, b"vide");
            assert_eq!(video.scale, 1000);
            let have: Vec<(u64, &[u8])> = video.samples.clone();
            let want: Vec<(u64, &[u8])> = want_video.iter().map(|(t, d)| (*t, &d[..])).collect();
            assert_eq!(have, want);
            // Each frame lasts until the next, the last until the end.
            let video_ms = end - first;
            assert_eq!(video.header_ms, video_ms);
            assert_eq!(video.placed_ms, video_ms);

            let mut movie = video_ms;
            if let Some(audio) = read.get(1) {
                assert_eq!(&audio.kind, b"soun");
                assert_eq!(audio.scale, u64::from(AUDIO_SCALE));
                let want: Vec<(u64, &[u8])> = want_audio
                    .iter()
                    .map(|p| ((p.ms - first) * audio.scale / 1000, &p.data[..]))
                    .collect();
                assert_eq!(audio.samples, want);
                let (head, last) = (want_audio[0], want_audio[want_audio.len() - 1]);
                let lasts = last.ms + AUDIO_MS - head.ms;
                assert_eq!(audio.placed_ms, head.ms - first + lasts);
                assert_eq!(audio.header_ms, audio.placed_ms);
                movie = movie.max(audio.header_ms);
            }
            assert_eq!(movie_ms, movie);
        }
        assert!(reached.iter().all(|&n| n > 0), "cases reached: {reached:?}");
    }
}
