//! voxl: low-latency desktop streaming for the browser, from an X11 display on Linux. The
//! daemon codes the desktop as AV1 and sends it, with its audio, over a session's lane;
//! `voxl attach` (run over ssh) is a session's control stream; `voxl relay` on the viewer's
//! machine serves the page and joins the two. The daemon runs on Linux alone; elsewhere
//! `voxl` is the relay, and the daemon's platform-free core is built there for its tests.
//!
//! The `voxl` binary calls [`run`] with its command line; other programs (the Tembo CLI's
//! `tembo voxl`) embed it the same way.

#[cfg(target_os = "linux")]
mod audio;
#[cfg(any(target_os = "linux", test))]
mod av1;
#[cfg(target_os = "linux")]
mod capture;
#[cfg(any(target_os = "linux", test))]
mod convert;
#[cfg(target_os = "linux")]
mod desktop;
mod env;
#[cfg(any(target_os = "linux", test))]
mod event;
mod files;
#[cfg(any(target_os = "linux", test))]
mod help;
#[cfg(any(target_os = "linux", test))]
mod histogram;
#[cfg(any(target_os = "linux", test))]
mod hub;
#[cfg(target_os = "linux")]
mod input;
mod logging;
#[cfg(target_os = "linux")]
mod mic;
#[cfg(any(target_os = "linux", test))]
mod mp4;
#[cfg(any(target_os = "linux", test))]
mod record;
mod relay;
#[cfg(target_os = "linux")]
mod screen;
#[cfg(any(target_os = "linux", test))]
mod session;
#[cfg(any(target_os = "linux", test))]
mod sound;
#[cfg(any(target_os = "linux", test))]
mod video;
pub mod wire;
#[cfg(test)]
mod xorshift;

#[cfg(any(target_os = "linux", test))]
use std::collections::HashMap;

/// A daemon flag as `voxl --help` says it: its name, the value it has when it is not given
/// (parsed as a given one is), and what it is.
#[cfg(any(target_os = "linux", test))]
struct Flag {
    name: &'static str,
    default: &'static str,
    says: &'static str,
}

/// A daemon flag, by which it is: a match over them names every one.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
enum Which {
    /// The folder of the daemon's sockets and its microphone FIFO, which `voxl attach` and
    /// the queries take too: the one the host makes at boot.
    Runtime,
    /// A 144 DPI desktop, apps drawn at 1.5x, shows windows as big as the viewer's: a
    /// choice, set to 2 for a retina viewer.
    ResizeScale,
    AudioEnabled,
}

#[cfg(any(target_os = "linux", test))]
impl Which {
    /// What `--help` says of it.
    const fn flag(self) -> Flag {
        match self {
            Which::Runtime => Flag {
                name: "runtime",
                default: "/run/voxl",
                says: "the folder of the sockets and the microphone FIFO",
            },
            Which::ResizeScale => Flag {
                name: "resize-scale",
                default: "1.5",
                says: "desktop pixels per CSS pixel of the viewer's window",
            },
            Which::AudioEnabled => Flag {
                name: "audio-enabled",
                default: "true",
                says: "desktop audio, true or false (alone, true)",
            },
        }
    }
}

/// The daemon's flags, every one it reads: anything else on its command line is a typed
/// error at startup, not a flag quietly ignored. Production sets the runtime folder and
/// the scale; the bench turns the desktop's audio off where no sound server runs.
#[cfg(any(target_os = "linux", test))]
const FLAGS: [Which; 3] = [Which::Runtime, Which::ResizeScale, Which::AudioEnabled];

/// The words `--audio-enabled` takes, and what each is: a word that is none of these is no
/// value of it.
#[cfg(any(target_os = "linux", test))]
const SWITCH: [(&str, session::control::Audio); 2] = [
    ("true", session::control::Audio::Sent),
    ("false", session::control::Audio::Off),
];

/// What a command line gives a flag: a value (`--name=value`, `--name value`), or nothing
/// (the flag alone, before another flag or at the end).
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, PartialEq)]
enum Given {
    Alone,
    Value(String),
}

#[cfg(any(target_os = "linux", test))]
impl std::fmt::Display for Given {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Given::Alone => write!(f, "alone"),
            Given::Value(v) => write!(f, "{v:?}"),
        }
    }
}

/// A command line the daemon or a subcommand does not take.
#[derive(Debug, PartialEq)]
#[cfg(any(target_os = "linux", test))]
enum FlagError {
    /// A word that is no `--flag`.
    NotAFlag(String),
    /// A flag no one reads.
    Unknown(String),
    /// A flag given a value it cannot take, or given alone when it takes a value.
    Malformed {
        flag: &'static str,
        value: Given,
        takes: &'static str,
    },
}

#[cfg(any(target_os = "linux", test))]
impl std::fmt::Display for FlagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlagError::NotAFlag(w) => write!(f, "{w:?}: not a flag"),
            FlagError::Unknown(k) => write!(f, "--{k}: no such flag (voxl --help lists them)"),
            FlagError::Malformed { flag, value, takes } => {
                write!(f, "--{flag} {value}: takes {takes}")
            }
        }
    }
}

/// Parses `--key=value` / `--key value` / `--key` alone, each key one of `known`; a key that
/// is not, or a word that is no flag, is the error.
#[cfg(any(target_os = "linux", test))]
fn parse_args(
    args: impl IntoIterator<Item = String>,
    known: &[&'static str],
) -> Result<HashMap<&'static str, Given>, FlagError> {
    let mut m = HashMap::new();
    let args: Vec<String> = args.into_iter().collect();
    let mut i = 0;
    while i < args.len() {
        let Some(k) = args[i].strip_prefix("--") else {
            return Err(FlagError::NotAFlag(args[i].clone()));
        };
        let (k, v) = match (k.split_once('='), args.get(i + 1)) {
            (Some((k, v)), _) => (k, Given::Value(v.to_string())),
            (None, Some(next)) if !next.starts_with("--") => {
                i += 1;
                (k, Given::Value(next.clone()))
            }
            (None, Some(_) | None) => (k, Given::Alone),
        };
        let Some(&known) = known.iter().find(|&&n| n == k) else {
            return Err(FlagError::Unknown(k.to_string()));
        };
        m.insert(known, v);
        i += 1;
    }
    Ok(m)
}

/// What the daemon's flags say, each parsed, given or by its default.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq)]
struct Flags {
    /// The runtime folder, as given: judged when the daemon starts.
    runtime: std::path::PathBuf,
    /// Desktop pixels per CSS pixel of the viewer's window.
    resize: hub::Resize,
    audio: session::control::Audio,
}

#[cfg(any(target_os = "linux", test))]
impl Flags {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Flags, FlagError> {
        let names = FLAGS.map(|f| f.flag().name);
        let mut a = parse_args(args, &names)?;
        Ok(Flags {
            runtime: folder(given(&mut a, Which::Runtime))?,
            resize: scale(given(&mut a, Which::ResizeScale))?,
            audio: switch(given(&mut a, Which::AudioEnabled))?,
        })
    }
}

/// What the command line `a` gave `flag`, taken out of it, or the flag's default when it
/// gave nothing: the one place a default is read, parsed after as a given value is.
#[cfg(any(target_os = "linux", test))]
fn given(a: &mut HashMap<&'static str, Given>, flag: Which) -> (Which, Given) {
    let Flag { name, default, .. } = flag.flag();
    match a.remove(name) {
        Some(given) => (flag, given),
        None => (flag, Given::Value(default.into())),
    }
}

/// A folder's path: any but none.
#[cfg(any(target_os = "linux", test))]
fn folder((flag, given): (Which, Given)) -> Result<std::path::PathBuf, FlagError> {
    let refused = |value| FlagError::Malformed {
        flag: flag.flag().name,
        value,
        takes: "a folder's path",
    };
    match given {
        Given::Alone => Err(refused(Given::Alone)),
        Given::Value(v) => match v.as_bytes() {
            [] => Err(refused(Given::Value(v))),
            [_, ..] => Ok(v.into()),
        },
    }
}

/// Desktop pixels per CSS pixel: a number over 0.
#[cfg(any(target_os = "linux", test))]
fn scale((flag, given): (Which, Given)) -> Result<hub::Resize, FlagError> {
    let refused = |value| FlagError::Malformed {
        flag: flag.flag().name,
        value,
        takes: "a number of pixels over 0",
    };
    match given {
        Given::Alone => Err(refused(Given::Alone)),
        Given::Value(v) => match v.parse::<f64>() {
            Ok(scale) if scale.is_finite() && scale > 0.0 => Ok(hub::Resize { scale }),
            Ok(_) | Err(_) => Err(refused(Given::Value(v))),
        },
    }
}

/// A switch: one of [`SWITCH`]'s words, or alone, which is on.
#[cfg(any(target_os = "linux", test))]
fn switch((flag, given): (Which, Given)) -> Result<session::control::Audio, FlagError> {
    match given {
        Given::Alone => Ok(session::control::Audio::Sent),
        Given::Value(v) => match SWITCH.iter().find(|(word, _)| *word == v) {
            Some((_, audio)) => Ok(*audio),
            None => Err(FlagError::Malformed {
                flag: flag.flag().name,
                value: Given::Value(v),
                takes: "true or false",
            }),
        },
    }
}

/// The runtime folder at `dir`, if it is one this user may trust; else the daemon or its
/// subcommand ends with status 2, saying why.
#[cfg(target_os = "linux")]
fn runtime(dir: std::path::PathBuf) -> session::Runtime {
    session::Runtime::open(dir.clone()).unwrap_or_else(|e| {
        eprintln!("voxl: --runtime {}: {e}", dir.display());
        std::process::exit(2)
    })
}

/// What `voxl --version` prints, on every platform.
fn version_line() -> String {
    format!("voxl {}", env!("CARGO_PKG_VERSION"))
}

/// What the first word of a command line asks for, of this platform's commands. On Linux,
/// a first word that is none of them is the daemon's: the whole command line is its flags.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Command {
    #[cfg(target_os = "linux")]
    Attach,
    #[cfg(target_os = "linux")]
    Stats,
    #[cfg(target_os = "linux")]
    Screenshot,
    #[cfg(target_os = "linux")]
    Record,
    #[cfg(target_os = "linux")]
    Files,
    Relay,
    /// A build is checked by running it: the version answers on every platform.
    #[cfg(not(target_os = "linux"))]
    Version,
}

/// This platform's commands, each by its word.
#[cfg(target_os = "linux")]
const COMMANDS: [(&str, Command); 6] = [
    ("attach", Command::Attach),
    ("stats", Command::Stats),
    ("screenshot", Command::Screenshot),
    ("record", Command::Record),
    ("files", Command::Files),
    ("relay", Command::Relay),
];

/// This platform's commands, each by its word.
#[cfg(not(target_os = "linux"))]
const COMMANDS: [(&str, Command); 3] = [
    ("relay", Command::Relay),
    ("--version", Command::Version),
    ("-V", Command::Version),
];

/// The command `args` begin with, and the arguments after its word, if they begin with one.
fn command(args: &[String]) -> Option<(Command, &[String])> {
    let (word, rest) = args.split_first()?;
    let (_, command) = COMMANDS.iter().find(|(w, _)| w == word)?;
    Some((*command, rest))
}

/// Runs voxl with these command-line arguments (without the program name) until it is
/// told to stop: on Linux the daemon or one of its subcommands, elsewhere the relay alone.
pub fn run(args: impl IntoIterator<Item = String>) {
    let args: Vec<String> = args.into_iter().collect();
    no_core_dumps();
    #[cfg(target_os = "linux")]
    match command(&args) {
        Some((Command::Attach, rest)) => session::attach::run(&control(rest, &[]).0),
        Some((Command::Stats, rest)) => {
            let (control, _) = control(rest, &[]);
            session::query::run(session::query::Query::Stats, &control)
        }
        Some((Command::Screenshot, rest)) => {
            let (control, _) = control(rest, &[]);
            session::query::run(session::query::Query::Screenshot, &control)
        }
        Some((Command::Record, rest)) => {
            use session::query::{Query, Recording};
            let (control, mut a) = control(rest, &[SECONDS]);
            let until = match a.remove(SECONDS) {
                None => Recording::UntilClosed,
                Some(Given::Value(s)) => match s.parse::<std::num::NonZeroU32>() {
                    Ok(s) => Recording::Seconds(s),
                    Err(_) => seconds_refused(),
                },
                Some(Given::Alone) => seconds_refused(),
            };
            session::query::run(Query::Record(until), &control)
        }
        Some((Command::Files, rest)) => files::run(rest),
        Some((Command::Relay, rest)) => relay::run(rest),
        None => daemon(args),
    }
    #[cfg(not(target_os = "linux"))]
    match command(&args) {
        Some((Command::Relay, rest)) => relay::run(rest),
        Some((Command::Version, _)) => println!("{}", version_line()),
        None => {
            eprintln!("voxl: here only `voxl relay ...`: the daemon runs on Linux");
            std::process::exit(2)
        }
    }
}

/// `voxl record --seconds` given what is not a number of seconds: it ends with status 2.
#[cfg(target_os = "linux")]
fn seconds_refused() -> ! {
    eprintln!("voxl record: --seconds: a whole number of seconds over 0");
    std::process::exit(2)
}

/// `voxl record`'s flag beside the runtime folder: how long it records.
#[cfg(target_os = "linux")]
const SECONDS: &str = "seconds";

/// A subcommand's flags, the runtime folder's and `also`, and the daemon's control socket in
/// the runtime folder they name (`--runtime`, the daemon's default when not given): a flag
/// it does not know, an empty path, or a folder it may not trust ends it with status 2.
#[cfg(target_os = "linux")]
fn control(
    args: &[String],
    also: &[&'static str],
) -> (std::path::PathBuf, HashMap<&'static str, Given>) {
    let known = [&[Which::Runtime.flag().name], also].concat();
    let parsed = parse_args(args.iter().cloned(), &known)
        .and_then(|mut a| Ok((folder(given(&mut a, Which::Runtime))?, a)));
    let (dir, a) = parsed.unwrap_or_else(|e| {
        eprintln!("voxl: {e}");
        std::process::exit(2)
    });
    (runtime(dir).control(), a)
}

/// No core dump is written of this process, of any of its subcommands: a dump of the
/// daemon or of `voxl attach` would hold a session's capability.
fn no_core_dumps() {
    let none = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit reads the struct it is given and nothing else.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) } != 0 {
        eprintln!(
            "voxl: core dumps cannot be turned off ({}): a dump could hold a capability",
            std::io::Error::last_os_error()
        );
        std::process::exit(1)
    }
}

/// Runs the daemon with these flags until it is told to stop.
#[cfg(target_os = "linux")]
fn daemon(args: Vec<String>) {
    // --help and --version print and return instead of starting the server.
    if help::print_if_asked(&args) {
        return;
    }
    let flags = Flags::parse(args).unwrap_or_else(|e| {
        eprintln!("voxl: {e}");
        std::process::exit(2)
    });
    let runtime = runtime(flags.runtime.clone());
    let read = |name: &str| std::env::var_os(name);
    let (env, desktop) = match (env::Env::read(read), env::Desktop::read(read)) {
        (Ok(env), Ok(desktop)) => (env, desktop),
        (Err(e), Ok(_) | Err(_)) | (Ok(_), Err(e)) => {
            eprintln!("voxl: {e}");
            std::process::exit(2)
        }
    };
    logging::init(env.log.logger());
    // A panic on any thread ends the process, as in voxl's own release build (panic = "abort"),
    // rather than leaving the server up without that thread when the embedder unwinds.
    #[cfg(panic = "unwind")]
    {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            hook(info);
            std::process::abort();
        }));
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            log::error!("the daemon's runtime: {e}");
            logging::exit(1)
        });
    rt.block_on(serve(flags, runtime, desktop));
}

/// Where the shared folder's panel opens and uploads go, under the root: made on the
/// first upload.
#[cfg(target_os = "linux")]
const UPLOAD_DIR: &str = "Downloads";

/// The longest recording `voxl record` makes. Provisional: five minutes of a screen's
/// video, upstream's bound since recordings came, unmeasured; what would measure it is
/// the memory a recording of the busiest screen holds at its end.
#[cfg(target_os = "linux")]
const MAX_RECORD: std::time::Duration = std::time::Duration::from_secs(300);

#[cfg(target_os = "linux")]
async fn serve(flags: Flags, runtime: session::Runtime, env: env::Desktop) {
    let display = env.display.clone();
    // The desktop follows the viewer's window: an X server that cannot change its size is
    // no desktop voxl serves. Every hello names the codec of its largest picture.
    let (screen, codec) = match screen::Screen::open(&display) {
        Ok(s) => {
            log::info!("dynamic resolution on, up to {}x{}", s.max().0, s.max().1);
            let (width, height) = s.max();
            let largest = av1::Shape { width, height };
            match av1::Stream::new(largest, std::num::NonZeroUsize::MIN) {
                Ok(stream) => (s, stream.codec()),
                Err(e) => {
                    log::error!("no AV1 level holds the largest desktop, {:?}", e.0);
                    logging::exit(1)
                }
            }
        }
        Err(e) => {
            log::error!("dynamic resolution: {e}");
            logging::exit(1)
        }
    };
    let audio = flags.audio;
    // Canonical, and text: the hello names it to the page, and the relay hands it back.
    let root = std::fs::canonicalize(&env.home)
        .map_err(|e| e.to_string())
        .and_then(|p| {
            p.into_os_string()
                .into_string()
                .map_err(|p| format!("{p:?} is not text"))
        });
    let root = root.unwrap_or_else(|e| {
        log::error!("the shared folder, $HOME {}: {e}", env.home.display());
        logging::exit(1);
    });
    let files = hub::FilesConfig {
        root,
        dir: UPLOAD_DIR.into(),
    };
    log::info!("file transfer under {}", files.root);
    let hello = hub::Hello {
        codec,
        startup: hub::Startup {
            audio,
            files,
            resize: flags.resize,
        },
    };
    let hub = hub::Hub::new(hello).unwrap_or_else(|e| {
        log::error!("the hub's waker: {e}");
        logging::exit(1)
    });
    // The cores this process may run on; one if the kernel will not say.
    let cores = std::thread::available_parallelism().unwrap_or_else(|e| {
        log::error!("the cores this daemon may use: {e}");
        logging::exit(1)
    });
    let vcfg = video::VideoConfig::new(display.clone(), env.debug, cores);
    if let Err(e) = video::spawn(vcfg, hub.clone(), screen) {
        log::error!("video init failed: {e}");
        logging::exit(1);
    }
    let clipboard = desktop::spawn(display.clone(), hub.clone()).unwrap_or_else(|e| {
        log::error!("cursor and clipboard: {e}");
        logging::exit(1)
    });
    let finished = match input::spawn(&display, clipboard, runtime.keymap()) {
        Ok(event::Input { events, finished }) => {
            *hub.input.lock() = Some(events);
            finished
        }
        Err(e) => {
            log::error!("input: {e}");
            logging::exit(1)
        }
    };
    if audio == session::control::Audio::Sent {
        audio::spawn(hub.clone());
    }
    let mic = mic::spawn(hub.clone(), runtime.mic());
    let daemon = std::sync::Arc::new(session::control::Daemon {
        hub: hub.clone(),
        audio,
        mic,
        max_record: MAX_RECORD,
    });
    tokio::spawn(stop_on_signal(hub.clone(), finished));
    if let Err(e) = session::serve(&runtime.sockets(), daemon).await {
        log::error!("{e}");
        logging::exit(1);
    }
}

/// Ends the daemon on SIGTERM or SIGINT, once the input thread has let go of every key
/// and button it holds down: its input ends with the hub's sender, and it says when it
/// has finished.
#[cfg(target_os = "linux")]
async fn stop_on_signal(
    hub: std::sync::Arc<hub::Hub>,
    finished: tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::signal::unix::{signal, SignalKind};
    let (term, int) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    );
    let (mut term, mut int) = match (term, int) {
        (Ok(term), Ok(int)) => (term, int),
        (Err(e), _) | (_, Err(e)) => {
            log::error!("the daemon's stop signals: {e}");
            logging::exit(1)
        }
    };
    tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
    log::info!("shutting down");
    drop(hub.input.lock().take());
    // The input thread says so once every key is up; gone, it holds none.
    if finished.await.is_err() {
        log::info!("the input thread had ended, holding no key");
    }
    logging::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A command line is flags the daemon reads and nothing else: each known flag, in each
    /// of its three spellings, parses to what it was given; any other flag (generated names,
    /// the known ones changed by a character) and any word that is no flag are refused.
    #[test]
    fn a_flag_is_one_the_daemon_reads() {
        let names = FLAGS.map(|f| f.flag().name);
        for f in names {
            for (args, want) in [
                (vec![format!("--{f}=v")], Given::Value("v".into())),
                (vec![format!("--{f}"), "v".into()], Given::Value("v".into())),
                (vec![format!("--{f}")], Given::Alone),
                (
                    vec![format!("--{f}"), format!("--{}", names[0])],
                    Given::Alone,
                ),
            ] {
                let got = parse_args(args.clone(), &names).map(|mut m| m.remove(f));
                assert_eq!(got, Ok(Some(want)), "{args:?}");
            }
        }
        let mut rng = Xorshift::new(0x428a_2f98_d728_ae22);
        let letters = b"abcdefghijklmnopqrstuvwxyz-=";
        for _ in 0..20_000 {
            let mut name: Vec<u8> = match rng.below(2) {
                0 => names[rng.below(names.len() as u64) as usize]
                    .as_bytes()
                    .to_vec(),
                1.. => Vec::new(),
            };
            let at = rng.below(name.len() as u64 + 1) as usize;
            name.insert(at, letters[rng.below(letters.len() as u64) as usize]);
            let name = String::from_utf8(name).unwrap();
            let key = match name.split_once('=') {
                Some((key, _)) => key,
                None => &name,
            };
            let flag = format!("--{name}");
            match names.contains(&key) {
                true => assert!(parse_args([flag.clone()], &names).is_ok(), "{flag}"),
                false => assert!(parse_args([flag.clone()], &names).is_err(), "{flag}"),
            }
            assert!(parse_args([name.clone()], &names).is_err(), "{name}");
        }
    }

    /// With no flag, each takes its default, which is parsed as a given value is; and a
    /// flag given its default reads the same.
    #[test]
    fn absent_flags_take_their_defaults() {
        let none = Flags::parse([]).expect("the defaults parse");
        assert_eq!(
            none,
            Flags {
                runtime: "/run/voxl".into(),
                resize: hub::Resize { scale: 1.5 },
                audio: session::control::Audio::Sent,
            }
        );
        let given = FLAGS.map(|f| format!("--{}={}", f.flag().name, f.flag().default));
        assert_eq!(Flags::parse(given), Ok(none));
    }

    /// Every flag given a value it cannot take, or given alone when it takes a value, stops
    /// the daemon with the error naming the flag and what it was given, and one it can take
    /// is read as it says: over generated values for each flag (words, numbers whole and
    /// not, of either sign, the empty value) and the flag alone, against each flag's own
    /// rule written apart.
    #[test]
    fn a_malformed_flag_is_refused_naming_it() {
        use session::control::Audio;
        let mut rng = Xorshift::new(0x1f83_d9ab_fb41_bd6b);
        let pieces = [
            "", "-", "0", "1", "2.5", ".", "e3", "inf", "NaN", "true", "false", "x", " ",
        ];
        for _ in 0..20_000 {
            let v: String = (0..rng.below(4))
                .map(|_| pieces[rng.below(pieces.len() as u64) as usize])
                .collect();
            for (which, given) in FLAGS
                .into_iter()
                .flat_map(|f| [(f, Given::Value(v.clone())), (f, Given::Alone)])
            {
                let name = which.flag().name;
                let got = Flags::parse(match &given {
                    Given::Value(v) => vec![format!("--{name}={v}")],
                    Given::Alone => vec![format!("--{name}")],
                });
                let takes = match (which, &given) {
                    (Which::Runtime, Given::Value(v)) => !v.is_empty(),
                    (Which::ResizeScale, Given::Value(v)) => {
                        v.parse::<f64>().is_ok_and(|s| s.is_finite() && s > 0.0)
                    }
                    (Which::AudioEnabled, Given::Value(v)) => v == "true" || v == "false",
                    (Which::Runtime | Which::ResizeScale, Given::Alone) => false,
                    (Which::AudioEnabled, Given::Alone) => true,
                };
                match (got, takes) {
                    (Ok(flags), true) => {
                        let read = match (which, &given) {
                            (Which::Runtime, Given::Value(v)) => flags.runtime == *v,
                            (Which::ResizeScale, Given::Value(v)) => {
                                v.parse() == Ok(flags.resize.scale)
                            }
                            (Which::AudioEnabled, Given::Value(v)) => {
                                (flags.audio == Audio::Sent) == (v == "true")
                            }
                            (Which::AudioEnabled, Given::Alone) => flags.audio == Audio::Sent,
                            (Which::Runtime | Which::ResizeScale, Given::Alone) => {
                                unreachable!("--{name} alone is refused")
                            }
                        };
                        assert!(read, "--{name} {given}");
                    }
                    (Err(FlagError::Malformed { flag, value, .. }), false) => {
                        assert_eq!((flag, value), (name, given))
                    }
                    (got @ Ok(_), false)
                    | (
                        got @ Err(
                            FlagError::Malformed { .. }
                            | FlagError::NotAFlag(_)
                            | FlagError::Unknown(_),
                        ),
                        true,
                    )
                    | (got @ Err(FlagError::NotAFlag(_) | FlagError::Unknown(_)), false) => {
                        panic!("--{name} {given}: {got:?}")
                    }
                }
            }
        }
    }
}
