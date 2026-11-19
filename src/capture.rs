//! X11 screen capture: MIT-SHM GetImage into a persistent BGRX frame, driven by XDamage.

use crate::env::Debugging;
use crate::video::{Band, Damage};
use std::os::fd::{AsFd, BorrowedFd};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// A SysV shared-memory segment attached to this process: detached when dropped.
struct ShmBuf {
    ptr: *mut u8,
    len: usize,
    shmid: i32,
}

unsafe impl Send for ShmBuf {}

impl ShmBuf {
    /// Attaches the segment `shmid` of `len` bytes, read-only or not (`flags`).
    fn attach(shmid: i32, len: usize, flags: libc::c_int) -> Result<ShmBuf, String> {
        // SAFETY: shmat maps the segment or fails, and reads nothing of ours.
        let ptr = unsafe { libc::shmat(shmid, std::ptr::null(), flags) };
        // shmat's failure is the address -1 (shmat(2)).
        if ptr as isize == -1 {
            return Err(format!(
                "shmat {shmid}: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(ShmBuf {
            ptr: ptr.cast(),
            len,
            shmid,
        })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: the segment is mapped for `len` bytes while this is attached.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        // SAFETY: the address shmat gave, detached once.
        unsafe { libc::shmdt(self.ptr as *const _) };
    }
}

/// A segment this process made: marked for removal when dropped, if it was not before, so
/// it goes once nothing has it attached.
struct Made(ShmBuf);

impl Made {
    fn new(len: usize) -> Result<Made, String> {
        // SAFETY: shmget makes a segment or fails, and reads nothing of ours.
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return Err(format!("shmget: {}", std::io::Error::last_os_error()));
        }
        match ShmBuf::attach(shmid, len, 0) {
            Ok(buf) => Ok(Made(buf)),
            Err(e) => {
                remove(shmid);
                Err(e)
            }
        }
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        remove(self.0.shmid);
    }
}

/// Marks the segment `shmid` for removal: it goes once the last process detaches it.
fn remove(shmid: i32) {
    // SAFETY: IPC_RMID reads no buffer of ours.
    unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
}

/// Where the screen's pixels are read.
enum Source {
    /// Xvfb's own framebuffer, read in place: the screen begins `offset` bytes into it.
    InPlace { fb: ShmBuf, offset: usize },
    /// A segment of this process's, which GetImage fills, attached to the X server as
    /// `seg`.
    GetImage { buf: Made, seg: u32 },
}

impl Source {
    /// The segment, and where the screen begins in it.
    fn image(&self) -> (&[u8], usize) {
        match self {
            Source::InPlace { fb, offset } => (fb.as_slice(), *offset),
            Source::GetImage { buf, .. } => (buf.0.as_slice(), 0),
        }
    }
}

pub struct Capturer {
    pub conn: RustConnection,
    root: u32,
    pub width: u32,
    pub height: u32,
    resize_pending: bool,
    pub stride: usize,
    source: Source,
    /// Kept for the connection's lifetime; raw-rectangle events need no further requests.
    _damage: u32,
    /// Rows touched since the last take (raw damage rectangles, no server round trip).
    pub damage: Damage,
    debug: Debugging,
    /// The threads that convert its captures.
    rows: crate::convert::Rows,
}

impl Capturer {
    pub fn new(display: &str, debug: Debugging, convert_threads: usize) -> Result<Self, String> {
        let (conn, screen_num) =
            RustConnection::connect(Some(display)).map_err(|e| format!("X connect: {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        // Subscribe before querying geometry so a resize during initialization is not lost.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
        let geometry = conn
            .get_geometry(root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let width = u32::from(geometry.width);
        let height = u32::from(geometry.height);
        if screen.root_depth != 24 && screen.root_depth != 32 {
            return Err(format!("unsupported root depth {}", screen.root_depth));
        }
        conn.extension_information(shm::X11_EXTENSION_NAME)
            .map_err(|e| e.to_string())?
            .ok_or("no MIT-SHM")?;
        conn.shm_query_version()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        conn.damage_query_version(1, 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        // Two ways to read the screen, chosen by what the X server is: an Xvfb run with
        // `-shmem` on this machine keeps its screen in a segment it made, read in place; any
        // other server is read with MIT-SHM GetImage. Why the first is not taken is said.
        let framebuffer = match debug.direct_fb {
            true => xserver_pid(display).and_then(|pid| xvfb_framebuffer(pid, width, height)),
            false => Err("VOXL_NO_DIRECT_FB is set".into()),
        };
        let (source, stride) = match framebuffer {
            Ok(Framebuffer { fb, offset, stride }) => {
                log::info!(
                    "capturing directly from the Xvfb framebuffer (shmid {})",
                    fb.shmid
                );
                (Source::InPlace { fb, offset }, stride)
            }
            Err(why) => {
                let stride = width as usize * 4;
                let buf = Made::new(stride * height as usize)?;
                let seg = conn.generate_id().map_err(|e| e.to_string())?;
                // A shmid shmget gave, so not negative.
                conn.shm_attach(seg, buf.0.shmid as u32, false)
                    .map_err(|e| e.to_string())?
                    .check()
                    .map_err(|e| e.to_string())?;
                // Marked for removal as soon as the server has it attached, so it goes
                // with the last of the two to detach, however this process ends.
                remove(buf.0.shmid);
                log::info!("capturing via MIT-SHM GetImage: {why}");
                (Source::GetImage { buf, seg }, stride)
            }
        };
        let damage = conn.generate_id().map_err(|e| e.to_string())?;
        conn.damage_create(damage, root, damage::ReportLevel::RAW_RECTANGLES)
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        Ok(Capturer {
            conn,
            root,
            width,
            height,
            resize_pending: false,
            stride,
            source,
            _damage: damage,
            damage: Damage::Unreported,
            debug,
            rows: crate::convert::Rows::new(convert_threads)?,
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    /// Drain queued X events; returns true if damage was reported.
    pub fn pump(&mut self) -> Result<(), String> {
        while let Some(ev) = self.conn.poll_for_event().map_err(|e| e.to_string())? {
            if let Event::ConfigureNotify(e) = &ev {
                if e.window == self.root
                    && (u32::from(e.width), u32::from(e.height)) != (self.width, self.height)
                {
                    self.resize_pending = true;
                }
            }
            if let Event::DamageNotify(e) = ev {
                if self.debug.damage {
                    log::info!(
                        "dmg {:?} x{} y{} {}x{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_micros()
                            % 1_000_000,
                        e.area.x,
                        e.area.y,
                        e.area.width,
                        e.area.height
                    );
                }
                let (top, bot) = on_screen(e.area.y, e.area.height, self.height);
                let (left, right) = on_screen(e.area.x, e.area.width, self.width);
                if bot > top && right > left {
                    self.damage.add(top, bot, std::time::Instant::now());
                }
            }
        }
        Ok(())
    }

    /// Take accumulated damage as a row band. Raw-rectangle reporting delivers the
    /// areas in the events themselves, so this never waits on the X server.
    pub fn take_damage(&mut self) -> Option<Band> {
        self.damage.take(self.height)
    }

    pub fn full(&self) -> Band {
        Band {
            y0: 0,
            y1: self.height,
        }
    }

    /// Convert rows [y0, y1) of the current frame into `dst`, on the capturer's threads.
    pub fn convert(
        &self,
        dst: &mut crate::convert::I420,
        y0: usize,
        y1: usize,
    ) -> Result<(), String> {
        self.rows.convert(self.pixels(), self.stride, dst, y0, y1)
    }

    /// The current frame as packed BGRX (`width * 4` bytes per row), for screenshots.
    /// Nothing draws while the server is grabbed (a few ms for the copy), so a repaint
    /// can't land in the pixels without its damage, or after them with it.
    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        self.conn.grab_server().map_err(|e| e.to_string())?;
        let shot = self.snapshot_grabbed();
        // Release the server even if the copy failed.
        self.conn.ungrab_server().map_err(|e| e.to_string())?;
        self.conn.flush().map_err(|e| e.to_string())?;
        shot
    }

    fn snapshot_grabbed(&mut self) -> Result<Vec<u8>, String> {
        // Damage for everything drawn before the grab arrives ahead of this reply.
        self.conn
            .get_input_focus()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        self.pump()?;
        self.grab(self.full())?;
        let (row, h) = (self.width as usize * 4, self.height as usize);
        let mut out = Vec::with_capacity(row * h);
        for line in self.pixels().chunks_exact(self.stride).take(h) {
            out.extend_from_slice(&line[..row]);
        }
        Ok(out)
    }

    /// The current frame as BGRX pixels and their stride (debug marker probe).
    pub fn bgrx(&self) -> (&[u8], usize) {
        (self.pixels(), self.stride)
    }

    /// Whether a root configure event changed the capture dimensions.
    pub fn needs_rebuild(&self) -> bool {
        self.resize_pending
    }

    /// A resize can race a GetImage request before its configure event is processed.
    pub fn geometry_changed(&self) -> Result<bool, String> {
        let g = self
            .conn
            .get_geometry(self.root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok((u32::from(g.width), u32::from(g.height)) != (self.width, self.height))
    }

    /// Current frame pixels (BGRX, `stride` bytes per row).
    fn pixels(&self) -> &[u8] {
        let (image, offset) = self.source.image();
        &image[offset..offset + self.stride * self.height as usize]
    }

    /// Grab a full-width band into the shm frame at its natural offset. Xvfb's own
    /// framebuffer is the screen already.
    pub fn grab(&self, band: Band) -> Result<(), String> {
        let seg = match &self.source {
            Source::InPlace { .. } => return Ok(()),
            Source::GetImage { seg, .. } => *seg,
        };
        let h = band.y1 - band.y0;
        if h == 0 {
            return Ok(());
        }
        let off = band.y0 as usize * self.stride;
        // X's coordinates and sizes are 16 bits, and a screen's segment under 4 GiB: a
        // band of the screen fits each.
        self.conn
            .shm_get_image(
                self.root,
                0,
                band.y0 as i16,
                self.width as u16,
                h as u16,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                seg,
                off as u32,
            )
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The part of X's range `at`..`at + len` (a damage rectangle's rows or columns) that is on a
/// screen `end` long, half-open: empty when none of it is.
fn on_screen(at: i16, len: u16, end: u32) -> (u32, u32) {
    let held = |v: i32| v.max(0).unsigned_abs().min(end);
    let at = i32::from(at);
    (held(at), held(at + i32::from(len)))
}

/// Pid of the local X server for `display` (":N" or "host:N.S"), from its lock file; or
/// why there is none to read: a display that is not this machine's server has no lock file
/// here.
fn xserver_pid(display: &str) -> Result<u32, String> {
    let n = match display.rsplit_once(':') {
        Some((_, after)) => match after.split_once('.') {
            Some((n, _)) => n,
            None => after,
        },
        None => return Err(format!("DISPLAY={display:?} names no display")),
    };
    let n: u32 = n
        .parse()
        .map_err(|_| format!("DISPLAY={display:?} names no display"))?;
    let lock = format!("/tmp/.X{n}-lock");
    let pid = std::fs::read_to_string(&lock).map_err(|e| format!("{lock}: {e}"))?;
    pid.trim()
        .parse()
        .map_err(|_| format!("{lock} holds no pid: {pid:?}"))
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]])
}

/// The XWD header's fields this reads (X11/XWDFile.h, each a big-endian CARD32): the
/// header's size, its version, the image's width and height, bits per pixel, bytes per
/// line, and the colour map's entries, each 12 bytes (`XWDColor`).
const XWD_HEADER_SIZE: usize = 0;
const XWD_FILE_VERSION: usize = 1;
const XWD_PIXMAP_WIDTH: usize = 4;
const XWD_PIXMAP_HEIGHT: usize = 5;
const XWD_BITS_PER_PIXEL: usize = 11;
const XWD_BYTES_PER_LINE: usize = 12;
const XWD_NCOLORS: usize = 19;
const XWD_VERSION: u32 = 7;
const XWD_COLOR: usize = 12;

/// Xvfb's framebuffer, attached: the screen begins `offset` bytes into it, `stride` bytes a
/// row.
struct Framebuffer {
    fb: ShmBuf,
    offset: usize,
    stride: usize,
}

/// The screen of the Xvfb whose pid is `pid`, if it runs with `-shmem`: Xvfb keeps each
/// screen as an XWD image in a SysV segment it made, and the segment its pid made whose
/// header is a 32-bit image the screen fits in is that screen. After a RandR resize the
/// header still describes the size Xvfb started with: the screen is the top-left
/// `width`x`height` of it, at the header's stride. Or why there is none.
fn xvfb_framebuffer(pid: u32, width: u32, height: u32) -> Result<Framebuffer, String> {
    const TABLE: &str = "/proc/sysvipc/shm";
    let table = std::fs::read_to_string(TABLE).map_err(|e| format!("{TABLE}: {e}"))?;
    // Columns of /proc/sysvipc/shm (proc(5)): key, shmid, perms, size, cpid, ...
    let rows = table
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>());
    rows.filter_map(|f| {
        let shmid = f.get(1)?.parse::<i32>().ok()?;
        let size = f.get(3)?.parse::<usize>().ok()?;
        let cpid = f.get(4)?.parse::<u32>().ok()?;
        (cpid == pid && size >= (width * height * 4) as usize).then_some((shmid, size))
    })
    .find_map(|(shmid, size)| {
        // A segment that cannot be attached is not one this process may read.
        let fb = ShmBuf::attach(shmid, size, libc::SHM_RDONLY).ok()?;
        let b = fb.as_slice();
        let (w, h, bpp, bpl) = (
            be32(b, XWD_PIXMAP_WIDTH),
            be32(b, XWD_PIXMAP_HEIGHT),
            be32(b, XWD_BITS_PER_PIXEL),
            be32(b, XWD_BYTES_PER_LINE) as usize,
        );
        let offset = be32(b, XWD_HEADER_SIZE) as usize + be32(b, XWD_NCOLORS) as usize * XWD_COLOR;
        let fits = be32(b, XWD_FILE_VERSION) == XWD_VERSION
            && w >= width
            && h >= height
            && bpp == 32
            && bpl >= (w * 4) as usize
            && offset + bpl * h as usize <= size;
        fits.then_some(Framebuffer {
            fb,
            offset,
            stride: bpl,
        })
    })
    .ok_or_else(|| format!("no segment of pid {pid} (Xvfb run without -shmem?) holds the screen"))
}
