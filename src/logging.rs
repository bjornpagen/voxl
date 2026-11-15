//! voxl's log is written by a thread of its own, and every other thread only hands it lines.
//!
//! Writing a line waits for whatever reads stderr. Under systemd that is a stream socket to
//! journald, and when journald falls behind (writing its own files on a busy disk, say) the
//! socket fills and every write blocks until it reads again; a terminal blocks while it
//! draws. A capture or an encode that logged waited with it: per-frame lines written to a
//! busy disk from the encoder threads took a three-line scroll from 43 ms a frame to 155,
//! and held one encode for 4 s. So a thread that logs formats its line into a slot of a
//! bounded queue and goes on, and the log thread writes the lines in order. When the queue
//! is full a line is dropped and counted, and the count is written after the lines that
//! were waiting: no thread that logs ever waits for the log.

use log::Log as _;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::Arc;

/// Lines the queue holds while the log thread cannot write. The busiest minute of a week of
/// a live desktop's journal logged 1,683 lines (a resize failing over and over, 39 a
/// second). Each line is a slot of `LINE` bytes, made when voxl starts, so the queue is
/// half a megabyte. At 1,024 that minute's lines would be dropped after 26 s of a stalled
/// writer; at 4,096 the memory doubles for more than any minute of that week logged.
const QUEUED: usize = 2048;

/// The longest line kept, target and message, in bytes; a longer one is cut at a character
/// and ends in `CUT`. The longest a week of a live desktop's journal held was 236: a RandR
/// error of 225 under `voxl::video`. At 128 that line and every other X error would be cut;
/// at 512 the queue doubles and no line of that week needed it.
const LINE: usize = 256;

/// What a line that was longer than `LINE` ends in.
const CUT: &str = "…";

/// A line as it was logged: formatted where it was logged, written by the log thread.
struct Line {
    level: log::Level,
    /// `text[..target]` is the record's target, `text[target..len]` its message.
    target: usize,
    len: usize,
    text: [u8; LINE],
}

/// A line being formatted: the whole characters that fit, and how much was asked for.
/// What follows a character that did not fit lands in its last three bytes, past where
/// `Line::of` cuts it.
struct Fill {
    text: [u8; LINE],
    len: usize,
    /// Bytes written to it, kept or not.
    asked: usize,
}

impl std::fmt::Write for Fill {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let keep = s.floor_char_boundary(LINE - self.len);
        self.text[self.len..self.len + keep].copy_from_slice(&s.as_bytes()[..keep]);
        self.len += keep;
        self.asked += s.len();
        Ok(())
    }
}

impl Line {
    fn of(record: &log::Record) -> Line {
        let mut fill = Fill {
            text: [0; LINE],
            len: 0,
            asked: 0,
        };
        let _ = fill.write_str(record.target());
        let mut target = fill.len;
        let _ = write!(fill, "{}", record.args());
        let Fill {
            mut text,
            mut len,
            asked,
        } = fill;
        if asked > len {
            let kept = std::str::from_utf8(&text[..len]).expect("whole characters");
            let at = kept.floor_char_boundary(LINE - CUT.len());
            text[at..at + CUT.len()].copy_from_slice(CUT.as_bytes());
            len = at + CUT.len();
            target = target.min(at);
        }
        Line {
            level: record.level(),
            target,
            len,
            text,
        }
    }

    fn target(&self) -> &str {
        std::str::from_utf8(&self.text[..self.target]).expect("whole characters")
    }

    fn message(&self) -> &str {
        std::str::from_utf8(&self.text[self.target..self.len]).expect("whole characters")
    }
}

// A line is queued in place, in a slot made when voxl starts: boxed, every line logged
// would be an allocation.
#[allow(clippy::large_enum_variant)]
enum Msg {
    Line(Line),
    /// Write everything sent before this, then answer.
    Flush(mpsc::SyncSender<()>),
}

/// What every thread logs to: it formats the line and queues it, or drops and counts it.
pub struct Logger {
    /// Which records are wanted (`RUST_LOG`), as the log thread's writer has it.
    writer: Arc<env_logger::Logger>,
    queue: mpsc::SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
}

/// The log thread's side: writes each line as `env_logger` formats it.
pub struct Writer {
    writer: Arc<env_logger::Logger>,
    queue: mpsc::Receiver<Msg>,
    dropped: Arc<AtomicU64>,
}

/// A queue of `slots` lines in front of `writer`.
fn queue(writer: env_logger::Logger, slots: usize) -> (Logger, Writer) {
    let writer = Arc::new(writer);
    let dropped = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::sync_channel(slots);
    (
        Logger {
            writer: writer.clone(),
            queue: tx,
            dropped: dropped.clone(),
        },
        Writer {
            writer,
            queue: rx,
            dropped,
        },
    )
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.writer.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.writer.matches(record) {
            return;
        }
        match self.queue.try_send(Msg::Line(Line::of(record))) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Waits until every line logged before it is written.
    fn flush(&self) {
        let (done, written) = mpsc::sync_channel(1);
        if self.queue.send(Msg::Flush(done)).is_ok() {
            let _ = written.recv();
        }
    }
}

impl Writer {
    /// Writes lines as they come, until the `Logger` is gone.
    fn run(self) {
        while let Ok(msg) = self.queue.recv() {
            self.write(msg);
            self.drain();
        }
    }

    /// Writes every line queued now, then how many were dropped since the last count.
    fn drain(&self) {
        while let Ok(msg) = self.queue.try_recv() {
            self.write(msg);
        }
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            self.writer.log(
                &log::Record::builder()
                    .level(log::Level::Warn)
                    .target(module_path!())
                    .args(format_args!(
                        "{dropped} log lines dropped: the log was not being written"
                    ))
                    .build(),
            );
        }
    }

    fn write(&self, msg: Msg) {
        match msg {
            Msg::Line(line) => self.writer.log(
                &log::Record::builder()
                    .level(line.level)
                    .target(line.target())
                    .args(format_args!("{}", line.message()))
                    .build(),
            ),
            Msg::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Makes `writer` voxl's log, written by a thread of its own. A program that embeds voxl and
/// has set a logger keeps its own.
pub fn init(writer: env_logger::Logger) {
    let max = writer.filter();
    let (logger, writer) = queue(writer, QUEUED);
    std::thread::Builder::new()
        .name("log".into())
        .spawn(move || writer.run())
        .expect("the log thread");
    // Refused, the logger is dropped, and with it the log thread's queue: the thread ends.
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(max);
    }
}

/// Ends the process once every line logged before is written.
pub fn exit(code: i32) -> ! {
    log::logger().flush();
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// What a writer wrote: a line each, level, target and message.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn writer(sink: &Sink) -> env_logger::Logger {
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .format(|buf, r| {
                use std::io::Write as _;
                writeln!(buf, "{} {} {}", r.level(), r.target(), r.args())
            })
            .write_style(env_logger::WriteStyle::Never)
            .target(env_logger::Target::Pipe(Box::new(sink.clone())))
            .build()
    }

    fn info(logger: &Logger, message: &str) {
        logger.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .target("voxl::video")
                .args(format_args!("{message}"))
                .build(),
        );
    }

    /// The writer's text for `message`, logged by `info`.
    fn written(message: &str) -> String {
        format!("INFO voxl::video {message}\n")
    }

    fn dropped(n: usize) -> String {
        format!("WARN voxl::logging {n} log lines dropped: the log was not being written\n")
    }

    /// A stream thread logs while nothing writes the log: in bursts, into a queue whose
    /// writer is stopped, past the point where the queue is full, and the writer runs only
    /// between some bursts. Nothing consumes while a burst is logged, so a log call that
    /// waited for room could never return: the thread finishing is the proof that none
    /// did. Each time the writer runs it writes what the model of a bounded queue says:
    /// the lines it kept, in order, then how many it dropped since the last count; and
    /// once the loggers are gone, the log thread's loop writes what is left and ends.
    #[test]
    fn a_thread_that_logs_never_waits_for_the_log() {
        let mut rng = Xorshift::new(0x5EED_1065);
        for _ in 0..300 {
            let slots = rng.range(1, 48) as usize;
            let sink = Sink::default();
            let (logger, writer) = queue(writer(&sink), slots);
            let logger = Arc::new(logger);
            let (mut queued, mut lost) = (VecDeque::new(), 0);
            let mut model = String::new();
            let mut n = 0;
            for _ in 0..rng.range(1, 8) {
                let burst: Vec<String> = (n..n + rng.range(0, 3 * slots as u64) as usize)
                    .map(|i| format!("frame {i} encode {} us", i * 7))
                    .collect();
                n += burst.len();
                for m in &burst {
                    match queued.len() < slots {
                        true => queued.push_back(written(m)),
                        false => lost += 1,
                    }
                }
                let to = logger.clone();
                std::thread::Builder::new()
                    .name("capture".into())
                    .spawn(move || burst.iter().for_each(|m| info(&to, m)))
                    .unwrap()
                    .join()
                    .unwrap();
                if rng.range(0, 3) > 0 {
                    writer.drain();
                    model.extend(queued.drain(..));
                    if lost > 0 {
                        model += &dropped(lost);
                        lost = 0;
                    }
                }
                assert_eq!(sink.text(), model, "{slots} slots");
            }
            // With every logger gone the log thread writes what is left, and ends.
            drop(logger);
            writer.run();
            model.extend(queued);
            if lost > 0 {
                model += &dropped(lost);
            }
            assert_eq!(sink.text(), model, "{slots} slots, at the end");
        }
    }

    /// With the log thread running, `flush` returns once every line logged before it is
    /// written, so a line logged before the process exits is in the log.
    #[test]
    fn a_flushed_log_holds_every_line_logged_before() {
        let mut rng = Xorshift::new(0xF1u64 << 32 | 0x05);
        for _ in 0..200 {
            let slots = rng.range(1, 32) as usize;
            let sink = Sink::default();
            let (logger, writer) = queue(writer(&sink), slots);
            let thread = std::thread::spawn(move || writer.run());
            let mut model = String::new();
            for i in 0..rng.range(0, slots as u64 + 1) {
                let m = format!("line {i}");
                info(&logger, &m);
                model += &written(&m);
            }
            logger.flush();
            assert_eq!(sink.text(), model, "{slots} slots");
            drop(logger);
            thread.join().unwrap();
        }
    }

    /// A line is kept whole when its target and message fit in `LINE` bytes. A longer one
    /// keeps the longest run of whole characters that leaves room for `CUT`, then `CUT`:
    /// in the message, or, if the target alone is that long, in place of the message.
    #[test]
    fn a_line_is_kept_whole_or_cut_at_a_character_and_marked() {
        fn model(target: &str, message: &str) -> (String, String) {
            let whole = format!("{target}{message}");
            if whole.len() <= LINE {
                return (target.into(), message.into());
            }
            let mut kept = String::new();
            for c in whole.chars() {
                if kept.len() + c.len_utf8() > LINE - CUT.len() {
                    break;
                }
                kept.push(c);
            }
            let t = target.len().min(kept.len());
            (kept[..t].into(), format!("{}{CUT}", &kept[t..]))
        }
        const CHARS: [char; 6] = ['a', ' ', 'é', '€', '…', '😀'];
        let mut rng = Xorshift::new(0xC07_u64 << 20 | 1);
        let text = |rng: &mut Xorshift, most: u64| -> String {
            (0..rng.range(0, most))
                .map(|_| CHARS[rng.range(0, CHARS.len() as u64) as usize])
                .collect()
        };
        for _ in 0..100_000 {
            // Long enough that a target alone is sometimes cut.
            let target = text(&mut rng, 140);
            let message = text(&mut rng, 160);
            let line = Line::of(
                &log::Record::builder()
                    .level(log::Level::Warn)
                    .target(&target)
                    .args(format_args!("{message}"))
                    .build(),
            );
            assert_eq!(
                (line.target().to_string(), line.message().to_string()),
                model(&target, &message),
                "{target:?} {message:?}"
            );
            assert_eq!(line.level, log::Level::Warn);
        }
    }
}
