//! Microphone: Opus packets from a browser -> a FIFO that PulseAudio reads
//! as the desktop's microphone (module-pipe-source). One session holds the microphone at
//! a time; a session that turns its microphone on takes it over.
//!
//! A writer thread feeds the FIFO a `sound::MIC` frame on its own clock: the user's audio
//! when there is some, silence otherwise, so apps recording the microphone never stall.
//! A pipe source plays whatever arrives, so this clock (not PulseAudio's timer, which runs
//! slow at low latency on some VMs) sets the pace. Audio waits in a short jitter queue
//! that drops its oldest samples past `MAX_QUEUED`, so bursts after a network stall or
//! clock drift never become growing delay.

use crate::event::MicMsg;
use crate::hub::{Hub, Out, ServerMessage};
use crate::sound::{MIC, RATE};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[link(name = "opus")]
extern "C" {
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_decode(
        st: *mut c_void,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
}

/// A frame's samples, and how long it plays.
const FRAME: usize = MIC.frame;
const FRAME_TIME: Duration = Duration::from_micros(FRAME as u64 * 1_000_000 / RATE as u64);

/// Playing starts once this much is queued, two frames, so a frame that comes up to a
/// frame late plays on time. Provisional: one frame of the page's jitter, unmeasured;
/// what would measure it is the underruns `microphone off` logs over a real link.
const PREFILL: usize = 2 * FRAME;

/// The most audio that waits to be played, six frames: past it the oldest is dropped, so
/// the delay a burst leaves is bounded by it. Provisional, as `PREFILL`; measured by the
/// samples `microphone off` logs dropped against the delay a listener hears.
const MAX_QUEUED: usize = 6 * FRAME;

/// The longest Opus packet decodes to this many samples: 120 ms, the most one packet
/// holds (RFC 6716 §3.2.5, "up to 120 ms").
const DECODED_MOST: usize = RATE as usize * 120 / 1000;

/// The FIFO's buffer, in bytes: one page, the least a pipe holds on Linux (pipe(7)), so
/// audio that piles up while nothing records is at most this stale when something starts.
const FIFO_BYTES: c_int = 4096;

#[derive(Default)]
pub struct Queue {
    samples: VecDeque<i16>,
    playing: bool,
    dropped: usize,
    /// Times the jitter queue ran dry mid-stream (each one is an audible gap).
    underruns: usize,
}

/// The next frame to play: queued audio once the jitter buffer has filled, else silence.
pub fn next_frame(queue: &Mutex<Queue>, frame: &mut [i16]) {
    let mut q = queue.lock();
    if !q.playing && q.samples.len() >= PREFILL {
        q.playing = true;
    }
    frame.fill(0);
    if q.playing {
        let n = q.samples.len().min(frame.len());
        for (d, s) in frame.iter_mut().zip(q.samples.drain(..n)) {
            *d = s;
        }
        if n < frame.len() {
            q.playing = false; // ran dry: refill the jitter buffer first
            q.underruns += 1;
        }
    }
}

/// Opens the FIFO at `fifo` without blocking, shrinks its buffer to `FIFO_BYTES`, and makes
/// it this user's alone (0600). It opens
/// once PulseAudio has it open for reading (`ENXIO` until then): an absent reader is
/// `None`, any other failure the error.
fn open_fifo(fifo: &std::path::Path) -> nix::Result<Option<std::os::fd::OwnedFd>> {
    use nix::fcntl::{fcntl, open, FcntlArg, OFlag};
    use nix::sys::stat::Mode;
    let flags = OFlag::O_WRONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let fd = match open(fifo, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(nix::errno::Errno::ENXIO | nix::errno::Errno::ENOENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    fcntl(&fd, FcntlArg::F_SETPIPE_SZ(FIFO_BYTES))?;
    // The sound server made it with its umask: the runtime folder's group may reach only
    // the lane sockets, so the microphone is this user's alone from here.
    nix::sys::stat::fchmod(&fd, Mode::S_IRUSR | Mode::S_IWUSR)?;
    Ok(Some(fd))
}

/// The writer: a frame every `FRAME_TIME`, on its own clock. The FIFO's reader comes
/// and goes with the sound server, so the writer opens it at a frame's turn, never on a
/// wait of its own, and a frame nobody reads (`EAGAIN`, or a reader gone) is not
/// needed. A writer that falls further behind than the jitter queue holds (the machine
/// suspended) starts its clock again, rather than write what it missed at once.
fn writer(fifo: &std::path::Path, queue: Arc<Mutex<Queue>>) -> nix::Result<()> {
    use nix::errno::Errno;
    use nix::unistd::write;
    let behind_most = FRAME_TIME * (MAX_QUEUED / FRAME) as u32;
    let mut fd: Option<std::os::fd::OwnedFd> = None;
    let mut frame = vec![0i16; FRAME];
    let mut bytes = vec![0u8; FRAME * 2];
    let mut next = Instant::now();
    loop {
        next += FRAME_TIME;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > behind_most {
            next = now;
        }
        next_frame(&queue, &mut frame);
        if fd.is_none() {
            fd = open_fifo(fifo)?;
            if fd.is_some() {
                log::info!("microphone: writing to {}", fifo.display());
            }
        }
        if let Some(f) = &fd {
            for (b, s) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(&frame) {
                *b = s.to_le_bytes();
            }
            match write(f, &bytes) {
                Ok(_) | Err(Errno::EAGAIN) => {}
                // The reader closed it: open it again when there is one.
                Err(Errno::EPIPE) => fd = None,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Starts the microphone: the writer, and the thread that takes sessions' messages,
/// decodes their audio and queues it. A thread that cannot start, or an Opus decoder that
/// cannot be made, ends the daemon; so does a writer that fails other than for want of
/// a reader.
/// `fifo` is the FIFO PulseAudio's pipe source reads as the desktop's microphone (its
/// `module-pipe-source file=`), in the runtime folder.
pub fn spawn(hub: Arc<Hub>, fifo: std::path::PathBuf) -> mpsc::Sender<MicMsg> {
    let (tx, rx) = mpsc::channel::<MicMsg>();
    let queue = Arc::new(Mutex::new(Queue::default()));
    let fatal = |what: &str, e: &dyn std::fmt::Display| -> ! {
        log::error!("microphone: {what}: {e}");
        crate::logging::exit(1)
    };
    let mut err = 0;
    // SAFETY: plain values and a place for the error.
    let dec = unsafe { opus_decoder_create(RATE as i32, MIC.channels.into(), &mut err) };
    if dec.is_null() {
        fatal("Opus's decoder", &err);
    }
    let dec = Decoder(dec);
    {
        let queue = queue.clone();
        let started = std::thread::Builder::new()
            .name("mic-out".into())
            .spawn(move || {
                if let Err(e) = writer(&fifo, queue) {
                    fatal("the FIFO", &e);
                }
            });
        if let Err(e) = started {
            fatal("no writer thread", &e);
        }
    }
    let started = std::thread::Builder::new()
        .name("mic".into())
        .spawn(move || {
            let dec = dec;
            let mut owner: Option<u64> = None;
            let mut since = Instant::now();
            let mut pcm = vec![0i16; DECODED_MOST];
            let tell = |id: u64, on: bool| {
                if let Some(c) = hub.clients.lock().iter().find(|c| c.id == id) {
                    c.send(Out::Text(ServerMessage::Mic { on }));
                }
            };
            let push = |samples: &[i16]| {
                let mut q = queue.lock();
                q.samples.extend(samples);
                let over = q.samples.len().saturating_sub(MAX_QUEUED);
                if over > 0 {
                    q.samples.drain(..over);
                    q.dropped += over;
                }
            };
            let reset = || {
                let mut q = queue.lock();
                let stats = (q.dropped, q.underruns);
                *q = Queue::default();
                stats
            };
            while let Ok(m) = rx.recv() {
                match m {
                    MicMsg::Start(id) => {
                        if let Some(prev) = owner.filter(|&p| p != id) {
                            tell(prev, false);
                        }
                        reset();
                        owner = Some(id);
                        since = Instant::now();
                        tell(id, true);
                        log::info!("microphone on (client {id})");
                    }
                    MicMsg::Opus(id, b) if owner == Some(id) => {
                        // SAFETY: a live decoder, the packet, and room for its samples.
                        let n = unsafe {
                            opus_decode(
                                dec.0,
                                b.as_ptr(),
                                b.len() as i32,
                                pcm.as_mut_ptr(),
                                pcm.len() as c_int,
                                0,
                            )
                        };
                        // A packet Opus cannot decode is the page's mistake: dropped, said.
                        match usize::try_from(n) {
                            Ok(n) => push(&pcm[..n]),
                            Err(_) => log::warn!("microphone: a packet Opus refused: {n}"),
                        }
                    }
                    MicMsg::Stop(id) if owner == Some(id) => {
                        owner = None;
                        let (dropped, underruns) = reset();
                        log::info!(
                            "microphone off (client {id}, {:.1} s, {} ms dropped, {underruns} underruns)",
                            since.elapsed().as_secs_f64(),
                            dropped * 1000 / RATE as usize
                        );
                    }
                    // Another session's audio, or its stop, while it does not hold it.
                    MicMsg::Opus(..) | MicMsg::Stop(_) => {}
                }
            }
        });
    if let Err(e) = started {
        fatal("no thread", &e);
    }
    tx
}

/// Opus's decoder, moved to the thread that alone uses it.
struct Decoder(*mut c_void);
// SAFETY: the decoder is used by one thread at a time: the one it is moved to.
unsafe impl Send for Decoder {}
