//! drdr-tty — shared termios raw-mode + key decoder for DrDrOS apps.
//!
//! Two pieces:
//!
//!   1. [`RawMode`] — RAII guard. Construction puts stdin into "raw"
//!      mode (no line buffering, no echo, no ICANON, no signals from
//!      `^C` / `^Z`) and enters the alternate screen buffer with cursor
//!      hidden. Drop restores everything — including on panic — so a
//!      crash inside an interactive app never leaves a wrecked TTY.
//!
//!   2. [`read_key`] — read one logical keypress, decoding CSI escape
//!      sequences for arrows / Home / End / PageUp / PageDown. Anything
//!      we don't recognise comes back as [`Key::Other`] so the loop can
//!      ignore it gracefully.
//!
//! Plus [`term_size`] which queries the kernel for the TTY's row/col
//! dimensions via TIOCGWINSZ.
//!
//! All `unsafe` is the single `ioctl` call inside `term_size` (commented
//! with its SAFETY invariant); everything else goes through `nix`.

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
    /// Unrecognised byte / escape sequence — callers can ignore.
    Other,
}

/// RAII guard. On construction, enters raw mode + alternate screen +
/// hides the cursor. On drop, restores everything.
pub struct RawMode {
    original: Termios,
    stdin: io::Stdin,
}

impl RawMode {
    pub fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let original = tcgetattr(stdin.as_fd()).map_err(io::Error::from)?;
        let mut raw = original.clone();

        // Same flags cfmakeraw() would clear, spelled out so the next
        // reader can see what's happening.
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
        raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;

        tcsetattr(stdin.as_fd(), SetArg::TCSAFLUSH, &raw).map_err(io::Error::from)?;

        // CSI ?1049 h = enter alt screen; CSI ?25 l = hide cursor.
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[?1049h\x1b[?25l")?;
        stdout.flush()?;

        Ok(Self { original, stdin })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
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
        0x7f | 0x08 => Ok(Key::Backspace),
        0x1b => decode_escape(&mut stdin),
        b if b.is_ascii() && !b.is_ascii_control() => Ok(Key::Char(b as char)),
        _ => Ok(Key::Other),
    }
}

fn decode_escape(stdin: &mut io::StdinLock<'_>) -> io::Result<Key> {
    let mut buf = [0u8; 1];
    if stdin.read_exact(&mut buf).is_err() {
        return Ok(Key::Escape);
    }
    match buf[0] {
        b'[' => decode_csi(stdin),
        b'O' => decode_ss3(stdin),
        _ => Ok(Key::Other),
    }
}

fn decode_csi(stdin: &mut io::StdinLock<'_>) -> io::Result<Key> {
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
/// 24×80 when the ioctl fails (output redirected to a file, etc.).
pub fn term_size() -> (u16, u16) {
    use nix::libc::{ioctl, winsize, STDOUT_FILENO, TIOCGWINSZ};
    let mut ws = winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ writes into our `winsize` struct. STDOUT_FILENO
    // is a valid fd for the life of the process.
    let rc = unsafe { ioctl(STDOUT_FILENO, TIOCGWINSZ, &mut ws) };
    if rc == -1 || ws.ws_row == 0 || ws.ws_col == 0 {
        (24, 80)
    } else {
        (ws.ws_row, ws.ws_col)
    }
}
