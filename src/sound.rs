//! The daemon's two audio formats, each the page's too: the desktop's audio, which it codes
//! as Opus and sends, and the microphone, which the page sends it as Opus and it
//! plays. `desktopAudio` and `microphone` in web/src/wire.json are the page's copy, which
//! a test here checks against these.

/// Opus's full-band rate, in samples a second a channel: the highest of the five Opus
/// codes at (RFC 6716 §2: "8, 12, 16, 24, or 48 kHz"), the only one that keeps all of a
/// desktop's audio, and one every decoder the page has takes.
pub const RATE: u32 = 48_000;

/// An audio stream's format: its channels, and the samples a channel of one Opus frame
/// holds at `RATE`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Format {
    pub channels: u8,
    pub frame: usize,
}

/// The samples a channel of an Opus frame of `millis` holds at `RATE`.
const fn frame(millis: usize) -> usize {
    RATE as usize * millis / 1000
}

/// The desktop's audio: stereo, as a desktop plays it, in 10 ms frames, one of Opus's
/// frame durations (RFC 6716 §2.1.4: 2.5, 5, 10, 20, 40 or 60 ms). A frame waits whole
/// before it is coded, so its duration is latency the audio adds: 10 ms is the shortest
/// every Opus mode takes (SILK, hybrid and CELT, §2), so the encoder keeps its choice
/// of mode, at the cost of twice the packets of 20 ms.
pub const DESKTOP: Format = Format {
    channels: 2,
    frame: frame(10),
};

/// The microphone: mono, a voice, in 20 ms frames, the frame the page's encoder sends
/// (web/src/client/mic.ts, `MICROPHONE.frameSamples`), so the daemon plays one of its
/// frames in each of its own.
pub const MIC: Format = Format {
    channels: 1,
    frame: frame(20),
};

#[cfg(target_os = "linux")]
/// The longest an Opus packet of one frame is, in bytes: its TOC byte, then a frame of at
/// most 1275 bytes (RFC 6716 §3.4, R2: "No implicit frame length is larger than 1275
/// bytes").
pub const PACKET_MOST: usize = 1 + 1275;

#[cfg(test)]
mod tests {
    use super::*;

    /// The formats are the page's: web/src/wire.json says each rate, channel count and
    /// frame the same.
    #[test]
    fn the_formats_are_the_pages() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../web/src/wire.json")).unwrap();
        let desktop = &wire["desktopAudio"];
        assert_eq!(desktop["sampleRate"], RATE);
        assert_eq!(desktop["channels"], DESKTOP.channels);
        assert_eq!(desktop["frameSamples"], DESKTOP.frame);
        let mic = &wire["microphone"];
        assert_eq!(mic["sampleRate"], RATE);
        assert_eq!(mic["frameSamples"], MIC.frame);
    }
}
