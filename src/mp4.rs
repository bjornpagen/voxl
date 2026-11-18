//! Minimal MP4 writer for recordings: the H.264 voxl already sends, rewrapped without
//! re-encoding. Frames arrive only when the screen changes, so each sample lasts until
//! the next one (variable frame rate).

/// One encoded frame: time since the recording started (ms), keyframe, Annex-B data.
pub struct Frame {
    pub ms: u64,
    pub key: bool,
    pub data: bytes::Bytes,
}

/// The movie's time unit, in ticks a second: milliseconds, the unit each frame's time is
/// kept in (`Frame::ms`).
const TIMESCALE: u32 = 1000;

/// NAL units of an Annex-B stream (start codes removed).
fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                // A 4-byte start code leaves a zero at the end of the previous unit.
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                out.push(&data[s..end]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        out.push(&data[s..]);
    }
    out.into_iter().filter(|n| !n.is_empty()).collect()
}

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

/// Writes `frames` (the first must be a keyframe) as an MP4 of `width`x`height`
/// lasting until `end_ms`.
pub fn write(frames: &[Frame], width: u32, height: u32, end_ms: u64) -> Result<Vec<u8>, String> {
    let first = frames.first().ok_or("no frames were recorded")?;
    if !first.key {
        return Err("recording doesn't start with a keyframe".into());
    }
    let (mut sps, mut pps) = (None, None);
    for n in nals(&first.data) {
        match n[0] & 0x1f {
            7 if sps.is_none() => sps = Some(n.to_vec()),
            8 if pps.is_none() => pps = Some(n.to_vec()),
            _ => {}
        }
    }
    let (sps, pps) = (
        sps.ok_or("keyframe without SPS")?,
        pps.ok_or("keyframe without PPS")?,
    );
    if sps.len() < 4 {
        return Err("bad SPS".into());
    }

    // Samples: length-prefixed NAL units, parameter sets and delimiters left out. They
    // are only sized here and copied once, straight into the output, at the end.
    let mut samples: Vec<Vec<&[u8]>> = Vec::with_capacity(frames.len());
    let (mut sizes, mut durations, mut sync) = (Vec::new(), Vec::new(), Vec::new());
    for (i, f) in frames.iter().enumerate() {
        let units: Vec<&[u8]> = nals(&f.data)
            .into_iter()
            .filter(|n| !matches!(n[0] & 0x1f, 7..=9))
            .collect();
        sizes.push(units.iter().map(|n| n.len() as u32 + 4).sum::<u32>());
        samples.push(units);
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
    let mdat_len: u64 = sizes.iter().map(|&s| s as u64).sum();

    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");
    if mdat_len + 8 > u32::MAX as u64 {
        return Err("recording too large".into());
    }
    // Patched in once moov's size is known (it doesn't depend on this value).
    let chunk_offset = 0;

    let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(&sps);
    avcc.push(1);
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(&pps);
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        // 4:2:0, 8-bit, no SPS extensions.
        avcc.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    }
    let mut avc1 = vec![0u8; 6];
    avc1.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    avc1.extend_from_slice(&[0u8; 16]);
    avc1.extend_from_slice(&(width as u16).to_be_bytes());
    avc1.extend_from_slice(&(height as u16).to_be_bytes());
    avc1.extend_from_slice(&u32s(&[0x48_0000, 0x48_0000, 0]));
    avc1.extend_from_slice(&1u16.to_be_bytes()); // frame count
    avc1.extend_from_slice(&[0u8; 32]); // compressor name
    avc1.extend_from_slice(&[0, 0x18, 0xff, 0xff]); // depth, pre-defined
    avc1.extend_from_slice(&bx(b"avcC", &avcc));
    let stsd = fbx(b"stsd", 0, &[u32s(&[1]), bx(b"avc1", &avc1)].concat());

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
    let stco = fbx(b"stco", 0, &u32s(&[1, chunk_offset]));
    let stbl = bx(b"stbl", &[stsd, stts, stss, stsz, stsc, stco].concat());

    let dinf = bx(
        b"dinf",
        &fbx(b"dref", 0, &[u32s(&[1]), fbx(b"url ", 1, &[])].concat()),
    );
    let vmhd = fbx(b"vmhd", 1, &[0u8; 8]);
    let minf = bx(b"minf", &[vmhd, dinf, stbl].concat());
    let mut hdlr = u32s(&[0]);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"voxl\0");
    let hdlr = fbx(b"hdlr", 0, &hdlr);
    let mut mdhd = u32s(&[0, 0, TIMESCALE, total as u32]);
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und"
    let mdia = bx(b"mdia", &[fbx(b"mdhd", 0, &mdhd), hdlr, minf].concat());

    let mut tkhd = u32s(&[0, 0, 1, 0, total as u32, 0, 0]);
    tkhd.extend_from_slice(&[0u8; 4]); // layer, alternate group
    tkhd.extend_from_slice(&[0u8; 4]); // volume, reserved
    tkhd.extend_from_slice(&u32s(&MATRIX));
    tkhd.extend_from_slice(&u32s(&[width << 16, height << 16]));
    let trak = bx(b"trak", &[fbx(b"tkhd", 3, &tkhd), mdia].concat());

    let mut mvhd = u32s(&[0, 0, TIMESCALE, total as u32, 0x10000]);
    mvhd.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0, reserved
    mvhd.extend_from_slice(&[0u8; 8]);
    mvhd.extend_from_slice(&u32s(&MATRIX));
    mvhd.extend_from_slice(&[0u8; 24]); // pre-defined
    mvhd.extend_from_slice(&u32s(&[2])); // next track id
    let mut moov = bx(b"moov", &[fbx(b"mvhd", 0, &mvhd), trak].concat());

    // moov goes first, so a player can start before the whole file has arrived. stco is
    // the last box in moov, and its one chunk offset the last 4 bytes: the samples start
    // right after ftyp, moov and mdat's header.
    let offset = (ftyp.len() + moov.len() + 8) as u32;
    let at = moov.len() - 4;
    moov[at..].copy_from_slice(&offset.to_be_bytes());
    let mut out = Vec::with_capacity(offset as usize + mdat_len as usize);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&(mdat_len as u32 + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for n in samples.iter().flatten() {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A NAL unit of `kind` (its header's low five bits), its payload as H.264 keeps it:
    /// no two zero bytes in a row (emulation prevention keeps three from meeting) and a
    /// last byte that is not zero (the RBSP's stop bit).
    fn nal(rng: &mut Xorshift, kind: u8) -> Vec<u8> {
        let mut n = vec![0x60 | kind];
        for _ in 0..rng.below(40) {
            let b = match (n.last(), rng.below(8)) {
                (Some(0), _) | (_, 1..) => 1 + rng.below(255) as u8,
                (_, 0) => 0,
            };
            n.push(b);
        }
        if n.last() == Some(&0) {
            n.push(1);
        }
        n
    }

    /// Units joined as Annex B, each after a start code of three or four bytes.
    fn annex_b(rng: &mut Xorshift, units: &[Vec<u8>]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|u| {
                let code: &[u8] = match rng.below(2) {
                    0 => &[0, 0, 1],
                    _ => &[0, 0, 0, 1],
                };
                code.iter().chain(u).copied().collect::<Vec<u8>>()
            })
            .collect()
    }

    /// An Annex B stream splits into exactly the units it was made of, whichever start
    /// codes joined them: over generated streams of 0 to 12 units.
    #[test]
    fn annex_b_splits_into_its_units() {
        let mut rng = Xorshift::new(0x1f83_d9ab_5be0_cd19);
        for _ in 0..5_000 {
            let units: Vec<Vec<u8>> = (0..rng.below(13))
                .map(|_| {
                    let kind = 1 + rng.below(23) as u8;
                    nal(&mut rng, kind)
                })
                .collect();
            let stream = annex_b(&mut rng, &units);
            let want: Vec<&[u8]> = units.iter().map(Vec::as_slice).collect();
            assert_eq!(nals(&stream), want);
        }
    }

    /// The box `name` at its first place in `mp4`, and its payload after its 8-byte header.
    fn find<'a>(mp4: &'a [u8], name: &[u8; 4]) -> (usize, &'a [u8]) {
        let at = mp4.windows(4).position(|w| w == name).expect("the box") - 4;
        let len = u32::from_be_bytes(mp4[at..at + 4].try_into().unwrap()) as usize;
        (at, &mp4[at + 8..at + len])
    }

    /// A recording is its frames as MP4 samples: the movie's header before its data,
    /// the chunk offset pointing at the first sample, and the data each frame's units
    /// length-prefixed in order with its parameter sets and delimiters left out, one
    /// sample size each; each sample lasts until the next frame, the last until the end.
    /// Over generated recordings of a keyframe (with SPS and PPS) and 0 to 20 frames more.
    #[test]
    fn a_recording_is_its_frames_as_samples() {
        let mut rng = Xorshift::new(0x9b05_688c_2b3e_6c1f);
        for _ in 0..1_000 {
            let mut ms = 0;
            let mut frames = Vec::new();
            let mut want_data = Vec::new();
            let mut want_sizes = Vec::new();
            for i in 0..1 + rng.below(21) {
                let mut units = Vec::new();
                if i == 0 {
                    let mut sps = nal(&mut rng, 7);
                    sps.extend([100, 0, 40]);
                    units.extend([sps, nal(&mut rng, 8)]);
                }
                if rng.below(4) == 0 {
                    units.push(nal(&mut rng, 9));
                }
                let kind = if i == 0 { 5 } else { 1 };
                units.extend((0..1 + rng.below(3)).map(|_| nal(&mut rng, kind)));
                let sample: Vec<u8> = units
                    .iter()
                    .filter(|u| !matches!(u[0] & 0x1f, 7..=9))
                    .flat_map(|u| (u.len() as u32).to_be_bytes().into_iter().chain(u.clone()))
                    .collect();
                want_sizes.push(sample.len() as u32);
                want_data.extend(sample);
                frames.push(Frame {
                    ms,
                    key: i == 0,
                    data: annex_b(&mut rng, &units).into(),
                });
                ms += 1 + rng.below(100);
            }
            let end = ms + rng.below(100);
            let out = write(&frames, 64, 32, end).unwrap();
            let (moov, _) = find(&out, b"moov");
            let (mdat, data) = find(&out, b"mdat");
            assert!(moov < mdat);
            assert_eq!(data, &want_data[..]);
            let (_, stco) = find(&out, b"stco");
            let first = u32::from_be_bytes(stco[8..12].try_into().unwrap()) as usize;
            assert_eq!(first, mdat + 8);
            let (_, stsz) = find(&out, b"stsz");
            let sizes: Vec<u32> = stsz[12..]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_be_bytes(*c))
                .collect();
            assert_eq!(sizes, want_sizes);
        }
    }
}
