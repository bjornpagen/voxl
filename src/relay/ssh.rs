//! The relay's one way into the server: `ssh <host> voxl …`, over whatever ssh's own
//! configuration makes of `<host>` (its master connection, its keys). ssh carries the
//! control stream (`voxl attach`, to the daemon whose runtime folder the relay names, or
//! to the daemon's default one) and each file transfer (`voxl files …`).

use std::fmt;
use std::process::Stdio;
use tokio::process::{Child, Command};

/// The ssh destination (`--ssh`): a word ssh takes as its destination, never as an option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination(String);

impl Destination {
    pub fn parse(v: &str) -> Result<Destination, String> {
        let word = !v.is_empty() && !v.starts_with('-') && v.chars().all(|c| c.is_ascii_graphic());
        match word {
            true => Ok(Destination(v.to_string())),
            false => Err(format!(
                "--ssh {v:?}: not an ssh destination (one word of printable ASCII, not starting with -)"
            )),
        }
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A daemon's runtime folder on the server (`--runtime`), which `voxl attach` is given as
/// it is: a path the daemon takes, any but none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runtime(String);

/// Which daemon on the server `voxl attach` reaches: the one in the server's own default
/// folder, which only the server's `voxl` knows, so attach is told nothing; or the one whose
/// folder `--runtime` named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Daemon {
    Default,
    In(Runtime),
}

impl Runtime {
    pub fn parse(v: &str) -> Result<Runtime, String> {
        match v.as_bytes() {
            [] => Err("--runtime \"\": not a folder's path".to_string()),
            [_, ..] => Ok(Runtime(v.to_string())),
        }
    }

    /// The flag `voxl attach` reads it from, in the one spelling that takes any value
    /// whole, one beginning with `--` included.
    fn flag(&self) -> String {
        format!("--runtime={}", self.0)
    }
}

/// `arg` as one word of a POSIX shell's command line: ssh hands the server's login shell
/// one string, so every argument is quoted whole. Inside single quotes nothing is special
/// but the quote itself, which is closed, escaped, and opened again.
fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// `voxl <args…>` as the one string ssh hands the server's login shell.
fn line(args: &[&str]) -> String {
    let mut line = String::from("voxl");
    for a in args {
        line.push(' ');
        line.push_str(&quote(a));
    }
    line
}

/// `ssh <to> voxl <args…>`, its three pipes the relay's, killed when dropped.
fn voxl(to: &Destination, args: &[&str]) -> Command {
    let line = line(args);
    let mut c = Command::new("ssh");
    c.arg(&to.0)
        .arg(line)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    c
}

/// ssh's own exit when it fails, as ssh(1) publishes it ("EXIT STATUS": ssh exits with the
/// exit status of the remote command or with 255 if an error occurred): it could not
/// connect, or could not authenticate.
const SSH_FAILED: i32 = 255;

/// How `ssh … voxl attach` ended before the server's voxl sent the capability.
#[derive(Debug, PartialEq, Eq)]
pub enum Unopened {
    /// ssh itself failed. The relay's ssh never logs in on its own: it rides a connection
    /// the user opens in a terminal (the door), and without one it cannot authenticate
    /// (it has no terminal to ask on), or the door's connection is gone.
    NoDoor,
    /// Anything else: the remote command's own exit, or a signal.
    Exited(std::process::ExitStatus),
}

impl Unopened {
    /// The cause of an exit that came before the capability, by its status alone.
    pub fn of(status: std::process::ExitStatus) -> Unopened {
        match status.code() {
            Some(SSH_FAILED) => Unopened::NoDoor,
            // A code of its own, or none: a signal ended it.
            Some(i32::MIN..SSH_FAILED | SSH_FAILED..) | None => Unopened::Exited(status),
        }
    }
}

/// `voxl attach`'s arguments, for `daemon`.
fn attach_args(daemon: &Daemon) -> Vec<String> {
    match daemon {
        Daemon::Default => vec!["attach".to_string()],
        Daemon::In(runtime) => vec!["attach".to_string(), runtime.flag()],
    }
}

/// `voxl attach` on the server, to `daemon`: the session's control stream on its stdin and
/// stdout. Its stderr is the relay's, so what ssh says (a key to touch, a host key) reaches
/// the relay's log.
pub fn attach(to: &Destination, daemon: &Daemon) -> std::io::Result<Child> {
    let args = attach_args(daemon);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    voxl(to, &args).stderr(Stdio::inherit()).spawn()
}

/// `voxl files <args…>` on the server.
pub fn files(to: &Destination, args: &[&str]) -> std::io::Result<Child> {
    let mut all = vec!["files"];
    all.extend_from_slice(args);
    voxl(to, &all).spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// The words a POSIX shell reads `line` as, for the shell language the relay writes:
    /// written apart from `quote`, from the standard (XCU 2.2 Quoting, 2.6.5 Field
    /// Splitting): an unquoted blank ends a word; a single quote begins a run that the next
    /// single quote ends, every byte between taken as it is; an unquoted backslash takes the
    /// byte after it as it is. Any other byte the shell treats specially, met unquoted, is a
    /// quoting the relay must never write: the test fails on it.
    fn shell_words(line: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word: Option<String> = None;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' => words.extend(word.take()),
                '\'' => {
                    let w = word.get_or_insert_with(String::new);
                    loop {
                        match chars.next() {
                            Some('\'') => break,
                            Some(q) => w.push(q),
                            None => panic!("{line:?}: a quote never closed"),
                        }
                    }
                }
                '\\' => match chars.next() {
                    Some(e) => word.get_or_insert_with(String::new).push(e),
                    None => panic!("{line:?}: a backslash at the end"),
                },
                'a'..='z' => word.get_or_insert_with(String::new).push(c),
                special => panic!("{line:?}: {special:?} unquoted"),
            }
        }
        words.extend(word);
        words
    }

    /// The server's login shell reads the line the relay sends as `voxl` and the arguments
    /// it means, and the attach's flags as the daemon's parser reads them: over drawn
    /// arguments of every byte a shell word can hold (no NUL; quotes, backslashes, `$`,
    /// backticks, blanks and newlines among them, and letters beyond ASCII), and over drawn
    /// runtime folders (a leading `--` and `=` too) and none. The oracles are the shell's
    /// quoting as the standard states it (`shell_words`) and the daemon's `parse_args`.
    #[test]
    fn the_shell_reads_the_attach_the_relay_means() {
        const SPECIAL: &[char] = &[
            '\'', '"', '\\', '$', '`', '!', '*', '?', '[', ']', '{', '}', '(', ')', '<', '>', '|',
            '&', ';', '#', '~', ' ', '\t', '\n', '-', '=', '%', '/', 'é',
        ];
        let mut rng = Xorshift::new(0xA0761D6478BD642F);
        let draw = |rng: &mut Xorshift, most: u64| -> String {
            (0..rng.below(most))
                .map(|_| match rng.below(3) {
                    0 => SPECIAL[rng.below(SPECIAL.len() as u64) as usize],
                    1 => char::from(b'a' + rng.below(26) as u8),
                    2.. => char::from(1 + rng.below(127) as u8),
                })
                .collect()
        };
        for _ in 0..2_000 {
            let args: Vec<String> = (0..rng.below(4)).map(|_| draw(&mut rng, 24)).collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let read = shell_words(&line(&args));
            assert_eq!(read[0], "voxl");
            assert_eq!(read[1..], args[..]);
        }
        for i in 0..2_000u32 {
            let daemon = match i % 5 {
                0 => Daemon::Default,
                1.. => {
                    let mut s = String::from("x");
                    s.push_str(&draw(&mut rng, 16));
                    if rng.below(6) == 0 {
                        s.insert_str(0, "--");
                    }
                    Daemon::In(Runtime::parse(&s).unwrap())
                }
            };
            let args = attach_args(&daemon);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let words = shell_words(&line(&args));
            assert_eq!(words[..2], ["voxl", "attach"], "{daemon:?}");
            let runtime = crate::Which::Runtime;
            let mut read = crate::parse_args(words[2..].iter().cloned(), &[runtime.flag().name])
                .unwrap_or_else(|e| panic!("{daemon:?}: {e}"));
            let folder = crate::folder(crate::given(&mut read, runtime)).unwrap();
            let want = match &daemon {
                Daemon::Default => runtime.flag().default,
                Daemon::In(runtime) => runtime.0.as_str(),
            };
            assert_eq!(folder, std::path::PathBuf::from(want), "{daemon:?}");
        }
        assert_eq!(
            Runtime::parse(""),
            Err("--runtime \"\": not a folder's path".into())
        );
    }

    /// An exit before the capability is NoDoor exactly when its status is ssh's own failure
    /// (255, ssh(1)); every other code, and death by any signal, is an exit of its own. The
    /// statuses are made as `wait(2)` encodes them (`ExitStatusExt::from_raw`).
    #[test]
    fn only_ssh_s_own_failure_is_no_door() {
        use std::os::unix::process::ExitStatusExt;
        for code in 0..=u8::MAX {
            let status = std::process::ExitStatus::from_raw(i32::from(code) << 8);
            assert_eq!(status.code(), Some(i32::from(code)));
            let got = Unopened::of(status);
            match code {
                255 => assert_eq!(got, Unopened::NoDoor),
                0..=254 => assert_eq!(got, Unopened::Exited(status), "exit {code}"),
            }
        }
        for signal in 1..32 {
            let killed = std::process::ExitStatus::from_raw(signal);
            assert_eq!(killed.signal(), Some(signal));
            assert_eq!(Unopened::of(killed), Unopened::Exited(killed));
        }
    }

    /// A destination is read exactly when ssh would take it as one destination word and
    /// never as an option: over drawn strings of printable ASCII, spaces, tabs, newlines, a
    /// leading `-` and letters beyond ASCII, one is read when it is non-empty, does not
    /// start with `-`, and holds only visible ASCII; and is read as itself.
    #[test]
    fn a_destination_is_never_an_option() {
        let mut rng = Xorshift::new(0x3C6EF372FE94F82B);
        let mut read = 0;
        for _ in 0..50_000 {
            let s: String = (0..rng.below(12))
                .map(|_| match rng.below(10) {
                    0 => '-',
                    1 => [' ', '\t', '\n', 'é', '\u{7f}'][rng.below(5) as usize],
                    2.. => char::from(b'!' + rng.below(94) as u8),
                })
                .collect();
            let word =
                !s.is_empty() && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_graphic());
            match Destination::parse(&s) {
                Ok(d) => {
                    assert!(word, "{s:?} read");
                    assert_eq!(d.to_string(), s);
                    read += 1;
                }
                Err(_) => assert!(!word, "{s:?} refused"),
            }
        }
        assert!(read > 5_000, "{read}");
    }
}
