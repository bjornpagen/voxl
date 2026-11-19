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
        match v {
            "" => Err("--runtime \"\": not a folder's path".to_string()),
            path => Ok(Runtime(path.to_string())),
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
            Some(_) | None => Unopened::Exited(status),
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

    /// A POSIX shell reads every quoted argument back as the argument: over strings of
    /// every byte a shell word can hold (no NUL), quotes, backslashes, `$`, backticks,
    /// spaces and newlines among them. The oracle is `/bin/sh` itself.
    #[test]
    fn the_shell_reads_a_quoted_argument_as_itself() {
        const SPECIAL: &[u8] = b"'\"\\$`!*?[]{}()<>|&;#~ \t\n-=%";
        let mut rng = Xorshift::new(0xA0761D6478BD642F);
        let args: Vec<String> = (0..200)
            .map(|_| {
                let bytes: Vec<u8> = (0..rng.below(24))
                    .map(|_| match rng.below(3) {
                        0 => SPECIAL[rng.below(SPECIAL.len() as u64) as usize],
                        1 => b'a' + rng.below(26) as u8,
                        _ => 1 + rng.below(127) as u8,
                    })
                    .collect();
                String::from_utf8(bytes).unwrap()
            })
            .collect();
        // One shell for all of them: each printed between NULs, which no argument holds.
        let line: String = args
            .iter()
            .map(|a| format!("printf '%s\\0' {}\n", quote(a)))
            .collect();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(line)
            .output()
            .unwrap();
        assert!(out.status.success());
        let read: Vec<String> = out
            .stdout
            .split(|&b| b == 0)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect();
        assert_eq!(read[..args.len()], args[..]);
        assert_eq!(read.len(), args.len() + 1);
    }

    /// The server reads the attach the relay sends as the attach it means: over drawn
    /// runtime folders (any non-empty string of what a command line holds: quotes, `$`,
    /// spaces, newlines, a leading `--`, `=` and letters beyond ASCII) and none, the line
    /// ssh hands the login shell, split by `/bin/sh` as that shell splits it, is `voxl
    /// attach` with flags the daemon's own parser reads as that folder, or as its default
    /// when none was named. The oracles are `/bin/sh` and the daemon's `parse_args`.
    #[test]
    fn attach_reaches_the_daemon_at_the_runtime_named() {
        const SPECIAL: &[char] = &[
            '\'', '"', '\\', '$', '`', ' ', '\t', '\n', '-', '=', '/', 'é', '*',
        ];
        let mut rng = Xorshift::new(0x9E3779B97F4A7C15);
        let daemons: Vec<Daemon> = (0..300)
            .map(|i| {
                if i % 5 == 0 {
                    return Daemon::Default;
                }
                Daemon::In({
                    let mut s: String = (0..1 + rng.below(16))
                        .map(|_| match rng.below(3) {
                            0 => SPECIAL[rng.below(SPECIAL.len() as u64) as usize],
                            _ => char::from(b'a' + rng.below(26) as u8),
                        })
                        .collect();
                    if rng.below(6) == 0 {
                        s.insert_str(0, "--");
                    }
                    Runtime::parse(&s).unwrap()
                })
            })
            .collect();
        let script: String = daemons
            .iter()
            .map(|r| {
                let args = attach_args(r);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                // The shell runs `voxl` as printf's arguments: each word between NULs, the
                // command's end a SOH, which no drawn string holds.
                format!(
                    "set -- {}; printf '%s\\0' \"$@\"; printf '\\001'\n",
                    line(&args)
                )
            })
            .collect();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        assert!(out.status.success());
        let commands: Vec<&[u8]> = out.stdout.split(|&b| b == 1).collect();
        assert_eq!(commands.len(), daemons.len() + 1);
        for (r, command) in daemons.iter().zip(commands) {
            let words: Vec<String> = command
                .split(|&b| b == 0)
                .map(|w| String::from_utf8(w.to_vec()).unwrap())
                .collect();
            let (end, words) = words.split_last().unwrap();
            assert_eq!(end, "");
            assert_eq!(words[..2], ["voxl", "attach"], "{r:?}");
            let read = crate::parse_args(words[2..].iter().cloned(), &[crate::RUNTIME.name])
                .unwrap_or_else(|e| panic!("{r:?}: {e}"));
            let folder = crate::folder((
                crate::RUNTIME.name,
                read.get(crate::RUNTIME.name)
                    .map_or(crate::RUNTIME.default, String::as_str),
            ))
            .unwrap();
            let want = match r {
                Daemon::Default => crate::RUNTIME.default,
                Daemon::In(runtime) => runtime.0.as_str(),
            };
            assert_eq!(folder, std::path::PathBuf::from(want), "{r:?}");
        }
        assert_eq!(
            Runtime::parse(""),
            Err("--runtime \"\": not a folder's path".into())
        );
    }

    /// An exit before the capability is NoDoor exactly when its status is ssh's own failure
    /// (255, ssh(1)); every other code, and death by a signal, is an exit of its own. The
    /// statuses are real ones, of `/bin/sh -c 'exit N'` and of a killed shell.
    #[test]
    fn only_ssh_s_own_failure_is_no_door() {
        for code in 0..=255 {
            let status = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("exit {code}"))
                .status()
                .unwrap();
            let got = Unopened::of(status);
            match code {
                255 => assert_eq!(got, Unopened::NoDoor),
                _ => assert_eq!(got, Unopened::Exited(status), "exit {code}"),
            }
        }
        let killed = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("kill -KILL $$")
            .status()
            .unwrap();
        assert_eq!(Unopened::of(killed), Unopened::Exited(killed));
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
                    _ => char::from(b'!' + rng.below(94) as u8),
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
