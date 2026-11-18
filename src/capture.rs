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

pub struct ShmBuf {
    pub ptr: *mut u8,
    pub len: usize,
    shmid: i32,
    /// Attached to someone else's segment (never removed by us).
    foreign: bool,
}

unsafe impl Send for ShmBuf {}

impl ShmBuf {
    fn new(len: usize) -> Result<Self, String> {
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                return Err("shmget failed".into());
            }
            let ptr = libc::shmat(shmid, std::ptr::null(), 0);
            if ptr as isize == -1 {
                return Err("shmat failed".into());
            }
            Ok(ShmBuf {
                ptr: ptr as *mut u8,
                len,
                shmid,
                foreign: false,
            })
        }
    }

    fn attach_readonly(shmid: i32, len: usize) -> Option<Self> {
        unsafe {
            let ptr = libc::shmat(shmid, std::ptr::null(), libc::SHM_RDONLY);
            if ptr as isize == -1 {
                return None;
            }
            Some(ShmBuf {
                ptr: ptr as *mut u8,
                len,
                shmid,
                foreign: true,
            })
        }
    }
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.ptr as *const _);
            if !self.foreign {
                libc::shmctl(self.shmid, libc::IPC_RMID, std::ptr::null_mut());
            }
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
    seg: u32,
    buf: ShmBuf,
    /// Byte offset of pixel data inside `buf` (non-zero for an Xvfb XWD framebuffer).
    offset: usize,
    /// Reading Xvfb's framebuffer in place: no GetImage needed.
    pub direct: bool,
    /// Kept for the connection's lifetime; raw-rectangle events need no further requests.
    _damage: u32,
    /// Rows touched since the last take (raw damage rectangles, no server round trip).
    pub damage: Damage,
    /// Macroblocks (16x16) touched since the last take, row-major.
    mb_dirty: Vec<u8>,
    pub mb_cols: usize,
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
        let mut stride = width as usize * 4;
        let seg = conn.generate_id().map_err(|e| e.to_string())?;
        // Two ways to read the screen, chosen by what the X server is: an Xvfb run with
        // `-shmem` on this machine keeps its screen in a segment it made, read in place; any
        // other server is read with MIT-SHM GetImage.
        let framebuffer = match (debug.direct_fb, xserver_pid(display)) {
            (true, Some(pid)) => xvfb_framebuffer(pid, width, height),
            (true, None) | (false, _) => None,
        };
        let (buf, offset, direct) = match framebuffer {
            Some((buf, offset, fb_stride)) => {
                log::info!(
                    "capturing directly from the Xvfb framebuffer (shmid {})",
                    buf.shmid
                );
                stride = fb_stride;
                (buf, offset, true)
            }
            None => {
                let buf = ShmBuf::new(stride * height as usize)?;
                conn.shm_attach(seg, buf.shmid as u32, false)
                    .map_err(|e| e.to_string())?
                    .check()
                    .map_err(|e| e.to_string())?;
                // Mark for removal now; it stays alive while attached.
                unsafe { libc::shmctl(buf.shmid, libc::IPC_RMID, std::ptr::null_mut()) };
                log::info!("capturing via MIT-SHM GetImage");
                (buf, 0, false)
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
            seg,
            buf,
            offset,
            direct,
            _damage: damage,
            damage: Damage::Unreported,
            mb_dirty: vec![0; (width as usize).div_ceil(16) * (height as usize).div_ceil(16)],
            mb_cols: (width as usize).div_ceil(16),
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
                let top = (e.area.y as i32).clamp(0, self.height as i32) as u32;
                let bot =
                    (e.area.y as i32 + e.area.height as i32).clamp(0, self.height as i32) as u32;
                if bot > top {
                    let left = (e.area.x as i32).clamp(0, self.width as i32) as u32;
                    let right =
                        (e.area.x as i32 + e.area.width as i32).clamp(0, self.width as i32) as u32;
                    if right <= left {
                        continue;
                    }
                    let (x0, x1) = (left as usize / 16, (right as usize).div_ceil(16));
                    for row in (top as usize / 16)..(bot as usize).div_ceil(16) {
                        self.mb_dirty[row * self.mb_cols + x0..row * self.mb_cols + x1].fill(1);
                    }
                    self.damage.add(top, bot, std::time::Instant::now());
                }
            }
        }
        Ok(())
    }

    /// Take accumulated damage as a row band. Raw-rectangle reporting delivers the
    /// areas in the events themselves, so this never waits on the X server.
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        Ok(self.damage.take(self.height))
    }

    /// Take the dirty-macroblock map accumulated with the current damage.
    /// The macroblocks damaged since the last take; `fresh`, a map of none of them as
    /// long, takes its place.
    pub fn take_mb_map(&mut self, fresh: Vec<u8>) -> Vec<u8> {
        std::mem::replace(&mut self.mb_dirty, fresh)
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
    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        Some((self.pixels(), self.stride))
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
        &self.buf.as_slice()[self.offset..self.offset + self.stride * self.height as usize]
    }

    /// Grab a full-width band into the shm frame at its natural offset.
    pub fn grab(&self, band: Band) -> Result<(), String> {
        let h = band.y1 - band.y0;
        if h == 0 || self.direct {
            return Ok(());
        }
        let off = band.y0 as usize * self.stride;
        self.conn
            .shm_get_image(
                self.root,
                0,
                band.y0 as i16,
                self.width as u16,
                h as u16,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                self.seg,
                off as u32,
            )
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Pid of the local X server for `display` (":N" or "host:N.S"), from its lock file:
/// none for a display that is not this machine's server.
fn xserver_pid(display: &str) -> Option<u32> {
    let after = display.rsplit(':').next()?;
    let n: u32 = after.split('.').next()?.parse().ok()?;
    std::fs::read_to_string(format!("/tmp/.X{n}-lock"))
        .ok()?
        .trim()
        .parse()
        .ok()
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

/// The screen of the Xvfb whose pid is `pid`, if it runs with `-shmem`: Xvfb keeps each
/// screen as an XWD image in a SysV segment it made, and the segment its pid made whose
/// header is a 32-bit image the screen fits in is that screen. After a RandR resize the
/// header still describes the size Xvfb started with: the screen is the top-left
/// `width`x`height` of it, at the header's stride.
fn xvfb_framebuffer(pid: u32, width: u32, height: u32) -> Option<(ShmBuf, usize, usize)> {
    let table = std::fs::read_to_string("/proc/sysvipc/shm").ok()?;
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
        let buf = ShmBuf::attach_readonly(shmid, size)?;
        let b = buf.as_slice();
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
        fits.then_some((buf, offset, bpl))
    })
}
