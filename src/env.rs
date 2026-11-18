//! The environment voxl runs in: the variables that configure it, and the `VOXL_*`
//! variables for looking inside it (its debugging logs, x264's own log, and the switches
//! that turn off x264's hints and reading Xvfb's framebuffer in place). They are read
//! once, when voxl starts, and handed to what each one changes.

use std::ffi::OsString;

/// The environment every part of voxl reads: how it logs.
pub struct Env {
    /// What env_logger logs, and how.
    pub log: Log,
}

/// The environment the daemon reads besides: the desktop it streams and the `VOXL_*`
/// debugging variables. Each is required, or has the meaning its absence states.
#[cfg(any(target_os = "linux", test))]
pub struct Desktop {
    /// The `VOXL_*` debugging variables.
    pub debug: Debugging,
    /// `DISPLAY`: the X display.
    pub display: String,
    /// `HOME`: the shared folder of file transfers.
    pub home: std::path::PathBuf,
}
/// env_logger's two variables, as `env_logger::Builder::from_env` would read them.
pub struct Log {
    /// `RUST_LOG`, else `info`: which records are logged.
    pub filter: String,
    /// `RUST_LOG_STYLE`: whether they are coloured (`always`, `never`, `auto`); unset,
    /// env_logger decides from the terminal.
    pub style: Option<String>,
}

#[cfg(any(target_os = "linux", test))]
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
    /// `VOXL_DEBUG_ENCODES`: every frame's encode is logged.
    pub encodes: bool,
}

#[cfg(any(target_os = "linux", test))]
/// How much x264 logs: warnings; with `VOXL_X264_INFO` set, its summaries; with
/// `VOXL_X264_INFO=debug`, every frame's QP, macroblock types and size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum X264Log {
    Warning,
    Info,
    Debug,
}

#[cfg(any(target_os = "linux", test))]
/// Where on the screen the bench loadgen draws its marker: `VOXL_DEBUG_MARKER=x,y`, in
/// whole pixels from the top left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Marker {
    pub x: usize,
    pub y: usize,
}

#[cfg(any(target_os = "linux", test))]
impl Marker {
    /// `x,y`: two whole numbers around one comma.
    fn parse(at: &str) -> Option<Marker> {
        let (x, y) = at.split_once(',')?;
        Some(Marker {
            x: x.parse().ok()?,
            y: y.parse().ok()?,
        })
    }
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
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl Desktop {
    /// Reads the daemon's variables through `var`, as [`Env::read`] does: a variable it
    /// needs that is unset, or one that holds what it cannot take, is the error, the line
    /// that says so.
    pub fn read(var: impl Fn(&str) -> Option<OsString>) -> Result<Desktop, String> {
        let needed = |name: &str| {
            var(name)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("${name} is not set; the daemon needs it"))
        };
        let text = |name: &str| {
            needed(name)?
                .into_string()
                .map_err(|v| format!("${name}={v:?}: not text"))
        };
        Ok(Desktop {
            debug: Debugging::read(&var)?,
            display: text("DISPLAY")?,
            home: needed("HOME")?.into(),
        })
    }
}

#[cfg(any(target_os = "linux", test))]
impl Debugging {
    /// Reads the debugging variables through `var`, as [`Env::read`] does. A
    /// `VOXL_DEBUG_MARKER` that is not `x,y` is the error, the line that says so: the
    /// marker would be looked for where the loadgen did not draw it.
    pub fn read(var: impl Fn(&str) -> Option<OsString>) -> Result<Debugging, String> {
        let marker = match var("VOXL_DEBUG_MARKER") {
            None => None,
            Some(at) => Some(at.to_str().and_then(Marker::parse).ok_or_else(|| {
                format!(
                    "VOXL_DEBUG_MARKER={at:?}: not where the bench loadgen's marker is, \
                     x,y in whole pixels (its clients look at 300,150 unless told)"
                )
            })?),
        };
        let var = |name: &str| var(name).and_then(|v| v.into_string().ok());
        let set = |name| var(name).is_some();
        let debug = Debugging {
            x264_log: match var("VOXL_X264_INFO").as_deref() {
                None => X264Log::Warning,
                Some("debug") => X264Log::Debug,
                Some(_) => X264Log::Info,
            },
            mb_info: !set("VOXL_NO_MBINFO"),
            direct_fb: !set("VOXL_NO_DIRECT_FB"),
            marker,
            damage: set("VOXL_DEBUG_DAMAGE"),
            encodes: set("VOXL_DEBUG_ENCODES"),
        };
        Ok(debug)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `x,y` as its definition says it: an optional `+` and digits (how Rust reads a whole
    /// number), a comma, the same again, and nothing else.
    fn model(at: &str) -> Option<Marker> {
        let whole = |t: &str| {
            let digits = t.strip_prefix('+').unwrap_or(t);
            (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
                .then(|| digits.parse().unwrap())
        };
        match at.split(',').collect::<Vec<_>>()[..] {
            [x, y] => Some(Marker {
                x: whole(x)?,
                y: whole(y)?,
            }),
            _ => None,
        }
    }

    /// The marker an environment with `VOXL_DEBUG_MARKER=at` gives, or the line refusing
    /// the value.
    fn marker(at: Option<OsString>) -> Result<Option<Marker>, String> {
        Debugging::read(|name| at.clone().filter(|_| name == "VOXL_DEBUG_MARKER")).map(|d| d.marker)
    }

    /// Every value of up to six characters from digits, signs, commas, a space and a
    /// letter puts the marker where the model says, or is refused: none falls back to a
    /// place nobody asked for.
    #[test]
    fn a_marker_is_where_it_says_or_refused() {
        const CHARS: [char; 7] = ['0', '7', '+', '-', ',', ' ', 'x'];
        let mut values = vec![String::new()];
        for _ in 0..6 {
            let longer: Vec<String> = values
                .iter()
                .filter(|v| v.len() == values.last().unwrap().len())
                .flat_map(|v| CHARS.map(|c| format!("{v}{c}")))
                .collect();
            values.extend(longer);
        }
        assert_eq!(values.len(), (7usize.pow(7) - 1) / 6);
        for at in &values {
            match (marker(Some(at.into())), model(at)) {
                (Ok(Some(m)), Some(want)) => assert_eq!(m, want, "{at:?}"),
                (Err(e), None) => assert!(e.starts_with("VOXL_DEBUG_MARKER="), "{e}"),
                (read, want) => panic!("{at:?}: read {read:?}, model {want:?}"),
            }
        }
        assert_eq!(marker(None), Ok(None));
    }

    /// Any place on any screen is read back as it was written, up to the largest whole
    /// number, and one past it is refused.
    #[test]
    fn a_marker_is_read_back() {
        let mut state = 0u64;
        for _ in 0..10_000 {
            // splitmix64, each coordinate cut to a random number of bits
            let mut next = || {
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 31;
                (z >> (z % 64)) as usize
            };
            let (x, y) = (next(), next());
            assert_eq!(
                marker(Some(format!("{x},{y}").into())),
                Ok(Some(Marker { x, y }))
            );
        }
        let max = usize::MAX;
        assert_eq!(
            marker(Some(format!("{max},{max}").into())),
            Ok(Some(Marker { x: max, y: max }))
        );
        assert!(marker(Some(format!("{max}0,1").into())).is_err());
        assert!(marker(Some(format!("1,{max}0").into())).is_err());
    }

    /// A value that is not Unicode is refused, not taken as unset.
    #[test]
    fn a_marker_that_is_not_text_is_refused() {
        let at = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(b"300,150\xff".to_vec())
        };
        assert!(marker(Some(at)).is_err());
    }

    /// The daemon's environment is read as it was set, and one it needs that is unset or
    /// empty, or a display that is not text, is the error naming it: over both, each
    /// unset, empty or set.
    #[test]
    fn the_desktop_needs_its_display_and_home() {
        use std::os::unix::ffi::OsStringExt;
        const NEEDED: [&str; 2] = ["DISPLAY", "HOME"];
        for set in 0..3u32.pow(2) {
            // Each variable unset (0), empty (1) or set (2).
            let state = |i: usize| set / 3u32.pow(i as u32) % 3;
            let value = |name: &str| {
                let i = NEEDED.iter().position(|n| *n == name)?;
                match state(i) {
                    0 => None,
                    1 => Some(OsString::new()),
                    _ => Some(OsString::from(format!("/{name}"))),
                }
            };
            match Desktop::read(value) {
                Ok(d) => {
                    assert!((0..2).all(|i| state(i) == 2));
                    assert_eq!(d.debug, Debugging::read(|_| None).unwrap());
                    assert_eq!(d.display, "/DISPLAY");
                    assert_eq!(d.home, std::path::Path::new("/HOME"));
                }
                Err(e) => {
                    let first = (0..2).find(|&i| state(i) != 2).expect("one is missing");
                    assert!(e.contains(NEEDED[first]), "{e}");
                }
            }
        }
        let bytes = OsString::from_vec(b":1\xff".to_vec());
        let not_text = Desktop::read(|name| match name {
            "DISPLAY" => Some(bytes.clone()),
            "HOME" => Some("/x".into()),
            _ => None,
        });
        assert!(not_text.is_err_and(|e| e.contains("DISPLAY")));
    }
}
