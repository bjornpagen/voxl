//! The environment voxl runs in: the variables that configure it, and the `VOXL_*`
//! variables for looking inside it (its debugging logs, x264's own log, and the switches
//! that turn off x264's hints and reading Xvfb's framebuffer in place). They are read
//! once, when voxl starts, and handed to what each one changes.

use std::ffi::OsString;
use std::path::PathBuf;

/// Every environment variable voxl reads. One that has to be text counts as set when it
/// holds Unicode, as `std::env::var` reads it; a path is taken as it is.
#[derive(Debug, PartialEq)]
pub struct Env {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::VarError;

    /// `std::env::var`, as the standard library defines it over `var_os`.
    fn std_var(var_os: &dyn Fn(&str) -> Option<OsString>, name: &str) -> Result<String, VarError> {
        match var_os(name) {
            Some(s) => s.into_string().map_err(VarError::NotUnicode),
            None => Err(VarError::NotPresent),
        }
    }

    /// The debugging variables as each was read where it was used, when it was needed:
    /// the old reads, as they were written.
    fn old_debugging(var: impl Fn(&str) -> Result<String, VarError>) -> Debugging {
        Debugging {
            x264_log: if var("VOXL_X264_INFO").as_deref() == Ok("debug") {
                X264Log::Debug
            } else if var("VOXL_X264_INFO").is_ok() {
                X264Log::Info
            } else {
                X264Log::Warning
            },
            mb_info: var("VOXL_NO_MBINFO").is_err(),
            direct_fb: var("VOXL_NO_DIRECT_FB").is_err(),
            marker: var("VOXL_DEBUG_MARKER").is_ok().then(|| {
                let (x, y) = var("VOXL_DEBUG_MARKER")
                    .ok()
                    .and_then(|s| {
                        s.split_once(',').and_then(|(a, b)| {
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                        })
                    })
                    .unwrap_or((300, 150));
                Marker { x, y }
            }),
            damage: var("VOXL_DEBUG_DAMAGE").is_ok(),
            corners: var("VOXL_DEBUG_CORNERS").is_ok(),
            stripes: var("VOXL_DEBUG_STRIPES").is_ok(),
        }
    }

    /// `tls::tls_dir` as it was written, reading the process's environment.
    fn old_tls_dir(var_os: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
        if let Some(d) = var_os("VOXL_TLS_DIR") {
            return d.into();
        }
        #[cfg(windows)]
        let base = var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        base.unwrap_or_else(|| PathBuf::from(".")).join("voxl")
    }

    /// Every variable as it was read where it was used: in `serve` (DISPLAY, the files
    /// root, the certificate's names), `Login::from_args`, `tls::tls_dir` and the keyboard's
    /// scratch file. What each use site adds to the read (`--display` before DISPLAY, the
    /// files root's `--files-root` and `/`, `/tmp` for the scratch file) stays there.
    fn old_env(var_os: &dyn Fn(&str) -> Option<OsString>) -> Env {
        let var = |name: &str| std_var(var_os, name);
        let mut names = Vec::new();
        for v in ["COMPUTERNAME", "HOSTNAME"] {
            if let Ok(h) = var(v) {
                names.push(h.to_ascii_lowercase());
            }
        }
        Env {
            debug: old_debugging(var),
            #[cfg(target_os = "linux")]
            display: var("DISPLAY").ok(),
            home: var("HOME").ok().or_else(|| var("USERPROFILE").ok()),
            host_names: names,
            password: var("VOXL_PASSWORD")
                .ok()
                .or_else(|| var("SELKIES_BASIC_AUTH_PASSWORD").ok()),
            tls_dir: old_tls_dir(var_os),
            runtime_dir: var("XDG_RUNTIME_DIR").ok(),
        }
    }

    /// `text` followed by a byte that is not Unicode, which `std::env::var` reads as unset
    /// and `std::env::var_os` as it is.
    fn not_unicode(text: &str) -> OsString {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0xff);
            OsString::from_vec(bytes)
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0xd800);
            OsString::from_wide(&wide)
        }
    }

    /// Every debugging variable reads as it did where it was used. Every combination of
    /// values is tried, so no variable is read under another's name.
    #[test]
    fn each_variable_reads_as_it_did_where_it_was_used() {
        const NAMES: [&str; 7] = [
            "VOXL_X264_INFO",
            "VOXL_NO_MBINFO",
            "VOXL_NO_DIRECT_FB",
            "VOXL_DEBUG_MARKER",
            "VOXL_DEBUG_DAMAGE",
            "VOXL_DEBUG_CORNERS",
            "VOXL_DEBUG_STRIPES",
        ];
        // Each of x264's levels, a marker where it says and one at the default.
        const VALUES: [Option<&str>; 6] = [
            None,
            Some(""),
            Some("debug"),
            Some("12,34"),
            Some("12,34,56"),
            Some("x,5"),
        ];
        for n in 0..VALUES.len().pow(NAMES.len() as u32) {
            let value = |name: &str| {
                let i = NAMES.iter().position(|&v| v == name).unwrap();
                VALUES[n / VALUES.len().pow(i as u32) % VALUES.len()].map(String::from)
            };
            let var = |name: &str| value(name).ok_or(VarError::NotPresent);
            assert_eq!(
                Debugging::read(|name| value(name).map(OsString::from)),
                old_debugging(var),
                "{:?}",
                NAMES.map(|name| (name, value(name)))
            );
        }
    }

    /// Every variable voxl reads, read once, is what it was where it was used. The
    /// environments are drawn at random (a fixed seed): each variable unset, empty, set to
    /// its own name (in capitals), or to its own name and a byte that is not Unicode. Any
    /// four variables take each of their 256 combinations of those (missed with odds below
    /// 10^-300 in 200,000 draws), so a variable read under another's name, read as text
    /// where it was a path, or with another's precedence, fails.
    #[test]
    fn every_variable_reads_as_it_did_where_it_was_used() {
        const NAMES: [&str; 20] = [
            "VOXL_X264_INFO",
            "VOXL_NO_MBINFO",
            "VOXL_NO_DIRECT_FB",
            "VOXL_DEBUG_MARKER",
            "VOXL_DEBUG_DAMAGE",
            "VOXL_DEBUG_CORNERS",
            "VOXL_DEBUG_STRIPES",
            "DISPLAY",
            "HOME",
            "USERPROFILE",
            "COMPUTERNAME",
            "HOSTNAME",
            "VOXL_PASSWORD",
            "SELKIES_BASIC_AUTH_PASSWORD",
            "VOXL_TLS_DIR",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_RUNTIME_DIR",
            "RUST_LOG",
            "PATH",
        ];
        let mut state = 0u64;
        for _ in 0..200_000 {
            // splitmix64
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut draw = state;
            draw = (draw ^ (draw >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            draw = (draw ^ (draw >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            draw ^= draw >> 31;
            let lookup = |name: &str| {
                let i = NAMES.iter().position(|&v| v == name)?;
                match draw >> (2 * i) & 3 {
                    0 => None,
                    1 => Some(OsString::new()),
                    2 => Some(OsString::from(name)),
                    _ => Some(not_unicode(name)),
                }
            };
            assert_eq!(
                Env::read(lookup),
                old_env(&lookup),
                "{:?}",
                NAMES.map(|name| (name, lookup(name)))
            );
        }
    }
}
