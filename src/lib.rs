//! voxl: low-latency desktop streaming (H.264 over WebSocket) for the browser, from an
//! X11 display on Linux or the screen of a Mac or a Windows machine.
//!
//! The `voxl` binary calls [`run`] with its command line; other programs (the Tembo CLI's
//! `tembo voxl`) embed the server the same way.

mod access;
#[cfg(all(target_os = "linux", feature = "pulseaudio"))]
mod audio;
#[cfg(target_os = "linux")]
mod capture;
mod clipboard;
mod convert;
#[cfg(target_os = "linux")]
mod corners;
#[cfg(target_os = "linux")]
mod desktop;
mod encoder;
mod env;
mod event;
mod files;
mod help;
mod hub;
#[cfg(target_os = "linux")]
mod input;
mod logging;
mod login;
#[cfg(target_os = "macos")]
mod mac;
mod mic;
mod mp4;
mod path;
mod rate;
mod record;
#[cfg(target_os = "linux")]
mod screen;
mod server;
mod timeline;
mod tls;
mod video;
mod wait;
#[cfg(windows)]
mod win;
#[allow(dead_code)]
mod window;
#[cfg(test)]
mod xorshift;

// The macOS and Windows backends provide the same modules with the same interfaces.
#[cfg(target_os = "macos")]
use mac::{audio, capture, desktop, input, screen};
#[cfg(windows)]
use win::{audio, capture, desktop, input, screen};

use std::collections::HashMap;

/// Parses `--key=value` / `--key value` / `--flag`; unknown options are accepted so the
/// binary can take the same argument list as the Python selkies service it replaces.
fn parse_args(args: impl IntoIterator<Item = String>) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let args: Vec<String> = args.into_iter().collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(k) = a.strip_prefix("--") {
            if let Some((k, v)) = k.split_once('=') {
                m.insert(k.replace('_', "-"), v.to_string());
            } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                m.insert(k.replace('_', "-"), args[i + 1].clone());
                i += 1;
            } else {
                m.insert(k.replace('_', "-"), "true".into());
            }
        }
        i += 1;
    }
    m
}

/// Selkies ranges look like "60-60"; take the upper bound.
fn range_max(v: Option<&String>, default: f64) -> f64 {
    v.and_then(|s| s.rsplit('-').next()?.trim().parse().ok())
        .unwrap_or(default)
}

/// A rate flag's value in whole kbit/s: a number, or a Selkies range of two numbers from
/// low to high, whose upper bound is the rate. Anything else is no rate.
fn whole_kbps(v: &str) -> Option<u32> {
    let whole = |s: &str| s.trim().parse::<u32>().ok();
    match v.split_once('-') {
        None => whole(v),
        Some((low, high)) => whole(low)
            .zip(whole(high))
            .and_then(|(low, high)| (low <= high).then_some(high)),
    }
}

/// A rate flag in whole kbit/s; `default` when it is not given.
fn rate_flag(flag: &str, v: Option<&String>, default: u32) -> Result<u32, String> {
    let Some(v) = v else {
        return Ok(default);
    };
    whole_kbps(v).ok_or_else(|| {
        format!("--{flag} {v:?}: not a whole number of kbit/s, or a range of two from low to high")
    })
}

/// `--preset`: a preset x264 has, `ultrafast` when not given. x264 is asked here, where
/// the flag is read, so a name it does not have starts nothing.
fn preset(flag: Option<&String>) -> Result<encoder::Preset, String> {
    let name = flag.map_or("ultrafast", String::as_str);
    encoder::Preset::parse(name).ok_or_else(|| format!("--preset {name}: x264 has no such preset"))
}

/// The most frames a second when `--framerate` is not given, or is not a number:
/// upstream's default.
const FRAMERATE: encoder::Fps = encoder::Fps::new(60).unwrap();

/// `--framerate` (or a Selkies range, whose upper bound is the rate): the most frames a
/// second, 1 to `Fps::MAX`. Under one (0, or a fraction of one) the capture loop would
/// space its frames an endless time apart; from 2^30 up, four frames' worth of the rate
/// no longer fits the VBV buffer's arithmetic. Both stopped the server; now neither
/// starts it.
fn framerate(v: Option<&String>) -> Result<encoder::Fps, String> {
    let Some(v) = v else {
        return Ok(FRAMERATE);
    };
    encoder::Fps::new(range_max(Some(v), FRAMERATE.get() as f64) as u32).ok_or_else(|| {
        format!(
            "--framerate {v:?}: not 1 to {} frames a second",
            encoder::Fps::MAX
        )
    })
}

/// `--min-kbps` and `--max-kbps` (or Selkies' `--video-bitrate`), read together: what the
/// stream is steered between; 2000 and 30000 kbit/s when not given. A ceiling of 0 is
/// none: the target goes as high as the viewers' paths carry, and encoders open without
/// a rate cap.
fn bounds(min: Option<&String>, max: Option<&String>) -> Result<rate::Bounds, String> {
    let floor = rate_flag("min-kbps", min, 2000)?;
    let ceiling = rate_flag("max-kbps", max, 30000)?;
    let kbps = |kbps: u32| path::Kbps::new(kbps as f64);
    Ok(rate::Bounds::new(
        kbps(floor),
        (ceiling > 0).then(|| kbps(ceiling)),
    ))
}

pub(crate) fn truthy(v: Option<&String>, default: bool) -> bool {
    v.map(|s| matches!(s.as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(default)
}

/// Runs the server with these command-line flags (without the program name) until it's
/// told to stop. Takes over the calling thread, which on macOS has to be the main thread.
pub fn run(args: impl IntoIterator<Item = String>) {
    let args: Vec<String> = args.into_iter().collect();
    // --help and --version print and return instead of starting the server.
    if help::print_if_asked(&args) {
        return;
    }
    let a = parse_args(args);
    let env = env::Env::read(|name| std::env::var_os(name));
    // What env_logger::Builder::from_env does with RUST_LOG and RUST_LOG_STYLE, from Env.
    let mut log = env_logger::Builder::new();
    log.parse_filters(&env.log.filter);
    if let Some(style) = &env.log.style {
        log.parse_write_style(style);
    }
    // x264 says what VOXL_X264_INFO asks of it (its own level), and all of that is written.
    log.filter_module("x264", log::LevelFilter::Debug);
    logging::init(log.build());
    if let Some(refused) = &env.refused {
        log::error!("{refused}");
    }
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
        .expect("tokio runtime");
    // AppKit and the keyboard layout APIs want the main thread, so on macOS it runs the
    // main dispatch queue and the server runs beside it.
    #[cfg(target_os = "macos")]
    {
        std::thread::spawn(move || rt.block_on(serve(a, env)));
        dispatch2::dispatch_main();
    }
    #[cfg(not(target_os = "macos"))]
    rt.block_on(serve(a, env));
}

async fn serve(a: HashMap<String, String>, env: env::Env) {
    // macOS: --display picks a screen (index or display ID); DISPLAY there belongs to XQuartz.
    #[cfg(target_os = "linux")]
    let display = env.display.or_else(|| a.get("display").cloned());
    #[cfg(not(target_os = "linux"))]
    let display = a.get("display").cloned();
    #[cfg(target_os = "macos")]
    mac::init(&a);
    #[cfg(windows)]
    win::init(&a);
    // A Mac's or a PC's own screen is only served locally unless --addr says otherwise;
    // a VM's X11 display sits behind the platform's proxy.
    let default_addr = if cfg!(not(target_os = "linux")) {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    let addr = a
        .get("addr")
        .cloned()
        .unwrap_or_else(|| default_addr.into());
    let port: u16 = a.get("port").and_then(|p| p.parse().ok()).unwrap_or(6901);
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4) as u32;
    let max = a.get("max-kbps").or(a.get("video-bitrate"));
    let bounds = bounds(a.get("min-kbps"), max).unwrap_or_else(|e| {
        log::error!("{e}");
        logging::exit(1);
    });
    let preset = preset(a.get("preset")).unwrap_or_else(|e| {
        log::error!("{e}");
        logging::exit(1);
    });
    let fps = framerate(a.get("framerate")).unwrap_or_else(|e| {
        log::error!("{e}");
        logging::exit(1);
    });
    let vcfg = video::VideoConfig {
        display: display.clone(),
        #[cfg(target_os = "macos")]
        enable_resize: truthy(a.get("enable-resize"), false) && !truthy(a.get("read-only"), false),
        fps,
        preset,
        // Four sliced threads: lower latency than one, and x264's slicing overhead grows
        // with thread count, so more threads only add CPU (measured on 2560x1600).
        threads: a
            .get("encoder-threads")
            .and_then(|v| v.parse().ok())
            .unwrap_or((ncpu / 4).clamp(1, 4)),
        convert_threads: a
            .get("convert-threads")
            .and_then(|v| v.parse().ok())
            .unwrap_or(4),
        crf: range_max(a.get("crf"), 24.0) as f32,
        bounds,
        paintover_crf: range_max(a.get("paintover-crf"), 16.0) as f32,
        paintover_delay_ms: range_max(a.get("paintover-delay-ms"), 150.0) as u64,
        stripes: range_max(a.get("stripes"), 1.0) as u32,
        coalesce: std::time::Duration::from_micros(range_max(a.get("coalesce-us"), 0.0) as u64),
        debug: env.debug,
    };
    let clip = a
        .get("enable-clipboard")
        .map(|s| s.as_str())
        .unwrap_or("true");
    let clip_mode = desktop::ClipboardMode {
        to_server: matches!(clip, "true" | "in"),
        to_client: matches!(clip, "true" | "out"),
    };
    let audio_on = truthy(a.get("audio-enabled"), true);
    #[cfg(all(target_os = "linux", not(feature = "pulseaudio")))]
    let audio_on = audio_on && {
        log::info!("desktop audio off: this voxl was built without PulseAudio");
        false
    };

    let hub = hub::Hub::new();
    // --read-only: the viewer watches the desktop, it does not drive it. Only the
    // commands watching needs are accepted (see server::READ_ONLY_COMMANDS), uploads
    // are refused, and the page hides the controls that would send anything else.
    let read_only = truthy(a.get("read-only"), false);
    // --enable-resize (as in selkies): clients resize the desktop to their window. Not
    // for read-only viewers: a new size changes the desktop for everyone.
    let enable_resize = truthy(a.get("enable-resize"), false);
    if enable_resize && read_only {
        log::warn!("--enable-resize is ignored with --read-only: viewers can't resize the desktop");
    }
    let screen = (enable_resize && !read_only)
        .then(|| match screen::Screen::open(display.as_deref()) {
            Ok(s) => {
                log::info!("dynamic resolution on, up to {}x{}", s.max().0, s.max().1);
                Some(s)
            }
            Err(e) => {
                log::warn!("dynamic resolution unavailable: {e}");
                None
            }
        })
        .flatten();
    let resize_on = screen.is_some();
    // Desktop pixels per CSS pixel of the viewer's window. The default matches a
    // 144 DPI desktop (apps drawn at 1.5x), so windows look as big as in the viewer;
    // on a Mac, the capture's pixels per point.
    #[cfg(target_os = "linux")]
    let default_scale = 1.5;
    #[cfg(target_os = "macos")]
    let default_scale = mac::capture_scale();
    // On Windows, the display's scaling (150% -> 1.5).
    #[cfg(windows)]
    let default_scale = win::dpi_scale();
    let resize_scale = range_max(a.get("resize-scale"), default_scale);
    if let Err(e) = video::spawn(vcfg, hub.clone(), screen) {
        log::error!("video init failed: {e}");
        logging::exit(1);
    }
    let radius = range_max(a.get("round-corners"), 0.0) as u16;
    // --round-windows=title:radius,...: round specific windows (docks included) by title.
    let named: Vec<(String, u16)> = a
        .get("round-windows")
        .map(|v| {
            v.split(',')
                .filter_map(|e| {
                    e.rsplit_once(':')
                        .and_then(|(n, r)| Some((n.to_string(), r.parse().ok()?)))
                })
                .collect()
        })
        .unwrap_or_default();
    if radius > 0 || !named.is_empty() {
        #[cfg(target_os = "linux")]
        if let Err(e) = corners::spawn(display.clone(), radius, named, env.debug) {
            log::warn!("rounded corners unavailable: {e}");
        }
        #[cfg(not(target_os = "linux"))]
        log::info!("--round-corners/--round-windows only apply to X11 desktops");
    }
    let clipboard = match desktop::spawn(display.clone(), hub.clone(), clip_mode) {
        Ok(h) => Some(h),
        Err(e) => {
            log::error!("cursor/clipboard init failed: {e}");
            None
        }
    };
    let px_per_notch = range_max(a.get("scroll-px-per-notch"), 180.0);
    match input::spawn(display.clone(), clipboard, px_per_notch, env.runtime_dir) {
        Ok(tx) => *hub.input.lock() = Some(tx),
        Err(e) => log::error!("input init failed: {e}"),
    }
    #[cfg(any(not(target_os = "linux"), feature = "pulseaudio"))]
    if audio_on {
        let dev = a
            .get("audio-device-name")
            .cloned()
            .unwrap_or_else(|| "output.monitor".into());
        let br = range_max(a.get("audio-bitrate"), 128000.0) as i32;
        audio::spawn(dev, br, hub.clone());
    }

    // --file-transfers=upload,download (as in selkies); "none" turns both off.
    let transfers = a
        .get("file-transfers")
        .cloned()
        .unwrap_or_else(|| "upload,download".into());
    let root = a
        .get("files-root")
        .cloned()
        .or(env.home)
        .unwrap_or_else(|| "/".into());
    let root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.into());
    let files = std::sync::Arc::new(files::Files {
        root,
        // Uploading writes to the desktop, so a read-only viewer does not get it.
        upload: !read_only && transfers.split(',').any(|t| t.trim() == "upload"),
        download: transfers.split(',').any(|t| t.trim() == "download"),
        default_dir: a
            .get("upload-dir")
            .cloned()
            .unwrap_or_else(|| "Downloads".into())
            .trim_matches('/')
            .to_string(),
        max_upload_bytes: (range_max(a.get("max-upload-mb"), 0.0) * 1048576.0) as u64,
    });
    log::info!("file transfer: {transfers} under {}", files.root.display());
    // --microphone-enabled (as in selkies): browser microphone -> a FIFO that a
    // PulseAudio module-pipe-source reads as the VM's microphone, or on macOS an
    // output device that loops back to an input (BlackHole and the like).
    #[cfg(target_os = "linux")]
    let mic_sink = Some(
        a.get("microphone-fifo")
            .cloned()
            .unwrap_or_else(|| "/tmp/voxl-mic.fifo".into()),
    );
    #[cfg(target_os = "macos")]
    let mic_sink = truthy(a.get("microphone-enabled"), false)
        .then(|| mac::mic::find_device(a.get("microphone-device").map(|s| s.as_str())))
        .flatten();
    // Windows: a virtual audio cable's playback side (VB-CABLE and the like).
    #[cfg(windows)]
    let mic_sink = truthy(a.get("microphone-enabled"), false)
        .then(|| win::mic::find_device(a.get("microphone-device").map(|s| s.as_str())))
        .flatten();
    let mic = truthy(a.get("microphone-enabled"), false)
        .then_some(())
        .and(mic_sink)
        .map(|sink| mic::spawn(sink, hub.clone()));
    hub.settings.lock().start(
        hub::Startup {
            audio: audio_on,
            files: hub::FilesConfig {
                dir: files.default_dir.clone(),
                download: files.download,
                upload: files.upload,
            },
            mic: mic.is_some(),
            os: std::env::consts::OS,
            read_only,
        },
        resize_on.then_some(hub::Resize {
            scale: resize_scale,
        }),
    );
    // --favicon: the tab's icon (default: voxl's own).
    let (favicon, favicon_type) = server::favicon(a.get("favicon").map(|s| s.as_str()))
        .unwrap_or_else(|e| {
            log::error!("{e}");
            logging::exit(1);
        });
    let state = server::AppState {
        hub: hub.clone(),
        audio: audio_on,
        files,
        mic,
        resize: enable_resize && !read_only,
        read_only,
        // GET /screenshot, /record; --max-record-seconds caps a recording's length.
        recorder: record::Recorder::new(
            hub.clone(),
            std::time::Duration::from_secs_f64(range_max(a.get("max-record-seconds"), 300.0)),
        ),
        // --title: the browser tab's title.
        index: server::index_html(
            a.get("title")
                .map_or("Voxl Computer Stream", |t| t.as_str()),
        ),
        favicon,
        favicon_type,
        debug: env.debug,
    };
    // --allowed-hosts and --allowed-origins: the host names and other sites voxl answers
    // besides IP addresses, localhost and its own page (see access.rs).
    let access = std::sync::Arc::new(access::Access::new(
        a.get("allowed-hosts").map(|s| s.as_str()),
        a.get("allowed-origins").map(|s| s.as_str()),
    ));
    // The login (see login.rs): a password, given or made up at startup, unless
    // --enable-basic-auth=false.
    let login = login::Login::from_args(&a, env.password)
        .map(|(l, generated)| (std::sync::Arc::new(l), generated));
    let mut app = server::router(state);
    if let Some((l, _)) = &login {
        app = app.layer(axum::middleware::from_fn_with_state(
            l.clone(),
            login::check,
        ));
    }
    // Added last, so it runs first: a request for the wrong host or from another site's
    // page is turned away before the login is asked for.
    let app = app.layer(axum::middleware::from_fn_with_state(access, access::check));
    // HTTPS (see tls.rs). A self-signed certificate names this machine, the bound address
    // and the --allowed-hosts names.
    let tls = {
        let mut names = vec!["localhost".to_string(), "127.0.0.1".into(), "::1".into()];
        names.extend(env.host_names);
        if !matches!(
            addr.as_str(),
            "0.0.0.0" | "::" | "127.0.0.1" | "::1" | "localhost"
        ) {
            names.push(addr.clone());
        }
        for h in a.get("allowed-hosts").map_or("", |s| s.as_str()).split(',') {
            let h = h.trim();
            if !h.is_empty() && h != "*" {
                names.push(
                    h.strip_prefix('.')
                        .map_or(h.to_string(), |d| format!("*.{d}")),
                );
            }
        }
        names.sort();
        names.dedup();
        tls::from_args(&a, names, &env.tls_dir)
    };
    let scheme = if tls.is_some() { "https" } else { "http" };
    let listener = tokio::net::TcpListener::bind((addr.as_str(), port))
        .await
        .expect("bind");
    log::info!("listening on {scheme}://{addr}:{port}");
    // Where to point a browser on this machine.
    let page = match addr.as_str() {
        "0.0.0.0" | "::" => format!("{scheme}://localhost:{port}/"),
        a if a.contains(':') => format!("{scheme}://[{a}]:{port}/"),
        a => format!("{scheme}://{a}:{port}/"),
    };
    match &login {
        Some((l, true)) => log::info!(
            "open {page}?token={} to sign in (the password is {0}; choose one with --basic-auth-password or VOXL_PASSWORD)",
            l.password()
        ),
        Some((_, false)) => log::info!(
            "login on: sign in at {page} with the password, or open {page}?token=<password>"
        ),
        None => log::info!("login off (--enable-basic-auth=false): anyone who reaches {page} can use this desktop"),
    }
    let shutdown_hub = hub.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).unwrap();
            let mut int = signal(SignalKind::interrupt()).unwrap();
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        }
        #[cfg(windows)]
        let _ = tokio::signal::ctrl_c().await;
        log::info!("shutting down");
        if let Some(tx) = shutdown_hub.input.lock().as_ref() {
            let _ = tx.send(event::InputEvent::Shutdown);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        logging::exit(0);
    });
    if let Some(config) = tls {
        return tls::serve(listener, app, config).await;
    }
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use path::Kbps;
    use rate::Bounds;

    /// A preset is one x264 has, asked where the flag is read. Each of x264's presets
    /// (x264.h's `x264_preset_names`), by its name, in capitals and by its number, starts
    /// voxl and opens an encoder; each name with a letter added or its first taken away
    /// starts nothing; and no flag is `ultrafast`.
    #[test]
    fn a_preset_is_one_x264_has() {
        const X264: [&str; 10] = [
            "ultrafast",
            "superfast",
            "veryfast",
            "faster",
            "fast",
            "medium",
            "slow",
            "slower",
            "veryslow",
            "placebo",
        ];
        for (i, name) in X264.iter().enumerate() {
            for given in [name.to_string(), name.to_uppercase(), i.to_string()] {
                let preset = preset(Some(&given)).unwrap_or_else(|e| panic!("{e}"));
                let cfg = encoder::EncoderConfig {
                    pictures: encoder::Pictures {
                        width: 64,
                        height: 64,
                        fps: encoder::Fps::new(60).unwrap(),
                        preset,
                        threads: 1,
                        debug: env::Debugging::read(|_| None).0,
                    },
                    crf: 24.0,
                    max_kbps: None,
                };
                assert!(encoder::Encoder::new(&cfg).is_ok(), "{given}");
            }
            for not in [format!("{name}x"), name[1..].to_string()] {
                assert!(preset(Some(&not)).is_err(), "{not}");
            }
        }
        assert_eq!(
            preset(None),
            Ok(encoder::Preset::parse("ultrafast").unwrap())
        );
    }

    fn read(min: Option<&str>, max: Option<&str>) -> Result<Bounds, String> {
        bounds(
            min.map(String::from).as_ref(),
            max.map(String::from).as_ref(),
        )
    }

    /// `--max-kbps` is a ceiling or none, decided where the flags are read: 0 is none, and
    /// what is not a whole number of kbit/s starts nothing.
    #[test]
    fn the_ceiling_is_a_rate_or_none() {
        let at = |kbps: f64| Ok(Some(Kbps::new(kbps)));
        for (flag, want) in [
            (None, at(30000.0)),
            (Some("0"), Ok(None)),
            (Some("12000"), at(12000.0)),
            (Some(" 12000 "), at(12000.0)),
            (Some("8000-12000"), at(12000.0)),
            (Some("0-0"), Ok(None)),
        ] {
            assert_eq!(read(None, flag).map(Bounds::ceiling), want, "{flag:?}");
        }
        for flag in [
            "",
            "fast",
            "12k",
            "1.5",
            "NaN",
            "inf",
            "8000-",
            "99999999999",
            "true",
        ] {
            assert!(read(None, Some(flag)).is_err(), "{flag:?} parsed");
        }
    }

    /// A range is two whole numbers from low to high. A leading `-` is no range with
    /// nothing before it (`--max-kbps -5` reaches the parse: it does not start with `--`),
    /// a lower bound is read, and a range has two ends. (RED's finding 19.)
    #[test]
    fn a_ceiling_that_is_not_a_rate_starts_nothing() {
        for flag in ["-5", "-0", "fast-12000", "1-2-3", "12000-8000"] {
            assert!(
                read(None, Some(flag)).is_err(),
                "{flag:?} parsed as {:?}",
                read(None, Some(flag))
            );
        }
    }

    /// `--min-kbps` is read the same way, 2000 when not given, and with the ceiling into
    /// one value: a ceiling under the floor brings the floor down to it.
    #[test]
    fn the_floor_is_read_with_the_ceiling() {
        let kbps = Kbps::new;
        let default = Some(kbps(30000.0));
        for (min, max, want) in [
            (None, None, Bounds::new(kbps(2000.0), default)),
            (Some("0"), None, Bounds::new(kbps(0.0), default)),
            (Some("1000-3000"), None, Bounds::new(kbps(3000.0), default)),
            (Some("5000"), Some("0"), Bounds::new(kbps(5000.0), None)),
            (
                None,
                Some("1000"),
                Bounds::new(kbps(1000.0), Some(kbps(1000.0))),
            ),
            (
                Some("9000"),
                Some("4000"),
                Bounds::new(kbps(4000.0), Some(kbps(4000.0))),
            ),
        ] {
            assert_eq!(
                read(min, max),
                Ok(want),
                "--min-kbps {min:?} --max-kbps {max:?}"
            );
        }
        for min in ["", "fast", "1.5", "-5", "3000-1000"] {
            assert!(read(Some(min), None).is_err(), "--min-kbps {min:?} parsed");
        }
    }

    /// `--framerate` reads as it did wherever that was 1 to 240 frames a second, and starts
    /// nothing anywhere else: over whole and fractional numbers, ranges, a leading `-`,
    /// words, and numbers of every size a u32 holds and past it.
    #[test]
    fn a_framerate_outside_1_to_240_starts_nothing() {
        let mut rng = crate::xorshift::Xorshift::new(0x2545F4914F6CDD1D);
        let mut draw = |n: u64| rng.below(n);
        for _ in 0..20_000 {
            let v = match draw(6) {
                0 => format!("{}", draw(300)),
                1 => format!("{}.{}", draw(3), draw(100)),
                2 => format!("{}-{}", draw(200), draw(300)),
                3 => format!("-{}.{}", draw(3), draw(10)),
                4 => {
                    let bits = draw(40);
                    format!("{}", draw(1 << bits))
                }
                _ => ["", "fast", "NaN", "inf", "-0", "0-0", " 0 ", "0.0"][draw(8) as usize].into(),
            };
            let before = range_max(Some(&v), 60.0) as u32;
            assert_eq!(
                framerate(Some(&v)).map(encoder::Fps::get),
                match before {
                    1..=240 => Ok(before),
                    _ => Err(format!("--framerate {v:?}: not 1 to 240 frames a second")),
                },
                "{v:?}"
            );
        }
        assert_eq!(framerate(None).map(encoder::Fps::get), Ok(60));
    }

    /// Every frame rate `--framerate` lets through is one an encoder opens at under a cap,
    /// and sets a new cap at: over the edges (240 and 241; 2^30 - 1 and 2^30, where four
    /// frames' worth of the rate leaves a u32; u32::MAX and past it) and over rates drawn
    /// across every magnitude a u32 holds.
    #[test]
    fn every_framerate_read_opens_an_encoder() {
        let mut rng = crate::xorshift::Xorshift::new(0x9E3779B97F4A7C15);
        let mut draw = |n: u64| rng.below(n);
        let edges = [
            "240",
            "241",
            "1073741823",
            "1073741824",
            "4294967295",
            "4294967296",
            "1e12",
        ];
        let drawn: Vec<String> = (0..200)
            .map(|_| {
                let bits = draw(33);
                draw(1 << bits).to_string()
            })
            .collect();
        for v in edges.into_iter().map(String::from).chain(drawn) {
            let Ok(fps) = framerate(Some(&v)) else {
                continue;
            };
            let mut enc = encoder::Encoder::new(&encoder::EncoderConfig {
                pictures: encoder::Pictures {
                    width: 64,
                    height: 64,
                    fps,
                    preset: encoder::Preset::parse("ultrafast").unwrap(),
                    threads: 1,
                    debug: env::Debugging::read(|_| None).0,
                },
                crf: 24.0,
                max_kbps: Some(Kbps::new(2000.0)),
            })
            .unwrap_or_else(|e| panic!("--framerate {v:?}: {e}"));
            enc.set_rate(24.0, 3000);
        }
    }
}
