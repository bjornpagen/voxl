//! The environment voxl runs in: the variables that configure it, and the `VOXL_*`
//! variables for looking inside it (its debugging logs, x264's own log, and the switches
//! that turn off x264's hints and reading Xvfb's framebuffer in place). They are read
//! once, when voxl starts, and handed to what each one changes.

use std::ffi::OsString;
use std::path::PathBuf;

/// Every environment variable voxl reads. One that has to be text counts as set when it
/// holds Unicode, as `std::env::var` reads it; a path is taken as it is.
pub struct Env {
    /// What env_logger logs, and how.
    pub log: Log,
    /// The `VOXL_*` debugging variables.
    pub debug: Debugging,
    /// `DISPLAY`: the X display, before `--display`.
    #[cfg(target_os = "linux")]
    pub display: Option<String>,
    /// `HOME`, else `USERPROFILE`: where file transfers are rooted without `--files-root`.
    pub home: Option<String>,
    /// `COMPUTERNAME` and `HOSTNAME`, those set, in lower case: names a certificate voxl
    /// makes gives this machine.
    pub host_names: Vec<String>,
    /// `VOXL_PASSWORD`, else `SELKIES_BASIC_AUTH_PASSWORD`: the password without
    /// `--basic-auth-password`.
    pub password: Option<String>,
    /// Where voxl keeps the certificate it makes (see `tls::tls_dir`).
    pub tls_dir: PathBuf,
    /// `XDG_RUNTIME_DIR`: where the keyboard's scratch file is kept, on Linux.
    pub runtime_dir: Option<String>,
}

/// env_logger's two variables, as `env_logger::Builder::from_env` would read them.
pub struct Log {
    /// `RUST_LOG`, else `info`: which records are logged.
    pub filter: String,
    /// `RUST_LOG_STYLE`: whether they are coloured (`always`, `never`, `auto`); unset,
    /// env_logger decides from the terminal.
    pub style: Option<String>,
}

/// What voxl's `VOXL_*` debugging variables ask for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Debugging {
    /// `VOXL_X264_INFO`: how much x264 logs.
    pub x264_log: X264Log,
    /// x264 takes the damage's per-block hints and skips the blocks that didn't change,
    /// unless `VOXL_NO_MBINFO` is set.
    pub mb_info: bool,
    /// Xvfb's framebuffer is read in place when it can be found, unless
    /// `VOXL_NO_DIRECT_FB` is set.
    pub direct_fb: bool,
    /// `VOXL_DEBUG_MARKER`: the bench loadgen's marker is read off every capture, the
    /// pipeline logs its timings, and a frame's header carries the time it was sent in
    /// place of its latency.
    pub marker: Option<Marker>,
    /// `VOXL_DEBUG_DAMAGE`: every damage rectangle is logged.
    pub damage: bool,
    /// `VOXL_DEBUG_CORNERS`: every window configured, and every change to a window's
    /// corners, is logged.
    pub corners: bool,
    /// `VOXL_DEBUG_STRIPES`: every stripe's encode is logged.
    pub stripes: bool,
}

/// How much x264 logs: warnings; with `VOXL_X264_INFO` set, its summaries; with
/// `VOXL_X264_INFO=debug`, every frame's QP, macroblock types and size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum X264Log {
    Warning,
    Info,
    Debug,
}

/// Where on the screen the bench loadgen draws its marker: `VOXL_DEBUG_MARKER=x,y`,
/// at 300,150 when the value is not two whole numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Marker {
    pub x: usize,
    pub y: usize,
}

impl Env {
    /// Reads every variable through `var`, which gives a variable's value if it is set:
    /// `std::env::var_os` for voxl's own environment.
    pub fn read(var: impl Fn(&str) -> Option<OsString>) -> Env {
        let text = |name: &str| var(name).and_then(|v| v.into_string().ok());
        Env {
            log: Log {
                filter: text("RUST_LOG").unwrap_or_else(|| "info".into()),
                style: text("RUST_LOG_STYLE"),
            },
            debug: Debugging::read(&var),
            #[cfg(target_os = "linux")]
            display: text("DISPLAY"),
            home: text("HOME").or_else(|| text("USERPROFILE")),
            host_names: ["COMPUTERNAME", "HOSTNAME"]
                .into_iter()
                .filter_map(|name| Some(text(name)?.to_ascii_lowercase()))
                .collect(),
            password: text("VOXL_PASSWORD").or_else(|| text("SELKIES_BASIC_AUTH_PASSWORD")),
            tls_dir: crate::tls::tls_dir(&var),
            runtime_dir: text("XDG_RUNTIME_DIR"),
        }
    }
}

impl Debugging {
    /// Reads the debugging variables through `var`, as [`Env::read`] does.
    pub fn read(var: impl Fn(&str) -> Option<OsString>) -> Debugging {
        let var = |name: &str| var(name).and_then(|v| v.into_string().ok());
        let set = |name| var(name).is_some();
        Debugging {
            x264_log: match var("VOXL_X264_INFO").as_deref() {
                None => X264Log::Warning,
                Some("debug") => X264Log::Debug,
                Some(_) => X264Log::Info,
            },
            mb_info: !set("VOXL_NO_MBINFO"),
            direct_fb: !set("VOXL_NO_DIRECT_FB"),
            marker: var("VOXL_DEBUG_MARKER").map(|at| {
                let (x, y) = at
                    .split_once(',')
                    .and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)))
                    .unwrap_or((300, 150));
                Marker { x, y }
            }),
            damage: set("VOXL_DEBUG_DAMAGE"),
            corners: set("VOXL_DEBUG_CORNERS"),
            stripes: set("VOXL_DEBUG_STRIPES"),
        }
    }
}
