//! drdr-ui — DrDrOS GUI framework.
//!
//! Draws DrDrOS's windows, buttons, input fields, and focus model directly
//! onto the Linux framebuffer (/dev/fb0). No X11. No Wayland. No desktop.
//!
//! Planned (Phase 1 → 3): framebuffer primitives (pixels, rects, blits),
//! a widget tree, an input-focus system, and DrDrTheme (dark, minimal).

// Phase 1 placeholder — keeps the crate buildable.
pub const VERSION: &str = "0.1.0";
