//! The daemon's sessions: one for each `voxl attach`, which is a viewer's relay come over
//! ssh. A session is its control connection (the ssh channel: hello, input, clipboard,
//! cursor, microphone, and the word of each video update the page took) and the lane the
//! relay attaches to it through a proxy, which carries video and audio one way. The daemon
//! serves no network port: the control socket and the lane socket are unix sockets,
//! reached only through ssh and the proxy on this machine.

pub mod attach;
pub mod control;
pub(crate) mod lane;
pub mod link;
mod listen;
pub mod query;
pub mod sender;
mod statement;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::Arc;

/// The folder everything the daemon opens by name is in (`--runtime`): its two sockets,
/// the microphone's FIFO and the keymap's scratch file, each by a fixed name. The host
/// makes it at boot (a tmpfiles.d rule, setgid to the group the proxy's user is in); the
/// daemon and its subcommands take it only if it is a folder of this user's that nobody
/// else may write in, so no other user can put a socket or a FIFO of theirs where these
/// are looked for.
#[derive(Clone, Debug, PartialEq)]
pub struct Runtime(PathBuf);

/// Why a runtime folder is not taken.
#[derive(Debug, PartialEq)]
pub enum Untrusted {
    /// It cannot be looked at (missing, say), as the error says.
    Unread(String),
    NotAFolder,
    /// Another user's: the uid that owns it.
    NotOurs(u32),
    /// Others may write in it: its mode.
    OthersWrite(u32),
}

impl std::fmt::Display for Untrusted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Untrusted::Unread(e) => write!(f, "{e}"),
            Untrusted::NotAFolder => write!(f, "not a folder"),
            Untrusted::NotOurs(uid) => write!(f, "owned by uid {uid}, not this user"),
            Untrusted::OthersWrite(mode) => write!(f, "mode {mode:o}: others may write in it"),
        }
    }
}

impl Runtime {
    /// `dir`, if it is a folder this user owns that others may not write in. Its link, if
    /// it is one, is not followed: the folder itself is what is judged.
    pub fn open(dir: PathBuf) -> Result<Runtime, Untrusted> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(&dir).map_err(|e| Untrusted::Unread(e.to_string()))?;
        // SAFETY: getuid has no failure and touches nothing.
        let me = unsafe { libc::getuid() };
        if !md.is_dir() {
            return Err(Untrusted::NotAFolder);
        }
        if md.uid() != me {
            return Err(Untrusted::NotOurs(md.uid()));
        }
        if md.mode() & 0o002 != 0 {
            return Err(Untrusted::OthersWrite(md.mode() & 0o7777));
        }
        Ok(Runtime(dir))
    }

    /// The daemon's two sockets in it.
    pub fn sockets(&self) -> Sockets {
        Sockets {
            control: self.control(),
            lane: self.0.join("lane-stream"),
        }
    }

    /// The control socket `voxl attach` and the queries connect to.
    pub fn control(&self) -> PathBuf {
        self.0.join("control")
    }

    /// The FIFO PulseAudio's pipe source reads as the desktop's microphone.
    #[cfg(target_os = "linux")]
    pub fn mic(&self) -> PathBuf {
        self.0.join("mic")
    }

    /// The scratch keycodes voxl has remapped, to be undone after a crash.
    pub fn keymap(&self) -> PathBuf {
        self.0.join("keymap")
    }
}

/// Writes `bytes` as the file at `path`, this user's alone (0600) whatever the umask and
/// whatever mode a file already there had: the runtime folder's group may reach only the
/// lane socket.
pub fn private_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)
}

/// The daemon's two sockets.
#[derive(Clone, Debug, PartialEq)]
pub struct Sockets {
    /// `voxl attach` connects here; only this user's processes are let in.
    pub control: PathBuf,
    /// A proxy's TCP to `localhost:6901` arrives here.
    pub lane: PathBuf,
}

#[cfg(target_os = "linux")]
/// Binds the daemon's two sockets and serves sessions on them until the daemon ends; a
/// socket that cannot be bound is the error, and nothing is served.
pub async fn serve(sockets: &Sockets, daemon: Arc<control::Daemon>) -> Result<(), listen::Unbound> {
    let control = listen::control(&sockets.control)?;
    let lane = listen::lane(&sockets.lane)?;
    log::info!(
        "serving sessions at {}, the lane at {}",
        sockets.control.display(),
        sockets.lane.display()
    );
    tokio::spawn(lane::lanes(lane, daemon.hub.clone()));
    control::serve(control, daemon).await;
    Ok(())
}
