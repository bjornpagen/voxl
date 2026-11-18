//! The relay's one way into the server: `ssh <host> voxl …`, over whatever ssh's own
//! configuration makes of `<host>` (its master connection, its keys). ssh carries the
//! control stream (`voxl attach`) and each file transfer (`voxl files …`).

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

/// `arg` as one word of a POSIX shell's command line: ssh hands the server's login shell
/// one string, so every argument is quoted whole. Inside single quotes nothing is special
/// but the quote itself, which is closed, escaped, and opened again.
fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// `ssh <to> voxl <args…>`, its three pipes the relay's, killed when dropped.
fn voxl(to: &Destination, args: &[&str]) -> Command {
    let mut line = String::from("voxl");
    for a in args {
        line.push(' ');
        line.push_str(&quote(a));
    }
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
            _ => Unopened::Exited(status),
        }
    }
}

/// `voxl attach` on the server: the session's control stream on its stdin and stdout.
/// Its stderr is the relay's, so what ssh says (a key to touch, a host key) reaches the
/// relay's log.
pub fn attach(to: &Destination) -> std::io::Result<Child> {
    voxl(to, &["attach"]).stderr(Stdio::inherit()).spawn()
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
