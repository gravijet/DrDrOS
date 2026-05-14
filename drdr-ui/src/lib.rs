//! drdr-ui — DrDrOS GUI framework.
//!
//! Draws DrDrOS's windows, buttons, input fields, and focus model directly
//! onto the Linux framebuffer (/dev/fb0). No X11. No Wayland. No desktop.
//!
//! Phase 1 ships the low-level [`fb`] primitives — open a framebuffer,
//! query its geometry, and paint pixels / rectangles. Phase 3 adds the
//! widget tree, input focus, and [`DrDrTheme`] on top of those primitives.

pub mod fb;

pub use fb::{Framebuffer, Pixel};
