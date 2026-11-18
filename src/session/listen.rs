//! The daemon's three unix sockets, bound where the flags say, each with the mode its
//! callers need: the control socket only for this user, the lane sockets also for the
//! proxy's group.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};

/// Who may connect to a socket, as its file's mode says.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// The daemon's own user (`voxl attach` over ssh): 0600. The peer's uid is checked
    /// as well (`own_user`), so a mode loosened by hand lets nobody else in.
    User,
    /// The user and the socket's group, which the proxy's user is in: 0660.
    Group,
}

impl Mode {
    fn bits(self) -> u32 {
        match self {
            Mode::User => 0o600,
            Mode::Group => 0o660,
        }
    }
}

/// A socket that could not be made, and why.
#[derive(Debug)]
pub struct Unbound {
    what: &'static str,
    path: std::path::PathBuf,
    error: std::io::Error,
}

impl std::fmt::Display for Unbound {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "the {} socket {}: {}",
            self.what,
            self.path.display(),
            self.error
        )
    }
}

fn unbound<'a>(what: &'static str, path: &'a Path) -> impl FnOnce(std::io::Error) -> Unbound + 'a {
    move |error| Unbound {
        what,
        path: path.to_path_buf(),
        error,
    }
}

fn chmod(path: &Path, mode: Mode) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode.bits()))
}

/// What is at a socket's path before it is bound.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Before {
    Nothing,
    /// A socket a crashed daemon left: nothing answers on it, and it is removed.
    Left,
}

/// What is at `path`, as a socket of `kind` is about to be bound there: nothing, or a
/// socket nothing answers on (a connect of its kind is refused), which a crash leaves
/// behind and is removed. Anything else (a live socket, a file, a folder) is an error, and
/// nothing is deleted.
fn clear(path: &Path, kind: Kind) -> std::io::Result<Before> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Before::Nothing),
        Err(e) => return Err(e),
        Ok(md) if !md.file_type().is_socket() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "something that is not a socket is there",
            ))
        }
        Ok(_) => {}
    }
    let answer = match kind {
        Kind::Stream => std::os::unix::net::UnixStream::connect(path).map(drop),
        Kind::Datagram => std::os::unix::net::UnixDatagram::unbound()?.connect(path),
    };
    match answer {
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path)?;
            Ok(Before::Left)
        }
        Err(e) => Err(e),
        Ok(()) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "a socket there answers: another daemon runs",
        )),
    }
}

/// A socket's kind, as `clear` connects to one left at its path.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Stream,
    Datagram,
}

fn listener(what: &'static str, path: &Path, mode: Mode) -> Result<UnixListener, Unbound> {
    if clear(path, Kind::Stream).map_err(unbound(what, path))? == Before::Left {
        log::info!(
            "removed the {what} socket a stopped daemon left at {}",
            path.display()
        );
    }
    let l = UnixListener::bind(path).map_err(unbound(what, path))?;
    chmod(path, mode).map_err(unbound(what, path))?;
    Ok(l)
}

/// The control socket at `path`, for this user only.
pub fn control(path: &Path) -> Result<UnixListener, Unbound> {
    listener("control", path, Mode::User)
}
/// The lane stream socket at `path`, where a proxy's TCP to the lane port arrives.
pub fn stream(path: &Path) -> Result<UnixListener, Unbound> {
    listener("lane stream", path, Mode::Group)
}

/// The lane datagram socket at `path`, where a proxy's UDP to the lane port arrives:
/// non-blocking, as the std socket the lanes' writers send from with a peer's address
/// (`send_to_addr`, which tokio's socket lacks for abstract addresses).
pub fn datagram(path: &Path) -> Result<std::os::unix::net::UnixDatagram, Unbound> {
    let what = "lane datagram";
    if clear(path, Kind::Datagram).map_err(unbound(what, path))? == Before::Left {
        log::info!(
            "removed the {what} socket a stopped daemon left at {}",
            path.display()
        );
    }
    let d = std::os::unix::net::UnixDatagram::bind(path).map_err(unbound(what, path))?;
    d.set_nonblocking(true).map_err(unbound(what, path))?;
    chmod(path, Mode::Group).map_err(unbound(what, path))?;
    Ok(d)
}

/// Whether the peer of a control connection runs as this process's user: `SO_PEERCRED`
/// on Linux, `getpeereid` on macOS (tokio's `peer_cred`). One that cannot be told is not.
pub fn own_user(s: &UnixStream) -> bool {
    // SAFETY: getuid has no failure and touches nothing.
    let me = unsafe { libc::getuid() };
    s.peer_cred().is_ok_and(|c| c.uid() == me)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;

    /// A folder of the test's own, removed when it is dropped.
    struct Folder(std::path::PathBuf);
    impl Drop for Folder {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    /// Each socket binds with its mode (the control socket the user's alone, 0600; the
    /// lanes' the group's too, 0660); a socket a stopped daemon left is removed and bound
    /// again; a live one, and anything that is no socket, is the error and is left as it
    /// was. Over the three sockets, each in each state its path can be in.
    #[tokio::test]
    async fn a_socket_binds_over_only_what_a_stopped_daemon_left() {
        let dir = std::env::temp_dir().join(format!("voxl-listen-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let folder = Folder(dir);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let is_socket = |p: &Path| {
            std::fs::symlink_metadata(p)
                .unwrap()
                .file_type()
                .is_socket()
        };
        type Bind = fn(&Path) -> Result<(), Unbound>;
        let sockets: [(&str, u32, Bind); 3] = [
            ("control", 0o600, |p| control(p).map(drop)),
            ("stream", 0o660, |p| stream(p).map(drop)),
            ("datagram", 0o660, |p| datagram(p).map(drop)),
        ];
        for (name, want, bind) in sockets {
            let path = folder.0.join(name);
            // Nothing there: bound, with its mode; the socket stays when its listener goes.
            bind(&path).unwrap();
            assert_eq!(mode(&path), want, "{name}");
            // What that left (nothing answers): removed and bound again.
            bind(&path).unwrap();
            assert!(is_socket(&path), "{name}");
            // A live socket of its kind there: refused, and left.
            let live: Box<dyn std::any::Any> = match name {
                "datagram" => {
                    std::fs::remove_file(&path).unwrap();
                    Box::new(std::os::unix::net::UnixDatagram::bind(&path).unwrap())
                }
                _ => {
                    std::fs::remove_file(&path).unwrap();
                    Box::new(std::os::unix::net::UnixListener::bind(&path).unwrap())
                }
            };
            assert!(bind(&path).is_err(), "{name} bound over a live socket");
            assert!(is_socket(&path), "{name}");
            drop(live);
            std::fs::remove_file(&path).unwrap();
            // A file there: refused, and left.
            std::fs::write(&path, b"x").unwrap();
            assert!(bind(&path).is_err(), "{name} bound over a file");
            assert_eq!(std::fs::read(&path).unwrap(), b"x");
            std::fs::remove_file(&path).unwrap();
        }
    }
}
