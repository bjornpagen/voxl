//! The environment variables for looking inside voxl: its debugging logs, x264's own
//! log, and the switches that turn off x264's hints and reading Xvfb's framebuffer in
//! place. They are read once, when voxl starts, and handed to what each one changes.

/// What voxl's `VOXL_*` debugging variables ask for. A variable counts as set when it
/// holds Unicode, as `std::env::var` reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Env {
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
    /// `|name| std::env::var(name).ok()` for voxl's own environment.
    pub fn read(var: impl Fn(&str) -> Option<String>) -> Env {
        let set = |name| var(name).is_some();
        Env {
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

    /// Every variable reads as it did where it was used, when each was read from the
    /// process's environment at the moment it was needed: the old reads, as they were
    /// written, are the oracle. Every combination of values is tried, so no variable is
    /// read under another's name.
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
            let oracle = Env {
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
            };
            assert_eq!(
                Env::read(value),
                oracle,
                "{:?}",
                NAMES.map(|name| (name, value(name)))
            );
        }
    }
}
