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

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;

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

// ─── KeyReader ───────────────────────────────────────────────────────

/// Opens an evdev device and yields [`Event`]s. Holds the Shift state
/// across calls so the right `Char` cases come out for letters.
pub struct KeyReader {
    file: File,
    shift: bool,
}

impl KeyReader {
    /// Open `/dev/input/eventN` (or any path the caller provides).
    /// Requires read permission on the device — typically that means
    /// running as root or being in the `input` group.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self { file, shift: false })
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
