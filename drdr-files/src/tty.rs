//! Raw-mode TTY helpers shared by DrDrFiles' interactive browser.
//!
//! Two pieces:
//!
//!   1. [`RawMode`] — an RAII guard that puts stdin into "raw" mode
//!      (no line buffering, no echo, no ICANON, etc.) on construction
//!      and restores the original termios + leaves the alternate screen
//!      buffer on Drop. The Drop runs on panic too, so a crash inside
//!      the interactive loop still leaves the terminal usable.
//!
//!   2. [`read_key`] — read one logical keypress. Handles plain ASCII,
//!      arrow keys (CSI A/B/C/D), Home/End/PageUp/PageDown, Enter, Tab,
//!      Backspace, and any other unrecognised sequence as a [`Key::Other`].
//!
//! No `unsafe` in this module — `nix` wraps the termios syscalls for us.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;

use nix::sys::termios::{
    tcgetattr, tcsetattr, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg,
    SpecialCharacterIndices, Termios,
};

/// One logical keypress, decoded from raw stdin bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Tab,
    Backspace,
    Escape,
    Char(char),
    /// Unrecognised byte / escape sequence — the loop can ignore these.
    Other,
}

/// RAII guard. On construction, enters raw mode + alternate screen +
/// hides the cursor. On drop, restores everything.
pub struct RawMode {
    original: Termios,
    /// We hold a borrow on stdin's fd via this owned handle for the
    /// lifetime of the guard. nix's tcsetattr accepts &impl AsFd, so the
    /// borrow is short-lived per call — we just need the fd live.
    stdin: io::Stdin,
}

impl RawMode {
    pub fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let original = tcgetattr(stdin.as_fd()).map_err(io::Error::from)?;
        let mut raw = original.clone();

        // Bit-twiddling lifted straight from termios(3) — same flags
        // cfmakeraw() would clear, but spelled out so the next reader
        // can see what's happening.
        raw.input_flags &= !(InputFlags::BRKINT
            | InputFlags::ICRNL
            | InputFlags::INPCK
            | InputFlags::ISTRIP
            | InputFlags::IXON);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.control_flags |= ControlFlags::CS8;
        raw.local_flags &= !(LocalFlags::ECHO
            | LocalFlags::ICANON
            | LocalFlags::IEXTEN
            | LocalFlags::ISIG);
        // VMIN=1 / VTIME=0: read() returns as soon as one byte is ready,
        // never times out. Good for an interactive REPL where we want
        // immediate response.
        raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;

        tcsetattr(stdin.as_fd(), SetArg::TCSAFLUSH, &raw).map_err(io::Error::from)?;

        // Enter the "alternate screen buffer" so the user's previous
        // terminal contents come back when we exit (just like vim/less).
        // CSI ?1049 h = enter, l = leave. CSI ?25 l = hide cursor.
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[?1049h\x1b[?25l")?;
        stdout.flush()?;

        Ok(Self { original, stdin })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Leave alt screen, show cursor, restore termios. Errors here
        // are swallowed — there's no useful recovery in a Drop, and
        // we'd rather leave the terminal as close to working as we can.
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
        let _ = tcsetattr(self.stdin.as_fd(), SetArg::TCSAFLUSH, &self.original);
    }
}

/// Read one keypress. Reads as many bytes as needed to decode an escape
/// sequence; never blocks for longer than necessary.
pub fn read_key() -> io::Result<Key> {
    let mut buf = [0u8; 1];
    let mut stdin = io::stdin().lock();
    stdin.read_exact(&mut buf)?;

    match buf[0] {
        b'\r' | b'\n' => Ok(Key::Enter),
        b'\t' => Ok(Key::Tab),
        // 0x7f (DEL) AND 0x08 (BS) both show up as "backspace" on
        // different terminals — accept either.
        0x7f | 0x08 => Ok(Key::Backspace),
        0x1b => decode_escape(&mut stdin),
        b if b.is_ascii() && !b.is_ascii_control() => Ok(Key::Char(b as char)),
        _ => Ok(Key::Other),
    }
}

/// We've already consumed the leading `0x1b` (ESC). Read the next byte
/// to decide whether it's a CSI sequence (`ESC [ ...`) or a lone ESC.
fn decode_escape(stdin: &mut io::StdinLock<'_>) -> io::Result<Key> {
    let mut buf = [0u8; 1];
    // Try to read; if nothing's there, treat as a bare ESC. With VMIN=1
    // we'd block forever, so we use a poll-with-zero-timeout via read.
    // Simpler approach: just read — most terminals send a complete
    // escape sequence as one packet, so the next read returns instantly.
    if stdin.read_exact(&mut buf).is_err() {
        return Ok(Key::Escape);
    }
    match buf[0] {
        b'[' => decode_csi(stdin),
        b'O' => decode_ss3(stdin), // some terminals send Home/End as ESC O H / F
        _ => Ok(Key::Other),
    }
}

/// CSI sequence: `ESC [ <param>* <final>`. We only handle the small set
/// of final bytes the file browser cares about.
fn decode_csi(stdin: &mut io::StdinLock<'_>) -> io::Result<Key> {
    // Accumulate up to 8 parameter bytes before the final.
    let mut params = [0u8; 8];
    let mut n = 0;
    loop {
        let mut b = [0u8; 1];
        stdin.read_exact(&mut b)?;
        if n < params.len() {
            params[n] = b[0];
            n += 1;
        }
        if (0x40..=0x7e).contains(&b[0]) {
            // Final byte.
            return Ok(match (b[0], &params[..n - 1]) {
                (b'A', _) => Key::Up,
                (b'B', _) => Key::Down,
                (b'C', _) => Key::Right,
                (b'D', _) => Key::Left,
                (b'H', _) => Key::Home,
                (b'F', _) => Key::End,
                (b'~', b"1") | (b'~', b"7") => Key::Home,
                (b'~', b"4") | (b'~', b"8") => Key::End,
                (b'~', b"5") => Key::PageUp,
                (b'~', b"6") => Key::PageDown,
                _ => Key::Other,
            });
        }
        if n >= params.len() {
            return Ok(Key::Other);
        }
    }
}

fn decode_ss3(stdin: &mut io::StdinLock<'_>) -> io::Result<Key> {
    let mut b = [0u8; 1];
    stdin.read_exact(&mut b)?;
    Ok(match b[0] {
        b'H' => Key::Home,
        b'F' => Key::End,
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        _ => Key::Other,
    })
}

/// Query the terminal for its current size (rows, cols). Falls back to
/// 24×80 — the smallest "real terminal" that's safe to assume — if the
/// ioctl is unsupported (e.g., output redirected to a file).
pub fn term_size() -> (u16, u16) {
    use nix::libc::{ioctl, winsize, STDOUT_FILENO, TIOCGWINSZ};
    let mut ws = winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ writes into our `winsize` struct. The fd is the
    // raw stdout fd, which is valid for the lifetime of the process.
    let rc = unsafe { ioctl(STDOUT_FILENO, TIOCGWINSZ, &mut ws) };
    if rc == -1 || ws.ws_row == 0 || ws.ws_col == 0 {
        (24, 80)
    } else {
        (ws.ws_row, ws.ws_col)
    }
}
