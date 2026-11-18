//! `--help` and `--version`: printed instead of starting the daemon.

const USAGE: &str = "
Usage: voxl [--flag=value]...
       voxl attach [--runtime DIR]
       voxl stats [--runtime DIR]           the pipeline's counts and each viewer, as JSON
       voxl screenshot [--runtime DIR]      the screen as a PNG on stdout
       voxl record [--seconds N] [--runtime DIR]
                                            an MP4 on stdout, of N seconds or until
                                            stdin ends
       voxl files list|get|put ...
       voxl relay ...

Runs the daemon: streams the X display in $DISPLAY to the sessions that attach to it. A
session is `voxl attach` run over ssh (its control stream) and the lanes its relay opens
through a proxy (video and audio); the daemon listens on unix sockets only. Flags take
--name=value or --name value; a flag alone is true.

Flags:
";

const END: &str = "
  -h, --help                    print this and exit
  -V, --version                 print the version and exit
";

/// What `--help` prints.
fn help(version: &str) -> String {
    format!(
        "voxl {version}: low-latency desktop streaming for the browser.\n{USAGE}{}{END}",
        flags()
    )
}

/// The daemon's flags as `--help` lists them: each with what it is and its default.
fn flags() -> String {
    crate::FLAGS
        .iter()
        .map(|f| format!("  --{:<28}{} (default {})\n", f.name, f.says, f.default))
        .collect()
}

/// Which of the two the arguments ask for, if either. Help wins over version.
#[derive(Debug, PartialEq)]
enum Request {
    Help,
    Version,
}

fn request(args: &[String]) -> Option<Request> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        Some(Request::Help)
    } else if args.iter().any(|a| a == "--version" || a == "-V") {
        Some(Request::Version)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
/// Prints the help or the version if the arguments ask for either, and says whether it did.
pub fn print_if_asked(args: &[String]) -> bool {
    let version = env!("CARGO_PKG_VERSION");
    match request(args) {
        Some(Request::Help) => {
            print!("{}", help(version));
            true
        }
        Some(Request::Version) => {
            println!("voxl {version}");
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-h` or `--help` anywhere asks for the help, else `-V` or `--version` anywhere for
    /// the version, else neither: over generated command lines of those, the daemon's
    /// flags, near misses of them, and values.
    #[test]
    fn help_wins_then_version_then_the_daemon() {
        let mut rng = crate::xorshift::Xorshift::new(0x6a09_e667_f3bc_c908);
        let words = [
            "-h",
            "--help",
            "-V",
            "--version",
            "--runtime",
            "/run/voxl",
            "-help",
            "--helpx",
            "-v",
            "--Version",
            "help",
            "version",
            "--resize-scale=2",
            "-",
            "--",
        ];
        for _ in 0..20_000 {
            let args: Vec<String> = (0..rng.below(6))
                .map(|_| words[rng.below(words.len() as u64) as usize].to_string())
                .collect();
            let has = |w: &[&str]| args.iter().any(|a| w.contains(&a.as_str()));
            let want = match (has(&["-h", "--help"]), has(&["-V", "--version"])) {
                (true, _) => Some(Request::Help),
                (false, true) => Some(Request::Version),
                (false, false) => None,
            };
            assert_eq!(request(&args), want, "{args:?}");
        }
    }

    /// The help lists every flag the daemon reads, each once, on a line with what it is
    /// and its default, and no flag it does not read.
    #[test]
    fn the_help_lists_every_flag_with_its_default() {
        let text = help("0");
        for f in &crate::FLAGS {
            let lines: Vec<&str> = text
                .lines()
                .filter(|l| l.trim_start().starts_with(&format!("--{} ", f.name)))
                .collect();
            assert_eq!(lines.len(), 1, "--{}", f.name);
            assert!(lines[0].contains(f.says), "{}", lines[0]);
            assert!(
                lines[0].ends_with(&format!("(default {})", f.default)),
                "{}",
                lines[0]
            );
        }
        let listed = text.lines().filter(|l| l.starts_with("  --")).count();
        assert_eq!(listed, crate::FLAGS.len());
    }
}
