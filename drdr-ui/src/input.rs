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

// Mouse / pointer event types and codes (linux/input-event-codes.h).
// A mouse reports motion as *relative* deltas: an `EV_REL` record with
// code `REL_X`/`REL_Y` and `value` = how far it moved since the last
// record. Buttons are `EV_KEY` like keyboard keys, just with codes in
// the `BTN_*` range. Every batch of records the kernel emits for one
// physical movement ends with an `EV_SYN`/`SYN_REPORT` marker — that's
// our cue to flush the accumulated delta as one logical move.
const EV_SYN: u16 = 0x00;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

// ─── KeyReader ───────────────────────────────────────────────────────

/// Opens an evdev device and yields [`Event`]s. Holds the Shift state
/// across calls so the right `Char` cases come out for letters.
pub struct KeyReader {
    file: File,
    shift: bool,
    alt: bool,
}

impl KeyReader {
    /// Open `/dev/input/eventN` (or any path the caller provides).
    /// Requires read permission on the device — typically that means
    /// running as root or being in the `input` group.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self { file, shift: false, alt: false })
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
        match ev.code {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                self.shift = ev.value != 0;
                return Ok(None);
            }
            KEY_LEFTALT | KEY_RIGHTALT => {
                self.alt = ev.value != 0;
                return Ok(None);
            }
            _ => {}
        }
        if ev.value == 0 {
            return Ok(None); // release
        }
        // Alt+Tab is a chord the WM wants as one logical key.
        if ev.code == KEY_TAB && self.alt {
            return Ok(Some(KeyCode::AltTab));
        }
        Ok(map_key(ev.code, self.shift))
    }

    /// Block until the next *real* press (value=1) of a key we care
    /// about. Key-up events and auto-repeat (value=2) currently fall
    /// through to the next read so apps that just want "did the user
    /// press something" stay simple.
    pub fn next_event(&mut self) -> io::Result<Event> {
        loop {
            let mut raw = [0u8; 24];
            self.file.read_exact(&mut raw)?;
            let ev = parse_input_event(&raw);

            if ev.type_ != EV_KEY {
                continue;
            }

            // Track modifier state on press AND release so it never gets stuck.
            match ev.code {
                KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                    self.shift = ev.value != 0;
                    continue;
                }
                _ => {}
            }

            // value: 0 = release, 1 = press, 2 = auto-repeat.
            // For Tier 2 we treat press + repeat the same so holding a key
            // delivers events — this is what users expect of arrow keys
            // in a menu.
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
}

impl PointerReader {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self { file, accum_dx: 0, accum_dy: 0 })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Read exactly one evdev record and fold it in. Returns `Some` only
    /// when that record completed a logical event (a button transition,
    /// a wheel notch, or the `SYN_REPORT` that ends a motion packet).
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
            EV_KEY => {
                let button = match ev.code {
                    BTN_LEFT => Some(MouseButton::Left),
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
                if self.accum_dx != 0 || self.accum_dy != 0 {
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

/// Watches the keyboard and the mouse **at the same time**.
///
/// A window manager can't block reading the keyboard — the mouse might
/// move first, or vice-versa. The kernel's answer is `poll(2)`: hand it
/// the set of fds you care about and it sleeps until *any* of them is
/// readable (or a timeout fires). That timeout doubles as a heartbeat
/// so the desktop can refresh time-based windows without input. This is
/// the same idea as DrDrNet's epoll reactor, one tier smaller: a fixed
/// couple of fds instead of thousands.
pub struct InputHub {
    keys: KeyReader,
    pointer: Option<PointerReader>,
}

impl InputHub {
    pub fn new(keys: KeyReader, pointer: Option<PointerReader>) -> Self {
        Self { keys, pointer }
    }

    /// Block until a key, a mouse event, or `timeout` elapses.
    ///
    /// Keyboard is checked before mouse so typing stays responsive under
    /// a moving mouse. Records that don't decode to a logical event
    /// (key-up, a modifier, an idle `SYN`) are drained transparently —
    /// the caller only ever sees real events or a `Tick`.
    pub fn poll_event(&mut self, timeout: Duration) -> io::Result<HubEvent> {
        let to: PollTimeout = timeout
            .as_millis()
            .try_into()
            .ok()
            .and_then(|ms: u64| PollTimeout::try_from(ms).ok())
            .unwrap_or(PollTimeout::MAX);

        loop {
            // PollFd borrows the fds, so the set is rebuilt each pass.
            let mut fds = Vec::with_capacity(2);
            fds.push(PollFd::new(self.keys.as_fd(), PollFlags::POLLIN));
            if let Some(p) = &self.pointer {
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

            let kbd_ready = fds[0]
                .revents()
                .is_some_and(|r| r.intersects(PollFlags::POLLIN));
            if kbd_ready {
                if let Some(k) = self.keys.decode_one()? {
                    return Ok(HubEvent::Key(k));
                }
                continue; // modifier / release — keep polling
            }

            if self.pointer.is_some() {
                let ptr_ready = fds[1]
                    .revents()
                    .is_some_and(|r| r.intersects(PollFlags::POLLIN));
                if ptr_ready {
                    if let Some(m) = self.pointer.as_mut().unwrap().decode_one()? {
                        return Ok(HubEvent::Mouse(m));
                    }
                    continue;
                }
            }

            // POLLERR/POLLHUP or a spurious wake — treat as a tick so the
            // caller stays alive and just redraws.
            return Ok(HubEvent::Tick);
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

/// Keyboard selection over an already-parsed device list (pure, tested).
fn pick_keyboard(devs: &[InputDev]) -> Option<String> {
    devs.iter()
        .find(|d| d.ev & EVBIT_KEY != 0 && d.ev & EVBIT_REP != 0)
        .or_else(|| devs.iter().find(|d| d.ev & EVBIT_KEY != 0))
        .map(|d| format!("/dev/input/{}", d.event))
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

/// Mouse selection over an already-parsed device list (pure, tested).
fn pick_mouse(devs: &[InputDev]) -> Option<String> {
    devs.iter()
        .find(|d| d.ev & EVBIT_REL != 0)
        .map(|d| format!("/dev/input/{}", d.event))
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
    }
}
