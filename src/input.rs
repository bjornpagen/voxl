//! Input injection via XTEST. Keys arrive as keysyms; they are resolved against the
//! server keymap, falling back to temporarily remapped spare keycodes for symbols the
//! layout lacks or that need a shift level the client isn't holding.
//!
//! A held key repeats when the viewer's keyboard repeats it, never on the X server's
//! timer: that timer runs until the key-up arrives, so a key-up held up on the network
//! typed characters the viewer never sent. While voxl runs, no keyboard on its X server
//! autorepeats (see `repeat_off`).

use crate::event::{InputEvent, KeyAct, Press};
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Where a key's symbol is on its keycode, which decides the Shift it is pressed with.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Level {
    /// The same with Shift or without (function keys, a remapped spare keycode).
    Either,
    /// Without Shift: Shift held by the viewer is let go around the press.
    Plain,
    /// With Shift: Shift is pressed around it unless the viewer holds one.
    Shifted,
}

/// A key a viewer holds, as X has it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Held {
    /// In X's modifier map: pressed as it is, and never repeated (X never repeats one).
    Modifier(u8),
    /// Pressed, and pressed again on each repeat, with the Shift its symbol needs given
    /// the Shift keys held at that moment.
    Key { keycode: u8, level: Level },
}

impl Held {
    fn keycode(self) -> u8 {
        match self {
            Held::Modifier(keycode) | Held::Key { keycode, .. } => keycode,
        }
    }
}

/// One XTEST key event.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stroke {
    Press(u8),
    Release(u8),
}

/// What a viewer's key event does: the key events X gets, and what is held for its
/// keysym afterwards.
#[derive(Debug, PartialEq)]
struct Outcome {
    strokes: Vec<Stroke>,
    held: Option<Held>,
}

/// A viewer's key event, from what is held for its keysym. `find` names the key X has for
/// the symbol; it is asked only for a key going down that isn't held, since finding one
/// can remap a spare keycode. `shift` is the keycode pressed for a shifted symbol when the
/// viewer holds no Shift, and `shifts` the Shift keys it holds.
fn key_event(
    held: Option<Held>,
    act: KeyAct,
    find: impl FnOnce() -> Option<Held>,
    shift: u8,
    shifts: &[u8],
) -> Outcome {
    let nothing = |held| Outcome {
        strokes: vec![],
        held,
    };
    match (act, held) {
        (KeyAct::Down, None) => match find() {
            Some(key) => Outcome {
                strokes: press(key, shift, shifts),
                held: Some(key),
            },
            // Neither the layout nor a spare keycode can type it.
            None => nothing(None),
        },
        // Down already: a second viewer holding the same key, or a page that sends its
        // repeats as downs. X itself drops a second press of a key that is down.
        (KeyAct::Down, Some(_)) => nothing(held),
        // Its down named a symbol X cannot type, or it was let go (ReleaseAll).
        (KeyAct::Repeat, None) => nothing(None),
        // X never repeats a modifier; a repeat would be a release and a press of it.
        (KeyAct::Repeat, Some(Held::Modifier(_))) => nothing(held),
        // X takes no press from XTEST as a repeat (a press of a key that is down is
        // dropped), so a repeat is a release and a press.
        (KeyAct::Repeat, Some(key @ Held::Key { keycode, .. })) => Outcome {
            strokes: std::iter::once(Stroke::Release(keycode))
                .chain(press(key, shift, shifts))
                .collect(),
            held,
        },
        // As for a repeat of a key that isn't held.
        (KeyAct::Up, None) => nothing(None),
        (KeyAct::Up, Some(key)) => Outcome {
            strokes: vec![Stroke::Release(key.keycode())],
            held: None,
        },
    }
}

/// Presses `key` so it types its symbol, given the Shift keys held (`shifts`); `shift`
/// is the keycode pressed for a shifted symbol when none is.
fn press(key: Held, shift: u8, shifts: &[u8]) -> Vec<Stroke> {
    let (keycode, level) = match key {
        Held::Modifier(keycode) => (keycode, Level::Either),
        Held::Key { keycode, level } => (keycode, level),
    };
    match (level, shifts.is_empty()) {
        (Level::Either, _) | (Level::Plain, true) | (Level::Shifted, false) => {
            vec![Stroke::Press(keycode)]
        }
        (Level::Shifted, true) => vec![
            Stroke::Press(shift),
            Stroke::Press(keycode),
            Stroke::Release(shift),
        ],
        (Level::Plain, false) => shifts
            .iter()
            .map(|&s| Stroke::Release(s))
            .chain([Stroke::Press(keycode)])
            .chain(shifts.iter().map(|&s| Stroke::Press(s)))
            .collect(),
    }
}

/// Turns the X server's autorepeat off for every key, and returns the per-key bits it
/// found, to put back at shutdown.
///
/// The repeats of a key XTEST holds come from the core keyboard's timer, whose per-key
/// bits every keyboard attached to it shares: none of them repeats while voxl runs, and
/// after a crash they stay so until the X server restarts. The per-key bits rather than
/// the global switch or the RepeatKeys control, because desktop settings daemons
/// (xfsettingsd) turn those back on.
fn repeat_off(conn: &RustConnection) -> Result<[u8; 32], String> {
    let xkb = conn
        .xkb_use_extension(1, 0)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    if !xkb.supported {
        return Err(format!(
            "the X server's XKB is {}.{}, older than 1.0",
            xkb.server_major, xkb.server_minor
        ));
    }
    let found = conn
        .xkb_get_controls(xkb::ID::USE_CORE_KBD.into())
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .per_key_repeat;
    set_repeat(conn, &[0; 32]).map_err(|e| e.to_string())?;
    Ok(found)
}

/// Sets the core keyboard's per-key autorepeat bits, and no other control.
fn set_repeat(
    conn: &RustConnection,
    bits: &[u8; 32],
) -> Result<(), x11rb::errors::ConnectionError> {
    conn.send_trait_request_without_reply(xkb::SetControlsRequest {
        device_spec: xkb::ID::USE_CORE_KBD.into(),
        change_controls: xkb::Control::PER_KEY_REPEAT,
        per_key_repeat: std::borrow::Cow::Borrowed(bits),
        ..Default::default()
    })?;
    Ok(())
}

/// X keycodes (evdev + 8) with no entry in Chromium's DOM code table.
const SAFE_SCRATCH: &[u8] = &[
    92, 109, 120, 126, 138, 147, 149, 153, 154, 155, 156, 157, 159, 161, 162, 165, 168, 170, 178,
    183, 184, 185, 186, 203, 204, 205, 206, 207, 208, 210, 211, 213, 219, 220, 221, 222, 223, 224,
    226, 227, 228, 229, 230, 231, 234, 235, 244, 245, 246, 247, 248, 249, 250, 251, 253, 254, 255,
];

const SHIFT_L: u32 = 0xffe1;

/// Scroll units per wheel notch: the increment patches/xorg-server-xtest-scroll.patch
/// gives the XTEST pointer's scroll axes (`SetScrollValuator(..., 120.0, ...)`), which is
/// the unit of a detent hi-res wheels report in (libinput's `v120`: 120 a detent).
const NOTCH: f64 = 120.0;

/// How long a scratch keycode's new mapping is left to reach the desktop's clients before
/// its key is pressed: they must have read X's MappingNotify and fetched the new map, and
/// the protocol says to nobody when they have, so no event can end this wait. Provisional:
/// upstream's value, unmeasured; what would measure it is the bench's key test typing
/// unmapped keysyms into each app with shorter waits until one lands wrong.
const MAPPING_SETTLE: Duration = Duration::from_millis(8);

/// Desktop pixels one wheel notch scrolls: what turns a viewer's pixel scroll into
/// notches. Provisional: upstream's calibration of smooth scrolling against a browser
/// on the desktop, unmeasured here; what would measure it is a page scrolled by a known
/// number of pixels on the viewer, against the pixels it moves on the desktop.
const PX_PER_NOTCH: f64 = 180.0;

/// The most notches one pixel scroll turns, either way, on an X server without smooth
/// scrolling; the rest of a longer scroll is dropped.
///
/// Who needs it: X turns its wheel by a press and a release of a button per notch, so
/// this bounds what one message makes the input thread send X at 40 events.
///
/// What sets it: 93ad2d4 clamps every wheel message to 20 notches on X, macOS and
/// Windows, and the pixel-scroll fallback to wheel clicks keeps that clamp. A message is
/// what pointer.ts gathered in one animation frame; at the default 180 px a notch, 20
/// notches is 3,600 desktop pixels in a sixtieth of a second. Unmeasured beyond that:
/// half of it drops the rest of a flick above 1,800 px a frame, double lets one message
/// send X 80 events.
const MAX_NOTCHES: i8 = 20;

/// Smooth scrolling through relative motion on the XTEST pointer's scroll axes.
/// Stock X servers give that device no scroll axes; Xvfb built with
/// patches/xorg-server-xtest-scroll.patch does.
struct SmoothScroll {
    device: u16,
    vaxis: u8,
    haxis: u8,
    xtest_opcode: u8,
    xi_first_event: u8,
}

impl SmoothScroll {
    /// The XTEST pointer's scroll axes, if the X server gives it some (an Xvfb with
    /// patches/xorg-server-xtest-scroll.patch does, a stock server does not): none is a
    /// server that scrolls by wheel clicks; a failed request is the error.
    fn detect(conn: &RustConnection) -> Result<Option<Self>, String> {
        use x11rb::connection::RequestConnection;
        use x11rb::protocol::xinput::{self, ConnectionExt as _, DeviceClassData, ScrollType};
        let e = |e: &dyn std::fmt::Display| format!("smooth scrolling: {e}");
        let extension = |name| conn.extension_information(name).map_err(|x| e(&x));
        let (Some(xtest), Some(xi)) = (
            extension(x11rb::protocol::xtest::X11_EXTENSION_NAME)?,
            extension(xinput::X11_EXTENSION_NAME)?,
        ) else {
            return Ok(None);
        };
        conn.xinput_xi_query_version(2, 2)
            .map_err(|x| e(&x))?
            .reply()
            .map_err(|x| e(&x))?;
        let reply = conn
            .xinput_xi_query_device(xinput::Device::ALL)
            .map_err(|x| e(&x))?
            .reply()
            .map_err(|x| e(&x))?;
        for dev in reply.infos {
            if !String::from_utf8_lossy(&dev.name).contains("XTEST pointer") {
                continue;
            }
            let (mut v, mut h) = (None, None);
            for class in &dev.classes {
                if let DeviceClassData::Scroll(sc) = &class.data {
                    match sc.scroll_type {
                        ScrollType::VERTICAL => v = Some(sc.number as u8),
                        ScrollType::HORIZONTAL => h = Some(sc.number as u8),
                        _ => {}
                    }
                }
            }
            if let (Some(vaxis), Some(haxis)) = (v.filter(|&a| a < 6), h.filter(|&a| a < 6)) {
                return Ok(Some(SmoothScroll {
                    device: dev.deviceid,
                    vaxis,
                    haxis,
                    xtest_opcode: xtest.major_opcode,
                    xi_first_event: xi.first_event,
                }));
            }
        }
        Ok(None)
    }

    /// XTestFakeInput with an XI DeviceMotionNotify (relative) followed by a
    /// DeviceValuator event carrying the scroll deltas. x11rb's FakeInput helper
    /// can't attach valuator events, so the request is assembled here.
    fn send(&self, conn: &RustConnection, vertical: i32, horizontal: i32) {
        use x11rb::connection::RequestConnection;
        // Always start at axis 0: XTest stores valuators at absolute indices but
        // builds the event mask relative to the first one, so a non-zero first
        // valuator loses the values. x/y get relative 0 (no pointer motion).
        let n = self.vaxis.max(self.haxis) as usize + 1;
        let mut vals = [0i32; 6];
        vals[self.vaxis as usize] = vertical;
        vals[self.haxis as usize] = horizontal;
        let mut req = Vec::with_capacity(4 + 64);
        req.extend_from_slice(&[self.xtest_opcode, 2, 17, 0]); // FakeInput, 68 bytes = 17 words
        let mut ev = [0u8; 32];
        ev[0] = self.xi_first_event + 5; // XI DeviceMotionNotify
        ev[1] = 1; // relative
        ev[31] = self.device as u8;
        req.extend_from_slice(&ev);
        let mut dv = [0u8; 32];
        dv[0] = self.xi_first_event; // XI DeviceValuator
        dv[1] = self.device as u8;
        dv[6] = n as u8; // num_valuators
        dv[7] = 0; // first_valuator
        for (i, v) in vals.iter().take(n).enumerate() {
            dv[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_ne_bytes());
        }
        req.extend_from_slice(&dv);
        sent(conn.send_request_without_reply(&[std::io::IoSlice::new(&req)], vec![]));
    }
}
const SHIFT_R: u32 = 0xffe2;

struct Injector {
    conn: RustConnection,
    root: u32,
    min_kc: u8,
    per: usize,
    syms: Vec<u32>,
    lookup: HashMap<u32, (u8, usize)>,
    scratch: Vec<u8>,
    /// keysym -> scratch keycode currently mapped (with last-use time).
    scratch_map: HashMap<u32, (u8, Instant)>,
    /// Keycodes in X's modifier map.
    modifiers: Vec<u8>,
    /// The per-key autorepeat bits as voxl found them, put back at shutdown.
    repeat: [u8; 32],
    /// keysym -> the key a viewer holds for it.
    pressed: HashMap<u32, Held>,
    buttons: [Press; 16],
    clipboard: crate::desktop::ClipboardHandle,
    smooth: Option<SmoothScroll>,
    /// Scroll units (120 per notch) not yet sent: fractions, or partial notches
    /// when falling back to wheel clicks.
    scroll_acc: (f64, f64),
    shift_kc: u8,
    scratch_path: std::path::PathBuf,
}

impl Injector {
    fn new(
        display: &str,
        keymap: std::path::PathBuf,
        clipboard: crate::desktop::ClipboardHandle,
    ) -> Result<Self, String> {
        let (conn, n) = RustConnection::connect(Some(display)).map_err(|e| e.to_string())?;
        let root = conn.setup().roots[n].root;
        let min_kc = conn.setup().min_keycode;
        let max_kc = conn.setup().max_keycode;
        let repeat = repeat_off(&conn)?;
        let mut me = Injector {
            conn,
            root,
            min_kc,
            per: 0,
            syms: vec![],
            lookup: HashMap::new(),
            scratch: vec![],
            scratch_map: HashMap::new(),
            modifiers: vec![],
            repeat,
            pressed: HashMap::new(),
            buttons: [Press::Up; 16],
            clipboard,
            smooth: None,
            scroll_acc: (0.0, 0.0),
            shift_kc: 0,
            scratch_path: keymap,
        };
        // Undo remaps left behind by a previous run that didn't exit cleanly.
        // A missing file is a clean exit before: nothing to undo. Keycodes in it are the
        // ones a voxl wrote; one that is not a keycode of this server is passed over.
        match std::fs::read_to_string(&me.scratch_path) {
            Ok(old) => {
                me.load_keymap(max_kc)?;
                for kc in old.split_whitespace().filter_map(|k| k.parse::<u8>().ok()) {
                    if kc >= min_kc && kc <= max_kc {
                        sent(me.conn.change_keyboard_mapping(
                            1,
                            kc,
                            me.per as u8,
                            &vec![0; me.per],
                        ));
                    }
                }
                me.conn
                    .sync()
                    .map_err(|e| format!("the keymap left behind: {e}"))?;
                std::fs::remove_file(&me.scratch_path)
                    .map_err(|e| format!("{}: {e}", me.scratch_path.display()))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", me.scratch_path.display())),
        }
        me.load_keymap(max_kc)?;
        me.shift_kc = match me.lookup.get(&SHIFT_L) {
            Some(&(k, _)) => k,
            None => return Err("the keymap has no Shift_L".into()),
        };
        me.smooth = SmoothScroll::detect(&me.conn)?;
        match &me.smooth {
            Some(s) => log::info!(
                "smooth scrolling via XTEST device {} (axes {}/{})",
                s.device,
                s.vaxis,
                s.haxis
            ),
            None => log::info!("XTEST pointer has no scroll axes; scrolling with wheel clicks"),
        }
        Ok(me)
    }

    fn load_keymap(&mut self, max_kc: u8) -> Result<(), String> {
        let count = max_kc - self.min_kc + 1;
        let r = self
            .conn
            .get_keyboard_mapping(self.min_kc, count)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        self.per = r.keysyms_per_keycode as usize;
        self.syms = r.keysyms;
        // Eight modifiers' keycodes, 0 where a modifier has fewer than the most.
        self.modifiers = self
            .conn
            .get_modifier_mapping()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .keycodes
            .into_iter()
            .filter(|&kc| kc != 0)
            .collect();
        self.lookup.clear();
        self.scratch.clear();
        for i in 0..count as usize {
            let kc = self.min_kc + i as u8;
            let row = &self.syms[i * self.per..(i + 1) * self.per];
            if row.iter().all(|&s| s == 0) {
                self.scratch.push(kc);
                continue;
            }
            // Only the first two levels (plain, shift) are reachable without extra modifiers.
            for (lvl, &s) in row.iter().take(2).enumerate() {
                if s != 0 {
                    self.lookup.entry(s).or_insert((kc, lvl));
                }
            }
        }
        // Only use keycodes whose evdev code browsers/Electron don't map to a physical key:
        // apps derive KeyboardEvent.code (and shortcuts like New Tab, Enter, media keys)
        // from the keycode, so a remapped "New" key would open tabs instead of typing.
        self.scratch.retain(|k| SAFE_SCRATCH.contains(k));
        self.scratch.reverse();
        log::info!(
            "keymap: {} keysyms, {} scratch keycodes",
            self.lookup.len(),
            self.scratch.len()
        );
        Ok(())
    }

    fn row(&self, kc: u8) -> &[u32] {
        let i = (kc - self.min_kc) as usize;
        &self.syms[i * self.per..(i + 1) * self.per]
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) {
        sent(
            self.conn
                .xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, self.root, x, y, 0),
        );
    }

    /// The keycode that types `keysym` and the level it is on, if X has one or a spare
    /// keycode can be mapped to it.
    fn resolve(&mut self, keysym: u32) -> Option<(u8, Level)> {
        if let Some(&(kc, lvl)) = self.lookup.get(&keysym) {
            let row = self.row(kc);
            let l0 = row[0];
            let l1 = if row.len() > 1 { row[1] } else { 0 };
            let shift_neutral = l1 == 0 || l0 == l1 || (0xfd00..0x10000).contains(&keysym);
            let level = match (shift_neutral, lvl) {
                (true, _) => Level::Either,
                (false, 0) => Level::Plain,
                (false, _) => Level::Shifted,
            };
            return Some((kc, level));
        }
        if let Some((kc, t)) = self.scratch_map.get_mut(&keysym) {
            *t = Instant::now();
            return Some((*kc, Level::Either));
        }
        if self.scratch.is_empty() {
            return None;
        }
        // Map the keysym onto a spare keycode at every level: a free one, else the one
        // used longest ago.
        let kc = match self.scratch.get(self.scratch_map.len()) {
            Some(&free) => free,
            None => {
                let oldest = self.scratch_map.iter().min_by_key(|(_, (_, t))| *t);
                let (&old, &(kc, _)) = oldest?;
                self.scratch_map.remove(&old);
                kc
            }
        };
        let row = vec![keysym; self.per];
        let mapped = self
            .conn
            .change_keyboard_mapping(1, kc, self.per as u8, &row)
            .map_err(x11rb::errors::ReplyError::from)
            .and_then(|_| self.conn.sync());
        if let Err(e) = mapped {
            log::error!("keysym {keysym:#x} not mapped: {e}");
            return None;
        }
        // Clients must process MappingNotify before they see the key.
        std::thread::sleep(MAPPING_SETTLE);
        let i = (kc - self.min_kc) as usize;
        let per = self.per;
        self.syms[i * per..(i + 1) * per].copy_from_slice(&row);
        self.scratch_map.insert(keysym, (kc, Instant::now()));
        let used: Vec<String> = self
            .scratch_map
            .values()
            .map(|(k, _)| k.to_string())
            .collect();
        // Without it, a crash leaves this mapping for the next voxl not to undo: said.
        if let Err(e) = crate::session::private_file(&self.scratch_path, used.join(" ").as_bytes())
        {
            log::warn!("{}: {e}", self.scratch_path.display());
        }
        Some((kc, Level::Either))
    }

    /// The keycodes of the Shift keys viewers hold.
    fn shifts(&self) -> Vec<u8> {
        [SHIFT_L, SHIFT_R]
            .iter()
            .filter_map(|k| self.pressed.get(k))
            .map(|h| h.keycode())
            .collect()
    }

    fn stroke(&self, stroke: Stroke) {
        match stroke {
            Stroke::Press(kc) => self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0),
            Stroke::Release(kc) => self.fake(xproto::KEY_RELEASE_EVENT, kc, 0, 0),
        }
    }

    /// The key X has for `keysym`, if the layout or a spare keycode can type it.
    fn find(&mut self, keysym: u32) -> Option<Held> {
        let (keycode, level) = self.resolve(keysym)?;
        Some(if self.modifiers.contains(&keycode) {
            Held::Modifier(keycode)
        } else {
            Held::Key { keycode, level }
        })
    }

    fn key(&mut self, keysym: u32, act: KeyAct) {
        let held = self.pressed.get(&keysym).copied();
        let (shift, shifts) = (self.shift_kc, self.shifts());
        let out = key_event(held, act, || self.find(keysym), shift, &shifts);
        match out.held {
            Some(held) => self.pressed.insert(keysym, held),
            None => self.pressed.remove(&keysym),
        };
        for s in out.strokes {
            self.stroke(s);
        }
    }

    fn restore_keymap(&mut self) {
        for (_, (kc, _)) in std::mem::take(&mut self.scratch_map) {
            sent(
                self.conn
                    .change_keyboard_mapping(1, kc, self.per as u8, &vec![0; self.per]),
            );
        }
        if let Err(e) = self.conn.sync() {
            log::error!("the keymap not put back: {e}");
        }
        match std::fs::remove_file(&self.scratch_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::error!("{}: {e}", self.scratch_path.display()),
        }
    }

    /// Turns the wheel by `notches`, a press and a release of one of X's wheel buttons
    /// for each: the first of `(back, forth)` for a negative count (4 up, 6 left), the
    /// second for a positive one (5 down, 7 right).
    fn wheel(&self, notches: i8, (back, forth): (u8, u8)) {
        let button = if notches < 0 { back } else { forth };
        for _ in 0..notches.unsigned_abs() {
            self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0);
            self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0);
        }
    }

    fn handle(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Move { x, y } => self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y),
            InputEvent::Button { button, press } => {
                let held = &mut self.buttons[usize::from(button.number())];
                if *held == press {
                    return;
                }
                *held = press;
                self.fake(
                    match press {
                        Press::Down => xproto::BUTTON_PRESS_EVENT,
                        Press::Up => xproto::BUTTON_RELEASE_EVENT,
                    },
                    button.number(),
                    0,
                    0,
                )
            }
            InputEvent::ScrollPx { dx, dy } => {
                let k = NOTCH / PX_PER_NOTCH;
                self.scroll_acc.0 += dx.get() * k;
                self.scroll_acc.1 += dy.get() * k;
                if let Some(sm) = &self.smooth {
                    let (ux, uy) = (self.scroll_acc.0.trunc(), self.scroll_acc.1.trunc());
                    if ux != 0.0 || uy != 0.0 {
                        self.scroll_acc.0 -= ux;
                        self.scroll_acc.1 -= uy;
                        sm.send(&self.conn, uy as i32, ux as i32);
                    }
                } else {
                    // Whole notches only; the remainder carries into the next gesture event.
                    // One turns at most MAX_NOTCHES each way, and the rest of a longer
                    // scroll is dropped.
                    let (nx, ny) = (
                        (self.scroll_acc.0 / NOTCH).trunc(),
                        (self.scroll_acc.1 / NOTCH).trunc(),
                    );
                    self.scroll_acc.0 -= nx * NOTCH;
                    self.scroll_acc.1 -= ny * NOTCH;
                    let most = f64::from(MAX_NOTCHES);
                    self.wheel(ny.clamp(-most, most) as i8, (4, 5));
                    self.wheel(nx.clamp(-most, most) as i8, (6, 7));
                }
            }
            InputEvent::Key { keysym, act } => self.key(keysym, act),
            InputEvent::Clipboard(text) => {
                self.clipboard.set_blocking(text);
            }
            InputEvent::Shutdown(done) => {
                self.handle(InputEvent::ReleaseAll);
                self.restore_keymap();
                if let Err(e) = set_repeat(&self.conn, &self.repeat) {
                    log::error!("key repeat not put back: {e}");
                }
                if let Err(e) = self.conn.sync() {
                    log::error!("the X server took no last input: {e}");
                }
                // The daemon's stop waits on this; gone, it waits for nothing.
                if done.send(()).is_err() {
                    log::info!("the daemon stopped before the input was let go");
                }
            }
            InputEvent::ReleaseAll => {
                for (_, held) in std::mem::take(&mut self.pressed) {
                    self.stroke(Stroke::Release(held.keycode()));
                }
                for b in 1..self.buttons.len() {
                    if self.buttons[b] == Press::Down {
                        self.buttons[b] = Press::Up;
                        self.fake(xproto::BUTTON_RELEASE_EVENT, b as u8, 0, 0);
                    }
                }
            }
        }
    }
}

impl crate::event::Inject for Injector {
    fn inject(&mut self, ev: InputEvent) {
        self.handle(ev);
    }

    /// X keeps requests in the connection's buffer until it is flushed.
    fn flush(&mut self) {
        sent(self.conn.flush());
    }
}

/// A request handed to the X server: a connection that fails is the X server gone, and
/// the daemon ends with it (its service manager starts it again with the desktop).
fn sent<T, E: std::fmt::Display>(r: Result<T, E>) -> T {
    match r {
        Ok(t) => t,
        Err(e) => {
            log::error!("input's X connection: {e}");
            crate::logging::exit(1)
        }
    }
}

/// `keymap` is the file of the scratch keycodes voxl has remapped, in the runtime folder:
/// what a voxl that did not exit cleanly left there is undone at the next start.
pub fn spawn(
    display: &str,
    clipboard: crate::desktop::ClipboardHandle,
    keymap: std::path::PathBuf,
) -> Result<mpsc::Sender<InputEvent>, String> {
    let inj = Injector::new(display, keymap, clipboard)?;
    crate::event::spawn(move || inj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Stroke::{Press, Release};

    const A: u8 = 38;
    const ONE: u8 = 10;
    const SHIFT_L_KC: u8 = 50;
    const SHIFT_R_KC: u8 = 62;
    const PLAIN: Held = Held::Key {
        keycode: A,
        level: Level::Plain,
    };
    const SHIFTED: Held = Held::Key {
        keycode: ONE,
        level: Level::Shifted,
    };
    const SHIFT: Held = Held::Modifier(SHIFT_L_KC);

    fn event(held: Option<Held>, act: KeyAct, found: Option<Held>, shifts: &[u8]) -> Outcome {
        key_event(held, act, || found, SHIFT_L_KC, shifts)
    }

    fn outcome(strokes: &[Stroke], held: Option<Held>) -> Outcome {
        Outcome {
            strokes: strokes.to_vec(),
            held,
        }
    }

    /// Every cell of the table: down, repeat and up, against a key that isn't held, a held
    /// key and a held modifier.
    #[test]
    fn each_key_event_does_what_the_table_says() {
        let down = KeyAct::Down;
        let repeat = KeyAct::Repeat;
        let up = KeyAct::Up;
        assert_eq!(
            event(None, down, Some(PLAIN), &[]),
            outcome(&[Press(A)], Some(PLAIN))
        );
        assert_eq!(event(None, down, None, &[]), outcome(&[], None));
        assert_eq!(
            event(Some(PLAIN), down, None, &[]),
            outcome(&[], Some(PLAIN))
        );
        assert_eq!(
            event(Some(SHIFT), down, None, &[]),
            outcome(&[], Some(SHIFT))
        );
        assert_eq!(event(None, repeat, None, &[]), outcome(&[], None));
        assert_eq!(
            event(Some(PLAIN), repeat, None, &[]),
            outcome(&[Release(A), Press(A)], Some(PLAIN))
        );
        assert_eq!(
            event(Some(SHIFT), repeat, None, &[SHIFT_L_KC]),
            outcome(&[], Some(SHIFT))
        );
        assert_eq!(event(None, up, None, &[]), outcome(&[], None));
        assert_eq!(
            event(Some(PLAIN), up, None, &[]),
            outcome(&[Release(A)], None)
        );
        assert_eq!(
            event(Some(SHIFT), up, None, &[SHIFT_L_KC]),
            outcome(&[Release(SHIFT_L_KC)], None)
        );
    }

    /// A symbol's Shift is decided by the Shift keys held when it is pressed, so a repeat
    /// after the viewer pressed or let go of Shift still types the same symbol.
    #[test]
    fn a_key_is_pressed_with_the_shift_its_symbol_needs() {
        let either = Held::Key {
            keycode: A,
            level: Level::Either,
        };
        let shifts = [SHIFT_L_KC, SHIFT_R_KC];
        assert_eq!(press(either, SHIFT_L_KC, &[]), [Press(A)]);
        assert_eq!(press(either, SHIFT_L_KC, &shifts), [Press(A)]);
        assert_eq!(press(PLAIN, SHIFT_L_KC, &[]), [Press(A)]);
        assert_eq!(
            press(PLAIN, SHIFT_L_KC, &shifts),
            [
                Release(SHIFT_L_KC),
                Release(SHIFT_R_KC),
                Press(A),
                Press(SHIFT_L_KC),
                Press(SHIFT_R_KC)
            ]
        );
        assert_eq!(
            press(SHIFTED, SHIFT_L_KC, &[]),
            [Press(SHIFT_L_KC), Press(ONE), Release(SHIFT_L_KC)]
        );
        assert_eq!(press(SHIFTED, SHIFT_L_KC, &[SHIFT_R_KC]), [Press(ONE)]);
        assert_eq!(press(SHIFT, SHIFT_L_KC, &[SHIFT_R_KC]), [Press(SHIFT_L_KC)]);
    }

    /// Every sequence of up to six key events over 'a', '!' (shifted, on the '1' key) and
    /// Shift, carried out as `Injector::key` does. The oracle is a model with no `Held`
    /// in it: a key is held from a down until its up. After every event X's keys are
    /// valid (nothing pressed twice or released while up) and exactly the held keys are
    /// down; and a key's own keycode is pressed once for a down of a key not held and
    /// once for a repeat of a held key that isn't a modifier, never otherwise.
    #[test]
    fn x_holds_exactly_what_the_viewer_holds_and_types_each_repeat_once() {
        const KEYS: [(u32, Held); 3] = [(0x61, PLAIN), (0x21, SHIFTED), (SHIFT_L, SHIFT)];
        const ACTS: [KeyAct; 3] = [KeyAct::Down, KeyAct::Repeat, KeyAct::Up];

        #[derive(Clone, Default)]
        struct State {
            pressed: HashMap<u32, Held>,
            /// The model: the keysyms the viewer holds.
            viewer: Vec<u32>,
            /// The keycodes X has down, from the key events sent.
            x: Vec<u8>,
        }

        fn walk(state: &State, depth: usize, events: &mut Vec<(u32, KeyAct)>, seen: &mut usize) {
            if depth == 0 {
                return;
            }
            for (keysym, key) in KEYS {
                for act in ACTS {
                    let mut st = state.clone();
                    events.push((keysym, act));
                    *seen += 1;
                    let shifts: Vec<u8> = [SHIFT_L, SHIFT_R]
                        .iter()
                        .filter_map(|k| st.pressed.get(k))
                        .map(|h| h.keycode())
                        .collect();
                    let held = st.pressed.get(&keysym).copied();
                    let out = key_event(held, act, || Some(key), SHIFT_L_KC, &shifts);
                    match out.held {
                        Some(h) => st.pressed.insert(keysym, h),
                        None => st.pressed.remove(&keysym),
                    };
                    for &s in &out.strokes {
                        match s {
                            Press(kc) => {
                                assert!(!st.x.contains(&kc), "{kc} pressed twice: {events:?}");
                                st.x.push(kc);
                            }
                            Release(kc) => {
                                assert!(st.x.contains(&kc), "{kc} released while up: {events:?}");
                                st.x.retain(|&k| k != kc);
                            }
                        }
                    }
                    let was_held = st.viewer.contains(&keysym);
                    match act {
                        KeyAct::Down if !was_held => st.viewer.push(keysym),
                        KeyAct::Up => st.viewer.retain(|&k| k != keysym),
                        _ => {}
                    }
                    let mut want: Vec<u8> = st
                        .viewer
                        .iter()
                        .map(|k| KEYS.iter().find(|(ks, _)| ks == k).unwrap().1.keycode())
                        .collect();
                    let mut got = st.x.clone();
                    want.sort();
                    got.sort();
                    assert_eq!(got, want, "X's keys after {events:?}");
                    let typed = out.strokes.iter().filter(|&&s| s == Press(key.keycode()));
                    let once = match (act, key) {
                        (KeyAct::Down, _) => !was_held,
                        (KeyAct::Repeat, Held::Key { .. }) => was_held,
                        (KeyAct::Repeat, Held::Modifier(_)) | (KeyAct::Up, _) => false,
                    };
                    assert_eq!(typed.count(), usize::from(once), "{events:?}");
                    walk(&st, depth - 1, events, seen);
                    events.pop();
                }
            }
        }

        let mut seen = 0;
        walk(&State::default(), 6, &mut Vec::new(), &mut seen);
        assert_eq!(seen, (1..=6).map(|n| 9usize.pow(n)).sum::<usize>());
    }
}
