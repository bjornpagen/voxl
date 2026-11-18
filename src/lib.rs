//! voxl: low-latency desktop streaming for the browser, from an X11 display on Linux. The
//! daemon codes the desktop as VP9 and sends it, with its audio, over a session's lanes;
//! `voxl attach` (run over ssh) is a session's control stream; `voxl relay` on the viewer's
//! machine serves the page and joins the two. The daemon runs on Linux alone; elsewhere
//! `voxl` is the relay, and the daemon's platform-free core is built there for its tests.
//!
//! The `voxl` binary calls [`run`] with its command line; other programs (the Tembo CLI's
//! `tembo voxl`) embed it the same way.

#[cfg(target_os = "linux")]
mod audio;
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
mod hub;
#[cfg(target_os = "linux")]
mod input;
pub mod lanes;
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
#[cfg(any(target_os = "linux", test))]
mod vp9;
#[cfg(test)]
mod xorshift;

#[cfg(any(target_os = "linux", test))]
use std::collections::HashMap;

/// A daemon flag: its name, the value it has when it is not given (parsed as a given one
/// is), and what it is, as `voxl --help` says.
#[cfg(any(target_os = "linux", test))]
struct Flag {
    name: &'static str,
    default: &'static str,
    says: &'static str,
}

/// The daemon's flags, every one it reads: anything else on its command line is a typed
/// error at startup, not a flag quietly ignored. Production sets the runtime folder and
/// the scale; the bench turns the desktop's audio off where no sound server runs.
#[cfg(any(target_os = "linux", test))]
const FLAGS: [Flag; 3] = [RUNTIME, RESIZE_SCALE, AUDIO_ENABLED];

/// The folder of the daemon's sockets and its microphone FIFO, which `voxl attach` and
/// the queries take too: the one the host makes at boot.
#[cfg(any(target_os = "linux", test))]
const RUNTIME: Flag = Flag {
    name: "runtime",
    default: "/run/voxl",
    says: "the folder of the sockets and the microphone FIFO",
};

/// A 144 DPI desktop, apps drawn at 1.5x, shows windows as big as the viewer's: a
/// choice, set to 2 for a retina viewer.
#[cfg(any(target_os = "linux", test))]
const RESIZE_SCALE: Flag = Flag {
    name: "resize-scale",
    default: "1.5",
    says: "desktop pixels per CSS pixel of the viewer's window",
};

#[cfg(any(target_os = "linux", test))]
const AUDIO_ENABLED: Flag = Flag {
    name: "audio-enabled",
    default: "true",
    says: "desktop audio, true or false",
};

/// A command line the daemon or a subcommand does not take.
#[derive(Debug, PartialEq)]
#[cfg(any(target_os = "linux", test))]
enum FlagError {
    /// A word that is no `--flag`.
    NotAFlag(String),
    /// A flag no one reads.
    Unknown(String),
    /// A flag given a value it cannot take.
    Malformed {
        flag: &'static str,
        value: String,
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
                write!(f, "--{flag} {value:?}: takes {takes}")
            }
        }
    }
}

/// Parses `--key=value` / `--key value` / `--flag` (a flag alone is `true`), each key one
/// of `known`; a key that is not, or a word that is no flag, is the error.
#[cfg(any(target_os = "linux", test))]
fn parse_args(
    args: impl IntoIterator<Item = String>,
    known: &[&'static str],
) -> Result<HashMap<&'static str, String>, FlagError> {
    let mut m = HashMap::new();
    let args: Vec<String> = args.into_iter().collect();
    let mut i = 0;
    while i < args.len() {
        let Some(k) = args[i].strip_prefix("--") else {
            return Err(FlagError::NotAFlag(args[i].clone()));
        };
        let (k, v) = match k.split_once('=') {
            Some((k, v)) => (k, v.to_string()),
            None if i + 1 < args.len() && !args[i + 1].starts_with("--") => {
                i += 1;
                (k, args[i].clone())
            }
            None => (k, "true".into()),
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
    /// Desktop pixels per CSS pixel of the viewer's window: finite, over 0.
    resize_scale: f64,
    audio: session::control::Audio,
}

#[cfg(any(target_os = "linux", test))]
impl Flags {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Flags, FlagError> {
        let names = FLAGS.map(|f| f.name);
        let given = parse_args(args, &names)?;
        let value = |flag: &Flag| -> (&'static str, &str) {
            (
                flag.name,
                given.get(flag.name).map_or(flag.default, String::as_str),
            )
        };
        Ok(Flags {
            runtime: folder(value(&RUNTIME))?,
            resize_scale: {
                let (flag, v) = value(&RESIZE_SCALE);
                v.parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s > 0.0)
                    .ok_or_else(|| FlagError::Malformed {
                        flag,
                        value: v.into(),
                        takes: "a number of pixels over 0",
                    })?
            },
            audio: {
                use session::control::Audio;
                match value(&AUDIO_ENABLED) {
                    (_, "true") => Audio::Sent,
                    (_, "false") => Audio::Off,
                    (flag, v) => {
                        return Err(FlagError::Malformed {
                            flag,
                            value: v.into(),
                            takes: "true or false",
                        })
                    }
                }
            },
        })
    }
}

/// A folder's path: any but none.
#[cfg(any(target_os = "linux", test))]
fn folder((flag, v): (&'static str, &str)) -> Result<std::path::PathBuf, FlagError> {
    match v {
        "" => Err(FlagError::Malformed {
            flag,
            value: v.into(),
            takes: "a folder's path",
        }),
        path => Ok(path.into()),
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

/// Runs voxl with these command-line arguments (without the program name) until it is
/// told to stop: on Linux the daemon or one of its subcommands, elsewhere the relay alone.
pub fn run(args: impl IntoIterator<Item = String>) {
    let args: Vec<String> = args.into_iter().collect();
    no_core_dumps();
    #[cfg(target_os = "linux")]
    match args.first().map(String::as_str) {
        Some("attach") => session::attach::run(&control(&args[1..], &["runtime"]).0),
        Some("stats") => {
            let (control, _) = control(&args[1..], &["runtime"]);
            session::query::run(session::query::Query::Stats, &control)
        }
        Some("screenshot") => {
            let (control, _) = control(&args[1..], &["runtime"]);
            session::query::run(session::query::Query::Screenshot, &control)
        }
        Some("record") => {
            use session::query::{Query, Recording};
            let (control, a) = control(&args[1..], &["runtime", "seconds"]);
            let until = match a.get("seconds").map(|s| s.parse::<std::num::NonZeroU32>()) {
                None => Recording::UntilClosed,
                Some(Ok(s)) => Recording::Seconds(s),
                Some(Err(_)) => {
                    eprintln!("voxl record: --seconds: a whole number of seconds over 0");
                    std::process::exit(2)
                }
            };
            session::query::run(Query::Record(until), &control)
        }
        Some("files") => files::run(&args[1..]),
        Some("relay") => relay::run(&args[1..]),
        _ => daemon(args),
    }
    #[cfg(not(target_os = "linux"))]
    match args.first().map(String::as_str) {
        Some("relay") => relay::run(&args[1..]),
        // A build is checked by running it: the version answers on every platform.
        Some("--version" | "-V") => println!("{}", version_line()),
        _ => {
            eprintln!("voxl: here only `voxl relay ...`: the daemon runs on Linux");
            std::process::exit(2)
        }
    }
}

/// A subcommand's flags, of `known`, and the daemon's control socket in the runtime
/// folder they name (`--runtime`, the daemon's default when not given): a flag it does not
/// know, an empty path, or a folder it may not trust ends it with status 2.
#[cfg(target_os = "linux")]
fn control(
    args: &[String],
    known: &[&'static str],
) -> (std::path::PathBuf, HashMap<&'static str, String>) {
    let parsed = parse_args(args.iter().cloned(), known).and_then(|a| {
        let dir = folder((
            RUNTIME.name,
            a.get(RUNTIME.name).map_or(RUNTIME.default, String::as_str),
        ))?;
        Ok((dir, a))
    });
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
    let env = env::Env::read(|name| std::env::var_os(name));
    let desktop = env::Desktop::read(|name| std::env::var_os(name)).unwrap_or_else(|e| {
        eprintln!("voxl: {e}");
        std::process::exit(2)
    });
    // What env_logger::Builder::from_env does with RUST_LOG and RUST_LOG_STYLE, from Env.
    let mut log = env_logger::Builder::new();
    log.parse_filters(&env.log.filter);
    if let Some(style) = &env.log.style {
        log.parse_write_style(style);
    }
    logging::init(log.build());
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
    let hub = hub::Hub::new().unwrap_or_else(|e| {
        log::error!("the hub's waker: {e}");
        logging::exit(1)
    });
    // The desktop follows the viewer's window where the X server can change its size;
    // where it cannot, the desktop keeps its size, and the viewer is told so.
    let screen = match screen::Screen::open(&display) {
        Ok(s) => {
            log::info!("dynamic resolution on, up to {}x{}", s.max().0, s.max().1);
            let (width, height) = s.max();
            let largest = vp9::Shape {
                width,
                height,
                fps: video::FRAMERATE,
            };
            match vp9::Stream::new(largest, std::num::NonZeroUsize::MIN) {
                Ok(stream) => {
                    let _ = hub.codec.set(stream.codec());
                }
                Err(e) => {
                    log::error!("no VP9 level holds the largest desktop, {:?}", e.0);
                    logging::exit(1)
                }
            }
            Some(s)
        }
        Err(e) => {
            log::warn!("dynamic resolution unavailable, the desktop keeps its size: {e}");
            None
        }
    };
    let resize = match screen {
        Some(_) => session::control::Resizing::Follows,
        None => session::control::Resizing::Fixed,
    };
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
    match input::spawn(&display, clipboard, runtime.keymap()) {
        Ok(tx) => *hub.input.lock() = Some(tx),
        Err(e) => {
            log::error!("input: {e}");
            logging::exit(1)
        }
    }
    let audio = flags.audio;
    if audio == session::control::Audio::Sent {
        audio::spawn(hub.clone());
    }
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
    let mic = mic::spawn(hub.clone(), runtime.mic());
    hub.settings.lock().start(
        hub::Startup {
            audio: audio == session::control::Audio::Sent,
            files,
        },
        (resize == session::control::Resizing::Follows).then_some(hub::Resize {
            scale: flags.resize_scale,
        }),
    );
    let daemon = std::sync::Arc::new(session::control::Daemon {
        hub: hub.clone(),
        audio,
        mic,
        resize,
        max_record: MAX_RECORD,
    });
    tokio::spawn(stop_on_signal(hub.clone()));
    if let Err(e) = session::serve(&runtime.sockets(), daemon).await {
        log::error!("{e}");
        logging::exit(1);
    }
}

/// Ends the daemon on SIGTERM or SIGINT, once the input thread has let go of every key
/// and button it holds down (`InputEvent::Shutdown`, answered when done).
#[cfg(target_os = "linux")]
async fn stop_on_signal(hub: std::sync::Arc<hub::Hub>) {
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
    let released = hub.input.lock().as_ref().map(|tx| {
        let (done, released) = tokio::sync::oneshot::channel();
        (tx.send(event::InputEvent::Shutdown(done)).is_ok()).then_some(released)
    });
    if let Some(Some(released)) = released {
        // The input thread answers once every key is up; gone, it holds none.
        if released.await.is_err() {
            log::info!("the input thread had ended, holding no key");
        }
    }
    logging::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A command line is flags the daemon reads and nothing else: each known flag, in each
    /// of its three spellings, parses to its value; any other flag (generated names, the
    /// known ones changed by a character) and any word that is no flag are refused.
    #[test]
    fn a_flag_is_one_the_daemon_reads() {
        let names = FLAGS.map(|f| f.name);
        for f in names {
            for (args, want) in [
                (vec![format!("--{f}=v")], "v"),
                (vec![format!("--{f}"), "v".into()], "v"),
                (vec![format!("--{f}")], "true"),
            ] {
                let got = parse_args(args.clone(), &names);
                assert_eq!(got.map(|m| m[f].clone()), Ok(want.to_string()), "{args:?}");
            }
        }
        let mut rng = Xorshift::new(0x428a_2f98_d728_ae22);
        let letters = b"abcdefghijklmnopqrstuvwxyz-=";
        for _ in 0..20_000 {
            let mut name: Vec<u8> = match rng.below(2) {
                0 => names[rng.below(names.len() as u64) as usize]
                    .as_bytes()
                    .to_vec(),
                _ => Vec::new(),
            };
            let at = rng.below(name.len() as u64 + 1) as usize;
            name.insert(at, letters[rng.below(letters.len() as u64) as usize]);
            let name = String::from_utf8(name).unwrap();
            let key = name.split('=').next().unwrap_or_default();
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
                resize_scale: 1.5,
                audio: session::control::Audio::Sent,
            }
        );
        let given = FLAGS.map(|f| format!("--{}={}", f.name, f.default));
        assert_eq!(Flags::parse(given), Ok(none));
    }

    /// Every flag given a value it cannot take stops the daemon with the error naming the
    /// flag and the value, and one it can take is read as it says: over generated values
    /// for each flag (words, numbers whole and not, of either sign, the empty value and
    /// the flag alone), against each flag's own rule written apart.
    #[test]
    fn a_malformed_flag_is_refused_naming_it() {
        let mut rng = Xorshift::new(0x1f83_d9ab_fb41_bd6b);
        let pieces = [
            "", "-", "0", "1", "2.5", ".", "e3", "inf", "NaN", "true", "false", "x", " ",
        ];
        for _ in 0..20_000 {
            let v: String = (0..rng.below(4))
                .map(|_| pieces[rng.below(pieces.len() as u64) as usize])
                .collect();
            for f in &FLAGS {
                let got = Flags::parse([format!("--{}={v}", f.name)]);
                let takes = match f.name {
                    "runtime" => !v.is_empty(),
                    "resize-scale" => v.parse::<f64>().is_ok_and(|s| s.is_finite() && s > 0.0),
                    "audio-enabled" => v == "true" || v == "false",
                    other => panic!("--{other}: no rule written for it"),
                };
                match (got, takes) {
                    (Ok(flags), true) => {
                        let read = match f.name {
                            "runtime" => flags.runtime.to_str() == Some(&v),
                            "resize-scale" => Some(flags.resize_scale) == v.parse().ok(),
                            _ => (flags.audio == session::control::Audio::Sent) == (v == "true"),
                        };
                        assert!(read, "--{}={v:?}", f.name);
                    }
                    (Err(FlagError::Malformed { flag, value, .. }), false) => {
                        assert_eq!((flag, value.as_str()), (f.name, v.as_str()))
                    }
                    (got, _) => panic!("--{}={v:?}: {got:?}", f.name),
                }
            }
        }
    }
}
