//! Input layer for DrDrUI Tier 2.
//!
//! Linux exposes keyboards and other input devices through `evdev`:
//! every device has a node at `/dev/input/eventN` from which we can
//! `read(2)` a stream of fixed-size `struct input_event` records. The
//! kernel takes care of debouncing and key repeat — we just translate
//! the raw codes into a [`KeyCode`] and emit one [`Event`] per real
//! key press.
//!
//! The mapping table here covers what a small TUI / GUI app needs:
//! arrows, Enter, Esc, Tab, Backspace, Space, Home/End/PageUp/PageDown,
//! and the US-QWERTY letters / digits / a handful of symbols. Anything
//! we don't recognise comes back as [`KeyCode::Other`] so apps can
//! ignore it without confusion.
//!
//! Modifier handling is intentionally minimal at Tier 2 — we track
//! Shift only so letters can come through as upper/lower-case. Ctrl
//! and Alt are reported as bare modifier presses but not folded into
//! [`KeyCode::Char`]. Tier 3 will revisit when we have a real
//! compose / keymap layer.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

// ─── Public types ────────────────────────────────────────────────────

/// One logical UI event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Key(KeyCode),
    /// Inserted by event-loop machinery; widgets usually ignore it.
    Tick,
}

/// A single key press, decoded from one or more raw evdev records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Escape,
    Tab,
    BackTab, // Shift+Tab
    /// Alt+Tab — the window-cycling chord. Emitted instead of `Tab`
    /// while an Alt key is held, so a window manager can switch focus
    /// without inventing its own modifier bookkeeping.
    AltTab,
    /// The Super / Meta / Windows key, pressed on its own — opens the
    /// Start menu (Windows convention; macOS uses Cmd-Space, GNOME uses
    /// Super-S, we pick the most familiar default).
    Super,
    /// Super + arrow key chords for window snapping. The WM resizes the
    /// focused window to the workarea's left half / right half / full /
    /// restores it, matching Win + arrows on every modern OS.
    SnapLeft,
    SnapRight,
    SnapUp,
    SnapDown,
    /// F1 — open the keyboard shortcut help overlay.
    Help,
    Backspace,
    Space,
    /// A printable ASCII character with Shift state already applied.
    Char(char),
    /// Catch-all for keys we haven't mapped.
    Other,
}

/// What a widget did with an event: did it consume it, or pass through?
/// Containers use this to decide whether to give the event to siblings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventResponse {
    Consumed,
    Passthrough,
}

// ─── Raw input_event struct ──────────────────────────────────────────

/// 24-byte record the kernel writes to /dev/input/eventN on x86_64
/// Linux. We mirror the layout from `linux/input.h` with `#[repr(C)]`
/// so a single `read_exact` lands the fields in the right slots.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

const EV_KEY: u16 = 0x01;

const KEY_ESC: u16 = 1;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_ENTER: u16 = 28;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_SPACE: u16 = 57;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_END: u16 = 107;
const KEY_DOWN: u16 = 108;
const KEY_PAGEUP: u16 = 104;
const KEY_PAGEDOWN: u16 = 109;
const KEY_HOME: u16 = 102;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
const KEY_F1: u16 = 59;

// Mouse / pointer event types and codes (linux/input-event-codes.h).
// A mouse reports motion as *relative* deltas: an `EV_REL` record with
// code `REL_X`/`REL_Y` and `value` = how far it moved since the last
// record. Buttons are `EV_KEY` like keyboard keys, just with codes in
// the `BTN_*` range. Every batch of records the kernel emits for one
// physical movement ends with an `EV_SYN`/`SYN_REPORT` marker — that's
// our cue to flush the accumulated delta as one logical move.
const EV_SYN: u16 = 0x00;
const EV_REL: u16 = 0x02;
/// Absolute axes — what a touchscreen / tablet reports instead of deltas.
/// The Surface Go 2 has no mouse; the panel is the pointer, so we must
/// decode this or the machine has no usable input at all.
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
/// Multitouch position (type-B devices). We only track the first finger.
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
/// Finger-on-glass for a touchscreen — we map it to the left button so a
/// tap is a click and a drag moves windows, no mouse required.
const BTN_TOUCH: u16 = 0x14a;

/// `struct input_absinfo` (linux/input.h) — the calibration for one
/// absolute axis. We only need `minimum`/`maximum` to map raw device
/// units onto screen pixels.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

// EVIOCGABS(abs) = _IOR('E', 0x40 + abs, struct input_absinfo). nix's
// `ioctl_read!` computes the modern `_IOC` request number for us; we
// need one per axis code we query.
nix::ioctl_read!(ev_get_abs_x, b'E', 0x40 + 0x00, InputAbsinfo);
nix::ioctl_read!(ev_get_abs_y, b'E', 0x40 + 0x01, InputAbsinfo);
nix::ioctl_read!(ev_get_abs_mt_x, b'E', 0x40 + 0x35, InputAbsinfo);
nix::ioctl_read!(ev_get_abs_mt_y, b'E', 0x40 + 0x36, InputAbsinfo);

// ─── KeyReader ───────────────────────────────────────────────────────

/// Opens an evdev device and yields [`Event`]s. Holds the Shift / Alt /
/// Super / Ctrl state across calls so chords (Alt-Tab, Super-Arrow,
/// Super alone) come out as the right [`KeyCode`].
pub struct KeyReader {
    file: File,
    shift: bool,
    alt: bool,
    /// Super / Meta / Windows key currently held.
    super_: bool,
    ctrl: bool,
    /// Has the Super key been used in a chord since it was pressed?
    /// If `false` when Super releases, we emit `KeyCode::Super` (open
    /// Start menu); if `true`, the chord already fired and the release
    /// is silent. Matches how Windows / GNOME handle the Super key.
    super_chorded: bool,
}

impl KeyReader {
    /// Open `/dev/input/eventN` (or any path the caller provides).
    /// Requires read permission on the device — typically that means
    /// running as root or being in the `input` group.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self {
            file,
            shift: false,
            alt: false,
            super_: false,
            ctrl: false,
            super_chorded: false,
        })
    }

    /// Borrow the device fd so [`InputHub`] can `poll` keyboard and
    /// mouse together without consuming the reader.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Read **exactly one** evdev record and decode it. Returns
    /// `Ok(Some(key))` for a real press, `Ok(None)` for anything that
    /// isn't one yet (key-up, a modifier, an unmapped code). Unlike
    /// [`next_event`](Self::next_event) this never loops, so a caller
    /// that already knows (via `poll`) the fd is readable can drain it
    /// one record at a time without risking a block.
    pub fn decode_one(&mut self) -> io::Result<Option<KeyCode>> {
        let mut raw = [0u8; 24];
        self.file.read_exact(&mut raw)?;
        let ev = parse_input_event(&raw);
        if ev.type_ != EV_KEY {
            return Ok(None);
        }
        // Modifier tracking — note that Super has the special "open Start
        // on release if not chorded" behaviour everyone is used to.
        match ev.code {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                self.shift = ev.value != 0;
                return Ok(None);
            }
            KEY_LEFTALT | KEY_RIGHTALT => {
                self.alt = ev.value != 0;
                return Ok(None);
            }
            KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                self.ctrl = ev.value != 0;
                return Ok(None);
            }
            KEY_LEFTMETA | KEY_RIGHTMETA => {
                if ev.value != 0 {
                    self.super_ = true;
                    self.super_chorded = false;
                    return Ok(None);
                }
                // Release: fire Super on tap, silent if chord already used.
                self.super_ = false;
                let chorded = std::mem::take(&mut self.super_chorded);
                return Ok(if chorded { None } else { Some(KeyCode::Super) });
            }
            _ => {}
        }
        if ev.value == 0 {
            return Ok(None); // release
        }
        // Super + arrow → snap chords.
        if self.super_ {
            let snap = match ev.code {
                KEY_LEFT => Some(KeyCode::SnapLeft),
                KEY_RIGHT => Some(KeyCode::SnapRight),
                KEY_UP => Some(KeyCode::SnapUp),
                KEY_DOWN => Some(KeyCode::SnapDown),
                _ => None,
            };
            if let Some(k) = snap {
                self.super_chorded = true;
                return Ok(Some(k));
            }
        }
        // Alt+Tab is a chord the WM wants as one logical key.
        if ev.code == KEY_TAB && self.alt {
            return Ok(Some(KeyCode::AltTab));
        }
        // F1 → help overlay.
        if ev.code == KEY_F1 {
            return Ok(Some(KeyCode::Help));
        }
        Ok(map_key(ev.code, self.shift))
    }

    /// Block until the next *real* press (value=1) of a key we care
    /// about. Key-up events and auto-repeat (value=2) currently fall
    /// through to the next read so apps that just want "did the user
    /// press something" stay simple. Modifier-state tracking matches
    /// [`decode_one`] so a held Shift/Alt/Super never gets stuck.
    pub fn next_event(&mut self) -> io::Result<Event> {
        loop {
            let mut raw = [0u8; 24];
            self.file.read_exact(&mut raw)?;
            let ev = parse_input_event(&raw);

            if ev.type_ != EV_KEY {
                continue;
            }

            // Track modifier state on press AND release so it never gets
            // stuck. Every modifier we know about goes through here.
            match ev.code {
                KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                    self.shift = ev.value != 0;
                    continue;
                }
                KEY_LEFTALT | KEY_RIGHTALT => {
                    self.alt = ev.value != 0;
                    continue;
                }
                KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                    self.ctrl = ev.value != 0;
                    continue;
                }
                KEY_LEFTMETA | KEY_RIGHTMETA => {
                    self.super_ = ev.value != 0;
                    continue;
                }
                _ => {}
            }

            // value: 0 = release, 1 = press, 2 = auto-repeat. We treat
            // press + repeat the same so holding a key delivers events.
            if ev.value == 0 {
                continue;
            }

            if let Some(key) = map_key(ev.code, self.shift) {
                return Ok(Event::Key(key));
            }
        }
    }

    /// Fetch the underlying file descriptor — useful if you want to
    /// `poll(2)` several input devices in one loop. Not used in Tier 2.
    pub fn raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

fn parse_input_event(buf: &[u8; 24]) -> InputEvent {
    // SAFETY: input_event is `#[repr(C)]` with no internal padding that
    // would invalidate this read. Both the kernel and `[u8; 24]` are
    // valid sources for the bytes — we never expose the pointer.
    unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) }
}

// ─── Linux keycode → KeyCode map ─────────────────────────────────────

fn map_key(code: u16, shift: bool) -> Option<KeyCode> {
    Some(match code {
        KEY_ESC => KeyCode::Escape,
        KEY_BACKSPACE => KeyCode::Backspace,
        KEY_TAB => {
            if shift {
                KeyCode::BackTab
            } else {
                KeyCode::Tab
            }
        }
        KEY_ENTER => KeyCode::Enter,
        KEY_SPACE => KeyCode::Space,
        KEY_UP => KeyCode::Up,
        KEY_DOWN => KeyCode::Down,
        KEY_LEFT => KeyCode::Left,
        KEY_RIGHT => KeyCode::Right,
        KEY_HOME => KeyCode::Home,
        KEY_END => KeyCode::End,
        KEY_PAGEUP => KeyCode::PageUp,
        KEY_PAGEDOWN => KeyCode::PageDown,
        other => KeyCode::Char(char_for_code(other, shift)?),
    })
}

/// US-QWERTY mapping for the alphanumeric block. Returns None for keys
/// outside the printable range so `map_key` can fall through to Other.
fn char_for_code(code: u16, shift: bool) -> Option<char> {
    // Row layout per Linux kernel: KEY_1..KEY_0 = 2..11, KEY_Q..P = 16..25,
    // KEY_A..L = 30..38, KEY_Z..M = 44..50.
    let (lower, upper) = match code {
        2 => ('1', '!'),
        3 => ('2', '@'),
        4 => ('3', '#'),
        5 => ('4', '$'),
        6 => ('5', '%'),
        7 => ('6', '^'),
        8 => ('7', '&'),
        9 => ('8', '*'),
        10 => ('9', '('),
        11 => ('0', ')'),
        12 => ('-', '_'),
        13 => ('=', '+'),
        // Top alpha row Q..P
        16 => ('q', 'Q'),
        17 => ('w', 'W'),
        18 => ('e', 'E'),
        19 => ('r', 'R'),
        20 => ('t', 'T'),
        21 => ('y', 'Y'),
        22 => ('u', 'U'),
        23 => ('i', 'I'),
        24 => ('o', 'O'),
        25 => ('p', 'P'),
        26 => ('[', '{'),
        27 => (']', '}'),
        // Home alpha row A..L
        30 => ('a', 'A'),
        31 => ('s', 'S'),
        32 => ('d', 'D'),
        33 => ('f', 'F'),
        34 => ('g', 'G'),
        35 => ('h', 'H'),
        36 => ('j', 'J'),
        37 => ('k', 'K'),
        38 => ('l', 'L'),
        39 => (';', ':'),
        40 => ('\'', '"'),
        41 => ('`', '~'),
        43 => ('\\', '|'),
        // Bottom alpha row Z..M
        44 => ('z', 'Z'),
        45 => ('x', 'X'),
        46 => ('c', 'C'),
        47 => ('v', 'V'),
        48 => ('b', 'B'),
        49 => ('n', 'N'),
        50 => ('m', 'M'),
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        _ => return None,
    };
    Some(if shift { upper } else { lower })
}

// ─── Pointer (mouse) input ───────────────────────────────────────────

/// Which mouse button an event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// One logical pointer event. Motion is **relative** — `dx`/`dy` is how
/// far the mouse moved since the last report, accumulated across the
/// raw records up to a `SYN_REPORT`. The window manager keeps the
/// absolute cursor position itself (it owns the screen bounds to clamp
/// against); the input layer only reports change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Moved { dx: i32, dy: i32 },
    /// Absolute position in **screen pixels**, already mapped from the
    /// touchscreen's raw device range. The window manager sets the
    /// cursor straight to this instead of integrating a delta.
    MovedTo { x: i32, y: i32 },
    Button { button: MouseButton, pressed: bool },
    /// Wheel notch: +1 = up/away, -1 = down/toward the user.
    Wheel(i32),
}

/// Opens a mouse `/dev/input/eventN` and turns its raw evdev stream into
/// [`MouseEvent`]s. Relative deltas are summed and only released as one
/// `Moved` when the kernel closes the packet with `SYN_REPORT`, so a
/// single physical move is one event no matter how the driver chunks it.
pub struct PointerReader {
    file: File,
    accum_dx: i32,
    accum_dy: i32,
    /// `Some` when this device reports absolute coordinates (a
    /// touchscreen). Carries the raw→screen calibration.
    abs: Option<AbsCal>,
    /// Latest raw absolute X/Y seen this packet (absolute mode only).
    last_ax: Option<i32>,
    last_ay: Option<i32>,
}

/// Maps a touchscreen's raw axis range onto the screen. `min/max` come
/// from `EVIOCGABS`; if that ioctl fails we start with a common default
/// and *widen* it whenever we see a value outside it, so calibration
/// self-corrects within the first swipe instead of staying wrong.
#[derive(Debug, Clone, Copy)]
struct AbsCal {
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
    screen_w: i32,
    screen_h: i32,
}

impl AbsCal {
    fn map(&mut self, ax: i32, ay: i32) -> (i32, i32) {
        if ax < self.x_min { self.x_min = ax; }
        if ax > self.x_max { self.x_max = ax; }
        if ay < self.y_min { self.y_min = ay; }
        if ay > self.y_max { self.y_max = ay; }
        let xspan = (self.x_max - self.x_min).max(1);
        let yspan = (self.y_max - self.y_min).max(1);
        let x = (ax - self.x_min) as i64 * (self.screen_w - 1).max(1) as i64 / xspan as i64;
        let y = (ay - self.y_min) as i64 * (self.screen_h - 1).max(1) as i64 / yspan as i64;
        (x as i32, y as i32)
    }
}

impl PointerReader {
    /// Open a **relative** pointer (a real mouse) — unchanged behaviour.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self {
            file,
            accum_dx: 0,
            accum_dy: 0,
            abs: None,
            last_ax: None,
            last_ay: None,
        })
    }

    /// Open an **absolute** pointer (a touchscreen) and calibrate it to a
    /// `screen_w × screen_h` display by querying the kernel's axis range.
    pub fn open_abs(
        path: impl AsRef<Path>,
        screen_w: u32,
        screen_h: u32,
    ) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let fd = file.as_raw_fd();
        // Try ABS_X/ABS_Y first, then the multitouch axes; fall back to a
        // wide default range that self-corrects.
        let read = |f: unsafe fn(i32, *mut InputAbsinfo) -> nix::Result<i32>| -> Option<(i32, i32)> {
            let mut a = InputAbsinfo::default();
            // SAFETY: `fd` is the live device fd; `a` is a valid, aligned
            // `#[repr(C)]` buffer the kernel fills. The ioctl only writes
            // those 24 bytes.
            match unsafe { f(fd, &mut a) } {
                Ok(_) if a.maximum > a.minimum => Some((a.minimum, a.maximum)),
                _ => None,
            }
        };
        let (x_min, x_max) = read(ev_get_abs_x)
            .or_else(|| read(ev_get_abs_mt_x))
            .unwrap_or((0, 4095));
        let (y_min, y_max) = read(ev_get_abs_y)
            .or_else(|| read(ev_get_abs_mt_y))
            .unwrap_or((0, 4095));
        Ok(Self {
            file,
            accum_dx: 0,
            accum_dy: 0,
            abs: Some(AbsCal {
                x_min,
                x_max,
                y_min,
                y_max,
                screen_w: screen_w as i32,
                screen_h: screen_h as i32,
            }),
            last_ax: None,
            last_ay: None,
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Read exactly one evdev record and fold it in. Returns `Some` only
    /// when that record completed a logical event (a button transition,
    /// a wheel notch, or the `SYN_REPORT` that ends a packet).
    pub fn decode_one(&mut self) -> io::Result<Option<MouseEvent>> {
        let mut raw = [0u8; 24];
        self.file.read_exact(&mut raw)?;
        let ev = parse_input_event(&raw);
        Ok(match ev.type_ {
            EV_REL => {
                match ev.code {
                    REL_X => self.accum_dx += ev.value,
                    REL_Y => self.accum_dy += ev.value,
                    REL_WHEEL => return Ok(Some(MouseEvent::Wheel(ev.value))),
                    _ => {}
                }
                None
            }
            EV_ABS => {
                match ev.code {
                    ABS_X | ABS_MT_POSITION_X => self.last_ax = Some(ev.value),
                    ABS_Y | ABS_MT_POSITION_Y => self.last_ay = Some(ev.value),
                    _ => {}
                }
                None
            }
            EV_KEY => {
                let button = match ev.code {
                    BTN_LEFT | BTN_TOUCH => Some(MouseButton::Left),
                    BTN_RIGHT => Some(MouseButton::Right),
                    BTN_MIDDLE => Some(MouseButton::Middle),
                    _ => None,
                };
                // value 2 == autorepeat; buttons don't autorepeat.
                match button {
                    Some(b) if ev.value != 2 => {
                        Some(MouseEvent::Button { button: b, pressed: ev.value == 1 })
                    }
                    _ => None,
                }
            }
            EV_SYN if ev.code == SYN_REPORT => {
                if let Some(cal) = &mut self.abs {
                    match (self.last_ax, self.last_ay) {
                        (Some(ax), Some(ay)) => {
                            let (x, y) = cal.map(ax, ay);
                            Some(MouseEvent::MovedTo { x, y })
                        }
                        _ => None,
                    }
                } else if self.accum_dx != 0 || self.accum_dy != 0 {
                    let (dx, dy) = (self.accum_dx, self.accum_dy);
                    self.accum_dx = 0;
                    self.accum_dy = 0;
                    Some(MouseEvent::Moved { dx, dy })
                } else {
                    None
                }
            }
            _ => None,
        })
    }
}

// ─── InputHub — multiplex keyboard + mouse + a timer tick ────────────

/// What the hub handed back this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubEvent {
    Key(KeyCode),
    Mouse(MouseEvent),
    /// The poll timeout elapsed with no input — the caller's cue to do
    /// periodic work (refresh a panel, blink a cursor) and redraw.
    Tick,
}

/// Watches **every** keyboard and pointer the system has at the same time.
///
/// A window manager can't block reading any one device — anything might
/// move first. The kernel's answer is `poll(2)`: hand it the set of fds
/// you care about and it sleeps until *any* of them is readable (or a
/// timeout fires). That timeout doubles as a heartbeat so the desktop
/// can refresh time-based windows without input.
///
/// Multi-device matters on real hardware: a Surface Go 2 may have the
/// Type Cover keyboard *and* an external USB keyboard plugged in; a
/// ThinkPad has the internal PS/2 keyboard *plus* a Synaptics touchpad
/// *plus* a TrackPoint *plus* whatever USB peripherals are attached.
/// We open and poll all of them so the user never has to think about
/// which device is "the" one.
pub struct InputHub {
    keys: Vec<KeyReader>,
    pointers: Vec<PointerReader>,
    /// Paths already opened — used so re-scans don't re-add the same
    /// device twice (each evdev fd is a one-reader interface).
    opened_paths: std::collections::HashSet<String>,
}

impl InputHub {
    /// Build an empty hub. Callers add devices with `add_keyboard` /
    /// `add_pointer` (one per real device).
    pub fn empty() -> Self {
        Self {
            keys: Vec::new(),
            pointers: Vec::new(),
            opened_paths: std::collections::HashSet::new(),
        }
    }

    /// Single-device constructor — kept for back-compat with `--kbd` /
    /// `--mouse` explicit paths and unit tests.
    pub fn new(keys: Option<KeyReader>, pointer: Option<PointerReader>) -> Self {
        let mut h = Self::empty();
        if let Some(k) = keys {
            h.keys.push(k);
        }
        if let Some(p) = pointer {
            h.pointers.push(p);
        }
        h
    }

    /// Attach an already-opened keyboard. The path string is recorded so
    /// subsequent rescans skip the same device.
    pub fn add_keyboard(&mut self, k: KeyReader, path: impl Into<String>) {
        self.opened_paths.insert(path.into());
        self.keys.push(k);
    }

    /// Attach an already-opened pointer / touchscreen.
    pub fn add_pointer(&mut self, p: PointerReader, path: impl Into<String>) {
        self.opened_paths.insert(path.into());
        self.pointers.push(p);
    }

    /// Has this path already been opened? Lets callers skip a rescan hit.
    pub fn has_path(&self, path: &str) -> bool {
        self.opened_paths.contains(path)
    }

    /// Back-compat single-keyboard setter — replaces any existing set.
    pub fn set_keyboard(&mut self, k: KeyReader) {
        self.keys.clear();
        self.keys.push(k);
    }

    /// Back-compat single-pointer setter.
    pub fn set_pointer(&mut self, p: PointerReader) {
        self.pointers.clear();
        self.pointers.push(p);
    }

    pub fn has_keyboard(&self) -> bool {
        !self.keys.is_empty()
    }

    pub fn has_pointer(&self) -> bool {
        !self.pointers.is_empty()
    }

    /// Block until a key, a mouse event, or `timeout` elapses.
    ///
    /// Records that don't decode to a logical event (key-up, a modifier,
    /// an idle `SYN`) are drained transparently — the caller only ever
    /// sees real events or a `Tick`.
    pub fn poll_event(&mut self, timeout: Duration) -> io::Result<HubEvent> {
        let to: PollTimeout = timeout
            .as_millis()
            .try_into()
            .ok()
            .and_then(|ms: u64| PollTimeout::try_from(ms).ok())
            .unwrap_or(PollTimeout::MAX);

        loop {
            // With no input devices yet (a fresh tablet boot before the
            // Type Cover / USB has enumerated) there is nothing to poll —
            // just sleep out the timeout and tick so the desktop stays
            // live and the caller can keep retrying device attach.
            if self.keys.is_empty() && self.pointers.is_empty() {
                std::thread::sleep(timeout.min(Duration::from_millis(250)));
                return Ok(HubEvent::Tick);
            }

            // PollFd borrows the fds. Slots in `fds` line up with
            // (keys ++ pointers); `kbd_count` tells the boundary.
            let kbd_count = self.keys.len();
            let mut fds = Vec::with_capacity(kbd_count + self.pointers.len());
            for k in &self.keys {
                fds.push(PollFd::new(k.as_fd(), PollFlags::POLLIN));
            }
            for p in &self.pointers {
                fds.push(PollFd::new(p.as_fd(), PollFlags::POLLIN));
            }

            let n = match poll(&mut fds, to) {
                Ok(n) => n,
                Err(nix::errno::Errno::EINTR) => continue, // signal; re-poll
                Err(e) => return Err(io::Error::other(e)),
            };
            if n == 0 {
                return Ok(HubEvent::Tick);
            }

            // Walk the slots looking for the first one with data. The
            // revents check below also clears EV_HUP/EV_ERR slots so a
            // device that unplugged is dropped from the set instead of
            // wedging the poll loop.
            let mut drop_kbd: Option<usize> = None;
            let mut drop_ptr: Option<usize> = None;

            for (i, fd) in fds.iter().enumerate() {
                let Some(r) = fd.revents() else { continue };
                if r.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
                    if i < kbd_count {
                        drop_kbd = Some(i);
                    } else {
                        drop_ptr = Some(i - kbd_count);
                    }
                    continue;
                }
                if !r.intersects(PollFlags::POLLIN) {
                    continue;
                }
                if i < kbd_count {
                    match self.keys[i].decode_one() {
                        Ok(Some(k)) => return Ok(HubEvent::Key(k)),
                        Ok(None) => break, // modifier / release — re-poll
                        Err(_) => {
                            drop_kbd = Some(i);
                            break;
                        }
                    }
                } else {
                    let pi = i - kbd_count;
                    match self.pointers[pi].decode_one() {
                        Ok(Some(m)) => return Ok(HubEvent::Mouse(m)),
                        Ok(None) => break, // accumulated, awaiting SYN_REPORT
                        Err(_) => {
                            drop_ptr = Some(pi);
                            break;
                        }
                    }
                }
            }

            // Borrows from `fds` have ended; safe to mutate the vectors.
            if let Some(i) = drop_kbd {
                if i < self.keys.len() {
                    self.keys.remove(i);
                }
            }
            if let Some(i) = drop_ptr {
                if i < self.pointers.len() {
                    self.pointers.remove(i);
                }
            }
            // Loop back: if we mutated and have nothing to return yet,
            // re-poll the remaining fds.
        }
    }
}

// ─── Device auto-detection ───────────────────────────────────────────
//
// We pick devices by *what they can do*, not by which kernel "handler"
// is bound to them. The naive "find a device whose `H: Handlers=` lists
// `kbd`" is wrong: the ACPI **Power Button** also uses the `kbd` handler
// (it emits KEY_POWER), and on QEMU it enumerates as `event0` — *before*
// the real keyboard — so handler-matching opens the Power Button and no
// keystroke ever arrives. Likewise the mouse's `mouseN` handler only
// exists if the kernel has `CONFIG_INPUT_MOUSEDEV`, which we don't need
// (we read evdev directly) and don't compile.
//
// Instead we read each device's `B: EV=` capability bitmask from
// `/proc/bus/input/devices`. `EV` is a bit per evdev event *type*
// (linux/input-event-codes.h): bit `EV_KEY` = it has keys, bit `EV_REL`
// = it reports relative motion (a mouse), bit `EV_REP` = it does key
// auto-repeat (every real keyboard does; buttons like Power/Sleep do
// not). That last bit is exactly what separates a keyboard from the
// Power Button. This also works unchanged on real USB hardware.

/// `1 << EV_KEY` — device has keys. (`EV_KEY` = 0x01.)
const EVBIT_KEY: u64 = 1 << 0x01;
/// `1 << EV_REL` — device reports relative motion → it's a mouse.
const EVBIT_REL: u64 = 1 << 0x02;
/// `1 << EV_ABS` — device reports absolute position → a touchscreen or
/// tablet. The Surface Go 2's only built-in pointer.
const EVBIT_ABS: u64 = 1 << 0x03;
/// `1 << EV_REP` — device does autorepeat → it's a real keyboard, not a
/// Power/Sleep button (those are `EV=3`: SYN+KEY only).
const EVBIT_REP: u64 = 1 << 0x14;

/// One parsed `/proc/bus/input/devices` block: the `eventN` node name
/// and the `EV=` capability bitmask. Everything else is ignored.
struct InputDev {
    event: String,
    ev: u64,
}

/// Parse `/proc/bus/input/devices` into (eventNode, EV-mask) pairs.
///
/// The file is blank-line-separated blocks; within a block we only need
/// `H: Handlers=… eventN …` (which `/dev/input` node to open) and
/// `B: EV=<hex>` (the capability bitmask). Devices with no `event*`
/// handler (can't be read via evdev) are skipped.
fn parse_input_devices() -> Vec<InputDev> {
    match fs::read_to_string("/proc/bus/input/devices") {
        Ok(table) => parse_input_devices_str(&table),
        Err(_) => Vec::new(),
    }
}

/// The pure parser, split out so it can be unit-tested against a fixed
/// `/proc/bus/input/devices` sample without touching the real filesystem.
fn parse_input_devices_str(table: &str) -> Vec<InputDev> {
    let mut out = Vec::new();
    let mut event: Option<String> = None;
    let mut ev: u64 = 0;
    let flush = |event: &mut Option<String>, ev: &mut u64, out: &mut Vec<InputDev>| {
        if let Some(e) = event.take() {
            out.push(InputDev { event: e, ev: *ev });
        }
        *ev = 0;
    };
    for line in table.lines() {
        if line.is_empty() {
            flush(&mut event, &mut ev, &mut out);
        } else if let Some(h) = line.strip_prefix("H: Handlers=") {
            event = h
                .split_whitespace()
                .find(|t| t.starts_with("event"))
                .map(|t| t.to_string());
        } else if let Some(rest) = line.strip_prefix("B: EV=") {
            // The kernel prints EV as one hex word (no spaces), e.g.
            // `120013`. KEY/REL bitmaps are multi-word but we don't need
            // them — the EV type mask alone classifies the device.
            ev = u64::from_str_radix(rest.trim(), 16).unwrap_or(0);
        }
    }
    flush(&mut event, &mut ev, &mut out); // file may not end with a blank line
    out
}

/// Auto-detect the keyboard's evdev node.
///
/// A real keyboard has `EV_KEY` **and** `EV_REP` set — the autorepeat
/// bit is what tells it apart from the ACPI Power/Sleep buttons (which
/// also carry the `kbd` handler but are `EV=3`). Falls back to any
/// keyed device, then `event0`, so an unusual setup still gets *a* try.
pub fn detect_keyboard() -> Option<String> {
    pick_keyboard(&parse_input_devices()).or_else(|| {
        (0..16)
            .map(|n| format!("/dev/input/event{n}"))
            .find(|p| Path::new(p).exists())
    })
}

/// Detect **every** keyboard on the system. A real laptop may have the
/// internal keyboard *and* a Type Cover *and* an external USB keyboard
/// all live at the same time — the desktop wants every one of them
/// usable without forcing the user to unplug. Returned in detection
/// order; deduplicated against the same eventN node.
pub fn detect_all_keyboards() -> Vec<String> {
    pick_all_keyboards(&parse_input_devices())
}

/// Keyboard selection over an already-parsed device list (pure, tested).
fn pick_keyboard(devs: &[InputDev]) -> Option<String> {
    devs.iter()
        .find(|d| d.ev & EVBIT_KEY != 0 && d.ev & EVBIT_REP != 0)
        .or_else(|| devs.iter().find(|d| d.ev & EVBIT_KEY != 0))
        .map(|d| format!("/dev/input/{}", d.event))
}

/// Pick every real keyboard (EV_KEY + EV_REP). The autorepeat filter is
/// what keeps the ACPI Power/Sleep buttons out of the set.
fn pick_all_keyboards(devs: &[InputDev]) -> Vec<String> {
    devs.iter()
        .filter(|d| d.ev & EVBIT_KEY != 0 && d.ev & EVBIT_REP != 0)
        .map(|d| format!("/dev/input/{}", d.event))
        .collect()
}

/// Auto-detect the mouse's evdev node: the device that reports relative
/// motion (`EV_REL`). Returns `None` if there is no pointer — the
/// desktop then runs keyboard-only.
///
/// Note this is the *relative* pointer our [`PointerReader`] decodes; an
/// absolute device (a `usb-tablet`, `EV_ABS`) is intentionally not
/// matched because we don't decode absolute axes yet.
pub fn detect_mouse() -> Option<String> {
    pick_mouse(&parse_input_devices())
}

/// Every relative pointer (real mouse, PS/2 touchpad, TrackPoint).
pub fn detect_all_mice() -> Vec<String> {
    pick_all_mice(&parse_input_devices())
}

/// Mouse selection over an already-parsed device list (pure, tested).
fn pick_mouse(devs: &[InputDev]) -> Option<String> {
    devs.iter()
        .find(|d| d.ev & EVBIT_REL != 0)
        .map(|d| format!("/dev/input/{}", d.event))
}

fn pick_all_mice(devs: &[InputDev]) -> Vec<String> {
    devs.iter()
        .filter(|d| d.ev & EVBIT_REL != 0)
        .map(|d| format!("/dev/input/{}", d.event))
        .collect()
}

/// Auto-detect a **touchscreen / absolute pointer** (`EV_ABS`).
///
/// This is what makes a keyboardless, mouseless tablet — a Surface Go 2 —
/// actually usable: the panel becomes the cursor. We deliberately ignore
/// devices that *also* report `EV_REL` (those are real mice / touchpads,
/// handled by [`detect_mouse`] with its delta decoder).
pub fn detect_touch() -> Option<String> {
    pick_touch(&parse_input_devices())
}

/// Every touchscreen / absolute pointer.
pub fn detect_all_touch() -> Vec<String> {
    pick_all_touch(&parse_input_devices())
}

/// Touchscreen selection over an already-parsed device list (pure).
fn pick_touch(devs: &[InputDev]) -> Option<String> {
    devs.iter()
        .find(|d| d.ev & EVBIT_ABS != 0 && d.ev & EVBIT_REL == 0)
        .map(|d| format!("/dev/input/{}", d.event))
}

fn pick_all_touch(devs: &[InputDev]) -> Vec<String> {
    devs.iter()
        .filter(|d| d.ev & EVBIT_ABS != 0 && d.ev & EVBIT_REL == 0)
        .map(|d| format!("/dev/input/{}", d.event))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim `/proc/bus/input/devices` from a DrDrOS QEMU boot, with a
    /// USB mouse block appended. The trap this locks down: the ACPI
    /// **Power Button** (`event0`) carries the `kbd` handler and sorts
    /// *before* the real keyboard, so the old handler-matching opened
    /// `event0` and no keystroke ever arrived.
    const SAMPLE: &str = "\
I: Bus=0019 Vendor=0000 Product=0001 Version=0000
N: Name=\"Power Button\"
P: Phys=LNXPWRBN/button/input0
S: Sysfs=/devices/LNXSYSTM:00/LNXPWRBN:00/input/input0
U: Uniq=
H: Handlers=kbd event0
B: PROP=0
B: EV=3
B: KEY=8000 10000000000000 0

I: Bus=0011 Vendor=0001 Product=0001 Version=ab41
N: Name=\"AT Translated Set 2 keyboard\"
P: Phys=isa0060/serio0/input0
S: Sysfs=/devices/platform/i8042/serio0/input/input1
U: Uniq=
H: Handlers=sysrq kbd leds event1
B: PROP=0
B: EV=120013
B: KEY=402000007 ff803078f800d001 feffffdfffcfffff fffffffffffffffe
B: MSC=10
B: LED=7

I: Bus=0003 Vendor=0627 Product=0001 Version=0000
N: Name=\"QEMU QEMU USB Mouse\"
P: Phys=usb-0000:00:01.2-1/input0
S: Sysfs=/devices/pci0000:00/0000:00:01.2/usb1/1-1/1-1:1.0/input/input2
U: Uniq=
H: Handlers=event2
B: PROP=0
B: EV=17
B: KEY=70000 0 0 0 0
B: REL=103
";

    #[test]
    fn parses_event_node_and_ev_mask() {
        let d = parse_input_devices_str(SAMPLE);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].event, "event0");
        assert_eq!(d[0].ev, 0x3); // Power Button: SYN+KEY only
        assert_eq!(d[1].event, "event1");
        assert_eq!(d[1].ev, 0x120013); // keyboard: …+REP+LED
        assert_eq!(d[2].event, "event2");
        assert_eq!(d[2].ev, 0x17); // mouse: …+REL
    }

    #[test]
    fn keyboard_pick_skips_the_power_button() {
        let d = parse_input_devices_str(SAMPLE);
        // The whole point: NOT event0 (Power Button, no EV_REP).
        assert_eq!(pick_keyboard(&d).as_deref(), Some("/dev/input/event1"));
    }

    #[test]
    fn mouse_pick_finds_the_relative_pointer() {
        let d = parse_input_devices_str(SAMPLE);
        assert_eq!(pick_mouse(&d).as_deref(), Some("/dev/input/event2"));
    }

    #[test]
    fn no_pointer_means_no_mouse() {
        // Power Button + keyboard only (the pre-usb-mouse QEMU state):
        // detection must return None, not a false positive.
        let kbd_only = SAMPLE.split("\nI: Bus=0003").next().unwrap();
        let d = parse_input_devices_str(kbd_only);
        assert_eq!(d.len(), 2);
        assert_eq!(pick_mouse(&d), None);
        assert_eq!(pick_keyboard(&d).as_deref(), Some("/dev/input/event1"));
    }

    #[test]
    fn an_absolute_tablet_is_not_picked_as_a_mouse() {
        // usb-tablet reports EV_ABS (bit 3 → 0x8), not EV_REL. We don't
        // decode absolute axes, so it must NOT be chosen (keyboard-only
        // is correct until PointerReader grows EV_ABS support).
        let tablet = "\
N: Name=\"QEMU USB Tablet\"
H: Handlers=event3
B: EV=b
B: ABS=3
";
        let d = parse_input_devices_str(tablet);
        assert_eq!(d[0].ev, 0xb); // SYN+KEY+ABS, no REL
        assert_eq!(pick_mouse(&d), None);
        // …but it IS a touchscreen — the Surface Go 2's only pointer.
        assert_eq!(pick_touch(&d).as_deref(), Some("/dev/input/event3"));
    }

    #[test]
    fn a_real_mouse_is_not_mistaken_for_a_touchscreen() {
        // The QEMU USB mouse (EV=17 = SYN+KEY+REL) must stay a mouse;
        // pick_touch deliberately skips anything that also reports REL.
        let d = parse_input_devices_str(SAMPLE);
        assert_eq!(pick_touch(&d), None);
        assert_eq!(pick_mouse(&d).as_deref(), Some("/dev/input/event2"));
    }

    #[test]
    fn absolute_calibration_maps_corners_to_screen() {
        let mut cal = AbsCal {
            x_min: 0,
            x_max: 4095,
            y_min: 0,
            y_max: 4095,
            screen_w: 1920,
            screen_h: 1280,
        };
        assert_eq!(cal.map(0, 0), (0, 0));
        assert_eq!(cal.map(4095, 4095), (1919, 1279));
        // A value past the learned range widens it and still maps in.
        let (x, _) = cal.map(8191, 0);
        assert!(x <= 1919);
    }
}
