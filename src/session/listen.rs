//! The daemon's two unix sockets, bound where the flags say, each with the mode its callers
//! need: the control socket only for this user, the lane socket also for the proxy's group.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};

/// Who may connect to a socket, as its file's mode says.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// The daemon's own user (`voxl attach` over ssh): 0600. The peer's uid is checked
    /// as well (`peer`), so a mode loosened by hand lets nobody else in.
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

/// What is at `path`, as a socket is about to be bound there: nothing, or a socket nothing
/// answers on (a connect is refused), which a crash leaves behind and is removed. Anything
/// else (a live socket, a file, a folder) is an error, and nothing is deleted.
fn clear(path: &Path) -> std::io::Result<Before> {
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
    match std::os::unix::net::UnixStream::connect(path).map(drop) {
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

fn listener(what: &'static str, path: &Path, mode: Mode) -> Result<UnixListener, Unbound> {
    if clear(path).map_err(unbound(what, path))? == Before::Left {
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
/// The lane socket at `path`, where a proxy's TCP to the lane port arrives.
pub fn lane(path: &Path) -> Result<UnixListener, Unbound> {
    listener("lane", path, Mode::Group)
}

/// Who the peer of a control connection runs as.
pub enum Peer {
    /// This process's user.
    Ours,
    /// Another user: its uid.
    Theirs(u32),
    /// Nobody can tell: why.
    Untold(std::io::Error),
}

/// Who the peer of `s` runs as: `SO_PEERCRED` on Linux, `getpeereid` on macOS (tokio's
/// `peer_cred`).
pub fn peer(s: &UnixStream) -> Peer {
    // SAFETY: getuid has no failure and touches nothing.
    let me = unsafe { libc::getuid() };
    match s.peer_cred() {
        Ok(c) if c.uid() == me => Peer::Ours,
        Ok(c) => Peer::Theirs(c.uid()),
        Err(e) => Peer::Untold(e),
    }
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

    /// Returns once nothing answers on the socket at `path`, whose listener this test has
    /// dropped. On macOS a socket is made close-on-exec only after it is made, so a child
    /// another test starts between the two (ffmpeg, a test in a process of its own)
    /// inherits the listener, and it answers until that child's exec closes it. That is
    /// this process's doing, not the daemon's: the test waits it out, on the one thing that
    /// ends it.
    fn unanswered(path: &Path) {
        while std::os::unix::net::UnixStream::connect(path).is_ok() {
            std::thread::yield_now();
        }
    }

    /// Each socket binds with its mode (the control socket the user's alone, 0600; the
    /// lane's the group's too, 0660); a socket a stopped daemon left is removed and bound
    /// again; a live one, and anything that is no socket, is the error and is left as it
    /// was. Over the two sockets, each in each state its path can be in.
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
        let sockets: [(&str, u32, Bind); 2] = [
            ("control", 0o600, |p| control(p).map(drop)),
            ("lane", 0o660, |p| lane(p).map(drop)),
        ];
        for (name, want, bind) in sockets {
            let path = folder.0.join(name);
            // Nothing there: bound, with its mode; the socket stays when its listener goes.
            bind(&path).unwrap();
            assert_eq!(mode(&path), want, "{name}");
            // What that left, once nothing answers on it: removed and bound again.
            unanswered(&path);
            bind(&path).unwrap();
            assert!(is_socket(&path), "{name}");
            // A live socket of its kind there: refused, and left.
            std::fs::remove_file(&path).unwrap();
            let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
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
