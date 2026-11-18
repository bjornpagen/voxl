//! Cursor shape tracking and two-way CLIPBOARD sync on a dedicated X connection.

use crate::hub::{Hub, Out, ServerMessage};
use bytes::Bytes;
use nix::fcntl::OFlag;
use nix::poll::{poll, PollFd, PollFlags};
use nix::unistd::{pipe2, read, write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;
use std::sync::Arc;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, EventMask, PropMode, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::event::{Bin, ClipContent};
use crate::lanes::wire::MAX_DOWN_BINARY;
use x11rb::errors::{ConnectionError, ReplyError};

/// The largest clipboard image sent to the viewers: what a page's binary message carries
/// (`MAX_DOWN_BINARY`) after the image's type byte.
const MAX_IMAGE: usize = MAX_DOWN_BINARY - std::mem::size_of::<Bin>();

/// Sets the remote CLIPBOARD and returns once the X server has registered us as owner,
/// so a paste shortcut injected right afterwards sees the new contents.
#[derive(Clone)]
pub struct ClipboardHandle {
    tx: mpsc::Sender<(ClipContent, mpsc::SyncSender<()>)>,
    wake_w: Arc<OwnedFd>,
}

impl ClipboardHandle {
    /// Hands `content` to the desktop thread and waits for it to own the clipboard. The
    /// thread answers once the X server has said so, or ends the daemon; gone, it set
    /// nothing, which is said.
    pub fn set_blocking(&self, content: ClipContent) {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        if self.tx.send((content, done_tx)).is_err() {
            log::error!("the desktop thread has ended: the clipboard is not set");
            return;
        }
        // A full pipe already holds a wake.
        match write(&*self.wake_w, &[1u8]) {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => log::error!("the desktop thread's wake: {e}"),
        }
        if done_rx.recv().is_err() {
            log::error!("the desktop thread ended before it set the clipboard");
        }
    }
}

struct Atoms {
    clipboard: u32,
    targets: u32,
    utf8: u32,
    text: u32,
    text_plain: u32,
    incr: u32,
    prop: u32,
    png: u32,
}

fn atom(c: &RustConnection, name: &str) -> Result<u32, String> {
    Ok(c.intern_atom(false, name.as_bytes())
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .atom)
}

/// Opens the desktop's own X connection, hides X's cursor, and starts the thread that
/// sends the cursor's image and syncs the clipboard both ways.
pub fn spawn(display: String, hub: Arc<Hub>) -> Result<ClipboardHandle, String> {
    let (conn, n) = RustConnection::connect(Some(&display)).map_err(|e| e.to_string())?;
    let root = conn.setup().roots[n].root;
    conn.xfixes_query_version(5, 0)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    let win = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(
        0,
        win,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(|e| e.to_string())?;
    let atoms = Atoms {
        clipboard: atom(&conn, "CLIPBOARD")?,
        targets: atom(&conn, "TARGETS")?,
        utf8: atom(&conn, "UTF8_STRING")?,
        text: atom(&conn, "TEXT")?,
        text_plain: atom(&conn, "text/plain;charset=utf-8")?,
        incr: atom(&conn, "INCR")?,
        prop: atom(&conn, "VOXL_CLIP")?,
        png: atom(&conn, "image/png")?,
    };
    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)
        .map_err(|e| e.to_string())?;
    // Clients draw the cursor locally. Xvfb's software cursor is painted into the
    // framebuffer we read directly, which would show a second, lagging cursor and
    // turn every pointer move into an encode, so hide it while we are connected.
    conn.xfixes_hide_cursor(root).map_err(|e| e.to_string())?;
    conn.xfixes_select_selection_input(
        root,
        atoms.clipboard,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER,
    )
    .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel::<(ClipContent, mpsc::SyncSender<()>)>();
    let (wake_r, wake_w) =
        pipe2(OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).map_err(|e| e.to_string())?;
    let desktop = Desktop {
        conn,
        win,
        atoms,
        hub,
        owned: None,
        incr: None,
        last_image: None,
    };
    std::thread::Builder::new()
        .name("desktop".into())
        .spawn(move || {
            // The connection broken is the X server gone: the daemon ends with it.
            if let Err(e) = desktop.run(&rx, &wake_r) {
                log::error!("desktop X connection: {e}");
                crate::logging::exit(1);
            }
        })
        .map_err(|e| format!("no desktop thread: {e}"))?;
    Ok(ClipboardHandle {
        tx,
        wake_w: Arc::new(wake_w),
    })
}

/// The desktop thread's state: its X connection and window, and the clipboard as it holds
/// it.
struct Desktop {
    conn: RustConnection,
    win: u32,
    atoms: Atoms,
    hub: Arc<Hub>,
    /// What this thread owns the clipboard with, while it does.
    owned: Option<ClipContent>,
    /// An INCR transfer in progress: the target being fetched, and its bytes so far.
    incr: Option<(u32, Vec<u8>)>,
    /// The last image sent to viewers, by its hash: one copied again is not resent.
    last_image: Option<u64>,
}

impl Desktop {
    /// Until the connection breaks: X's events and the viewers' clipboards, each as it
    /// comes, waiting with no time on the X connection and the wake pipe. A request the X
    /// server refuses (a requestor gone, a property it will not take) is said and skipped.
    fn run(
        mut self,
        rx: &mpsc::Receiver<(ClipContent, mpsc::SyncSender<()>)>,
        wake_r: &OwnedFd,
    ) -> Result<(), ConnectionError> {
        send_cursor(&self.conn, &self.hub)?;
        loop {
            let mut drain = [0u8; 64];
            while matches!(read(wake_r, &mut drain), Ok(n) if n > 0) {}
            while let Ok((content, done)) = rx.try_recv() {
                self.own(content)?;
                if done.send(()).is_err() {
                    log::info!("a clipboard was set for an input thread that had gone");
                }
            }
            let Some(ev) = self.conn.poll_for_event()? else {
                let mut p = [
                    PollFd::new(self.conn.stream().as_fd(), PollFlags::POLLIN),
                    PollFd::new(wake_r.as_fd(), PollFlags::POLLIN),
                ];
                match poll(&mut p, nix::poll::PollTimeout::NONE) {
                    Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                    Err(e) => return Err(ConnectionError::IoError(e.into())),
                }
                continue;
            };
            self.event(ev)?;
        }
    }

    /// A viewer's clipboard: this thread owns the desktop's with it, in effect once the
    /// round trip after it returns.
    fn own(&mut self, content: ClipContent) -> Result<(), ConnectionError> {
        match &content {
            ClipContent::Text(t) => *self.hub.last_clipboard.lock() = Some(t.clone()),
            ClipContent::Image(png) => self.last_image = Some(hash(png)),
        }
        self.owned = Some(content);
        self.conn
            .set_selection_owner(self.win, self.atoms.clipboard, x11rb::CURRENT_TIME)?;
        said(self.conn.get_selection_owner(self.atoms.clipboard)?.reply());
        Ok(())
    }

    fn event(&mut self, ev: Event) -> Result<(), ConnectionError> {
        let (conn, atoms, win) = (&self.conn, &self.atoms, self.win);
        match ev {
            Event::XfixesCursorNotify(_) => send_cursor(conn, &self.hub)?,
            // Another client owns the clipboard: ask what it offers, then fetch text or
            // an image.
            Event::XfixesSelectionNotify(e) if e.owner != win && e.owner != x11rb::NONE => {
                conn.convert_selection(
                    win,
                    atoms.clipboard,
                    atoms.targets,
                    atoms.prop,
                    x11rb::CURRENT_TIME,
                )?;
                conn.flush()?;
            }
            Event::SelectionNotify(e) if e.property != x11rb::NONE => {
                let reply = conn
                    .get_property(true, win, atoms.prop, AtomEnum::ANY, 0, u32::MAX / 4)?
                    .reply();
                let Some(r) = said(reply) else {
                    return Ok(());
                };
                if e.target == atoms.targets {
                    let offered: Vec<u32> = match r.value32() {
                        Some(v) => v.collect(),
                        None => return Ok(()),
                    };
                    // Text wins when both are offered (rich text copies carry both).
                    let want = [atoms.utf8, atoms.png, u32::from(AtomEnum::STRING)]
                        .into_iter()
                        .find(|t| offered.contains(t));
                    if let Some(t) = want {
                        conn.convert_selection(
                            win,
                            atoms.clipboard,
                            t,
                            atoms.prop,
                            x11rb::CURRENT_TIME,
                        )?;
                    }
                } else if r.type_ == atoms.incr {
                    self.incr = Some((e.target, Vec::new()));
                } else {
                    publish(&self.hub, atoms, e.target, r.value, &mut self.last_image);
                }
                conn.flush()?;
            }
            Event::PropertyNotify(e)
                if e.atom == atoms.prop && e.state == xproto::Property::NEW_VALUE =>
            {
                let Some((target, data)) = self.incr.as_mut() else {
                    return Ok(());
                };
                let reply = conn
                    .get_property(true, win, atoms.prop, AtomEnum::ANY, 0, u32::MAX / 4)?
                    .reply();
                if let Some(r) = said(reply) {
                    match r.value.is_empty() {
                        // An empty chunk ends the transfer.
                        true => {
                            let (target, data) = (*target, std::mem::take(data));
                            self.incr = None;
                            publish(&self.hub, atoms, target, data, &mut self.last_image);
                        }
                        false => data.extend_from_slice(&r.value),
                    }
                }
                conn.flush()?;
            }
            Event::SelectionRequest(e) => {
                let prop = match e.property {
                    x11rb::NONE => e.target,
                    p => p,
                };
                let given = self.answer(&e, prop)?;
                let notify = xproto::SelectionNotifyEvent {
                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: e.time,
                    requestor: e.requestor,
                    selection: e.selection,
                    target: e.target,
                    property: match given {
                        true => prop,
                        false => x11rb::NONE,
                    },
                };
                conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
                conn.flush()?;
            }
            Event::SelectionClear(_) => self.owned = None,
            _ => {}
        }
        Ok(())
    }

    /// Writes what `e` asks of the clipboard this thread owns onto the requestor's
    /// `prop`; whether there was any to write.
    fn answer(
        &self,
        e: &xproto::SelectionRequestEvent,
        prop: u32,
    ) -> Result<bool, ConnectionError> {
        let (conn, atoms) = (&self.conn, &self.atoms);
        if e.selection != atoms.clipboard {
            return Ok(false);
        }
        let text_targets = [
            atoms.utf8,
            atoms.text,
            atoms.text_plain,
            u32::from(AtomEnum::STRING),
        ];
        let put8 = |target, bytes: &[u8]| {
            conn.change_property8(PropMode::REPLACE, e.requestor, prop, target, bytes)
        };
        let put_targets = |list: &[u32]| {
            conn.change_property32(PropMode::REPLACE, e.requestor, prop, AtomEnum::ATOM, list)
        };
        match &self.owned {
            Some(ClipContent::Text(_)) | Some(ClipContent::Image(_))
                if e.target == atoms.targets =>
            {
                let list: &[u32] = match &self.owned {
                    Some(ClipContent::Image(_)) => &[atoms.targets, atoms.png],
                    _ => &[
                        atoms.targets,
                        atoms.utf8,
                        u32::from(AtomEnum::STRING),
                        atoms.text,
                        atoms.text_plain,
                    ],
                };
                put_targets(list)?;
                Ok(true)
            }
            Some(ClipContent::Text(text)) if text_targets.contains(&e.target) => {
                put8(e.target, text.as_bytes())?;
                Ok(true)
            }
            // An image longer than the X server's longest request is no property it takes.
            Some(ClipContent::Image(png)) if e.target == atoms.png => match put8(atoms.png, png) {
                Ok(_) => Ok(true),
                Err(ConnectionError::MaximumRequestLengthExceeded) => {
                    log::warn!(
                        "a clipboard image of {} bytes is past X's longest request",
                        png.len()
                    );
                    Ok(false)
                }
                Err(e) => Err(e),
            },
            Some(_) | None => Ok(false),
        }
    }
}

/// A reply, or the X error that came in its place, said: a request the X server refuses
/// is skipped. A broken connection is not this: it is the `ConnectionError` before it.
fn said<T>(reply: Result<T, ReplyError>) -> Option<T> {
    match reply {
        Ok(r) => Some(r),
        Err(e) => {
            log::warn!("the X server refused a clipboard request: {e}");
            None
        }
    }
}

fn hash(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

fn publish(hub: &Hub, atoms: &Atoms, target: u32, data: Vec<u8>, last_image: &mut Option<u64>) {
    if target == atoms.png {
        if data.is_empty() || data.len() > MAX_IMAGE {
            return;
        }
        let h = hash(&data);
        if *last_image == Some(h) {
            return;
        }
        *last_image = Some(h);
        let mut msg = Vec::with_capacity(data.len() + 1);
        msg.push(Bin::ClipImage as u8);
        msg.extend_from_slice(&data);
        let b = Bytes::from(msg);
        hub.broadcast(|| Out::Bin(b.clone()));
        return;
    }
    let Ok(text) = String::from_utf8(data) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    {
        let mut last = hub.last_clipboard.lock();
        if last.as_deref() == Some(text.as_str()) {
            return;
        }
        *last = Some(text.clone());
    }
    let msg = ServerMessage::Clip { d: text };
    hub.broadcast(|| Out::Text(msg.clone()));
}

/// Sends the cursor's image to every viewer, and keeps it for those that come.
fn send_cursor(conn: &RustConnection, hub: &Hub) -> Result<(), ConnectionError> {
    let Some(img) = said(conn.xfixes_get_cursor_image()?.reply()) else {
        return Ok(());
    };
    let (w, h) = (img.width as usize, img.height as usize);
    let mut msg = Vec::with_capacity(14 + w * h * 4);
    msg.push(Bin::Cursor as u8);
    msg.push(0);
    msg.extend_from_slice(&(w as u16).to_be_bytes());
    msg.extend_from_slice(&(h as u16).to_be_bytes());
    msg.extend_from_slice(&img.xhot.to_be_bytes());
    msg.extend_from_slice(&img.yhot.to_be_bytes());
    msg.extend_from_slice(&img.cursor_serial.to_be_bytes());
    for &p in img.cursor_image.iter().take(w * h) {
        let a = (p >> 24) & 0xff;
        // Un-premultiplied; a transparent pixel's colour is none.
        let un = |c: u32| match (c * 255 + a / 2).checked_div(a) {
            Some(v) => v.min(255) as u8,
            None => 0,
        };
        msg.extend_from_slice(&[
            un((p >> 16) & 0xff),
            un((p >> 8) & 0xff),
            un(p & 0xff),
            a as u8,
        ]);
    }
    let b = Bytes::from(msg);
    *hub.cursor.lock() = Some(b.clone());
    hub.broadcast(|| Out::Bin(b.clone()));
    Ok(())
}
