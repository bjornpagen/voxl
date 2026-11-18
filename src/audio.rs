//! Desktop audio: PulseAudio's monitor of the desktop's output, coded as Opus in
//! `sound::DESKTOP` frames, sent to the sessions that want it. It reads only while a
//! session wants audio, and sends nothing for silence.

use crate::hub::Hub;
use crate::sound::{DESKTOP, PACKET_MOST, RATE};
use std::ffi::{c_char, c_int, c_void};
use std::sync::Arc;

#[repr(C)]
struct PaSampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}
#[repr(C)]
struct PaBufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}
#[link(name = "pulse-simple")]
extern "C" {
    fn pa_simple_new(
        server: *const c_char,
        name: *const c_char,
        dir: c_int,
        dev: *const c_char,
        stream: *const c_char,
        ss: *const PaSampleSpec,
        map: *const c_void,
        attr: *const PaBufferAttr,
        error: *mut c_int,
    ) -> *mut c_void;
    fn pa_simple_read(s: *mut c_void, data: *mut c_void, bytes: usize, error: *mut c_int) -> c_int;
    fn pa_simple_free(s: *mut c_void);
}
#[link(name = "opus")]
extern "C" {
    fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encoder_ctl(st: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encode(
        st: *mut c_void,
        pcm: *const i16,
        frame_size: c_int,
        data: *mut u8,
        max: i32,
    ) -> i32;
}

/// pulse/def.h and pulse/sample.h: a stream that records, of signed 16-bit little-endian
/// samples.
const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_S16LE: c_int = 3;
/// opus_defines.h: the application that adds the least delay, and the request that sets
/// the bitrate.
const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_OK: c_int = 0;

/// The PulseAudio source read: the monitor of the sink named `output`, the desktop's
/// output as its sound server is set up, so what the desktop plays.
const DEVICE: &std::ffi::CStr = c"output.monitor";

/// The desktop's audio bitrate, bits a second: the top of RFC 7587's range for full-band
/// stereo music (§3.1.1: 64 to 128 kbit/s), which a desktop's audio may be.
const BITRATE: c_int = 128_000;

/// The most PulseAudio holds for the reader, in frames: audio older than this is
/// dropped, not read late. Provisional: 80 ms, eight frames, what keeps a reader
/// descheduled for a few frames from losing any while capping the delay it can build;
/// what would measure it is the drops `pa_simple_read` shows with the desktop busy.
const HELD_FRAMES: u32 = 8;

/// Starts the desktop's audio thread. PulseAudio is opened while a session wants audio,
/// and closed when none does; a sound server that cannot be opened or read ends the
/// daemon, which its service manager starts again, rather than leave it without audio.
pub fn spawn(hub: Arc<Hub>) {
    let started = std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            if let Err(e) = run(&hub) {
                log::error!("desktop audio: {e}");
                crate::logging::exit(1);
            }
        });
    if let Err(e) = started {
        log::error!("desktop audio: no thread: {e}");
        crate::logging::exit(1);
    }
}

fn run(hub: &Hub) -> Result<(), String> {
    let mut err = 0;
    // SAFETY: the arguments are plain values and a place for the error.
    let enc = unsafe {
        opus_encoder_create(
            RATE as i32,
            DESKTOP.channels.into(),
            OPUS_APPLICATION_RESTRICTED_LOWDELAY,
            &mut err,
        )
    };
    if enc.is_null() {
        return Err(format!("Opus's encoder: error {err}"));
    }
    // SAFETY: a live encoder and the request's one int.
    let set = unsafe { opus_encoder_ctl(enc, OPUS_SET_BITRATE_REQUEST, BITRATE) };
    if set != OPUS_OK {
        return Err(format!("Opus's bitrate: error {set}"));
    }
    let name = c"voxl";
    let ss = PaSampleSpec {
        format: PA_SAMPLE_S16LE,
        rate: RATE,
        channels: DESKTOP.channels,
    };
    let samples = DESKTOP.frame * usize::from(DESKTOP.channels);
    let frame_bytes = (samples * std::mem::size_of::<i16>()) as u32;
    // A recording stream reads fragments of `fragsize`; the other fields are a
    // playback's, left to the server (`u32::MAX`, as pulse/def.h says).
    let attr = PaBufferAttr {
        maxlength: frame_bytes * HELD_FRAMES,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: frame_bytes,
    };
    let mut pcm = vec![0i16; samples];
    let mut out = vec![0u8; PACKET_MOST];
    loop {
        hub.wait_for_audio();
        // SAFETY: every pointer is to a live value of the type pa_simple_new reads.
        let s = unsafe {
            pa_simple_new(
                std::ptr::null(),
                name.as_ptr(),
                PA_STREAM_RECORD,
                DEVICE.as_ptr(),
                name.as_ptr(),
                &ss,
                std::ptr::null(),
                &attr,
                &mut err,
            )
        };
        if s.is_null() {
            return Err(format!("PulseAudio's monitor: error {err}"));
        }
        log::info!("audio capture started");
        while hub.audio_wanted() {
            // SAFETY: `pcm` holds the bytes asked for.
            let read =
                unsafe { pa_simple_read(s, pcm.as_mut_ptr().cast(), pcm.len() * 2, &mut err) };
            if read < 0 {
                // SAFETY: the stream is live, and freed once.
                unsafe { pa_simple_free(s) };
                return Err(format!("PulseAudio's read: error {err}"));
            }
            if pcm.iter().all(|&s| s == 0) {
                continue;
            }
            // SAFETY: a live encoder, a frame of samples, and room for a packet.
            let n = unsafe {
                opus_encode(
                    enc,
                    pcm.as_ptr(),
                    DESKTOP.frame as c_int,
                    out.as_mut_ptr(),
                    out.len() as i32,
                )
            };
            match usize::try_from(n) {
                Ok(n) => hub.audio(&out[..n]),
                Err(_) => {
                    unsafe { pa_simple_free(s) };
                    return Err(format!("Opus's encode: error {n}"));
                }
            }
        }
        // SAFETY: the stream is live, and freed once.
        unsafe { pa_simple_free(s) };
        log::info!("audio capture stopped");
    }
}
