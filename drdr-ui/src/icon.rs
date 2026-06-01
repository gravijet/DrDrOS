//! drdr-ui/icon — real pictographic app icons, hand-drawn from
//! framebuffer primitives (rounded rects, discs, strokes) rather than a
//! single scaled font letter.
//!
//! The desktop used to render each app's icon as one big ASCII glyph
//! ('F' for Files, 'T' for the editor). That reads as a debug
//! placeholder, not a real OS. This module draws actual little pictures
//! — a folder, a sheet of paper with a folded corner, a calculator with
//! a keypad — in the same from-scratch spirit as the bitmap font: no
//! image files, no SVG parser, just shapes composed at draw time.
//!
//! Every icon is drawn inside a square box `(x, y, size, size)` in two
//! tones derived from one `ink` colour, so a single call restyles the
//! whole icon for the tile it sits on (white ink on a coloured tile,
//! dark ink on a light one). The shapes are alpha-blended, so they only
//! render correctly on the canonical heap back buffer — exactly where
//! the window manager paints them.

use drdr_fb::{Framebuffer, Pixel};

/// Which picture to draw. One variant per app concept; [`IconKind::Generic`]
/// is the fallback for anything without a bespoke drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    Folder,
    Document,
    Note,
    Tasks,
    Terminal,
    Calculator,
    Clock,
    Monitor,
    Info,
    Chat,
    Paint,
    Snake,
    Dice2048,
    Mine,
    Disk,
    Settings,
    Browser,
    Image,
    Network,
    Power,
    Music,
    /// WhatsApp-style messenger — a speech bubble with a phone handset.
    Messages,
    /// Discord-style community chat — the rounded "face" mark.
    Discord,
    /// E-mail client — an envelope.
    Mail,
    /// Month-view calendar — a bound page with a day grid.
    Calendar,
    /// Address book — a contact card with an avatar.
    Contacts,
    /// Photo gallery — two stacked picture frames.
    Gallery,
    Generic,
}

impl IconKind {
    /// Map a short name (used by the file manager and app catalogue) to a
    /// kind, so callers can stay stringly-typed where that is convenient.
    pub fn from_name(name: &str) -> IconKind {
        match name {
            "folder" | "files" => IconKind::Folder,
            "document" | "text" | "editor" => IconKind::Document,
            "note" | "notes" => IconKind::Note,
            "tasks" => IconKind::Tasks,
            "terminal" | "console" | "shell" => IconKind::Terminal,
            "calculator" | "calc" => IconKind::Calculator,
            "clock" => IconKind::Clock,
            "monitor" => IconKind::Monitor,
            "info" => IconKind::Info,
            "chat" => IconKind::Chat,
            "paint" => IconKind::Paint,
            "snake" => IconKind::Snake,
            "2048" => IconKind::Dice2048,
            "mine" | "mines" => IconKind::Mine,
            "disk" | "disks" => IconKind::Disk,
            "settings" | "gear" => IconKind::Settings,
            "browser" | "web" | "html" => IconKind::Browser,
            "image" | "png" | "picture" => IconKind::Image,
            "network" | "wifi" | "net" => IconKind::Network,
            "power" => IconKind::Power,
            "music" | "audio" => IconKind::Music,
            "messages" | "whatsapp" | "messenger" => IconKind::Messages,
            "discord" => IconKind::Discord,
            "mail" | "email" | "e-mail" => IconKind::Mail,
            "calendar" | "events" => IconKind::Calendar,
            "contacts" | "people" | "addressbook" => IconKind::Contacts,
            "gallery" | "photos" | "pictures" => IconKind::Gallery,
            _ => IconKind::Generic,
        }
    }
}

/// A tiny drawing helper bound to one icon box: normalised 0..1 coords map
/// into the box. Three tones give crisp, always-legible icons: `main` is
/// the solid ink silhouette, `soft` a tile-tinted ink for secondary
/// bodies, and `carve` is the *tile colour itself* — used for fine
/// details (clock hands, globe lines, paper text) so they read as
/// knockouts in the silhouette and never wash out.
struct Pen<'a> {
    fb: &'a mut Framebuffer,
    ox: i32,
    oy: i32,
    s: f32,
    main: Pixel,
    soft: Pixel,
    carve: Pixel,
}

impl<'a> Pen<'a> {
    fn px(&self, f: f32) -> i32 {
        self.ox + (f * self.s).round() as i32
    }
    fn py(&self, f: f32) -> i32 {
        self.oy + (f * self.s).round() as i32
    }
    fn len(&self, f: f32) -> u32 {
        (f * self.s).round().max(0.0) as u32
    }
    /// Filled rectangle in normalised coords.
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Pixel) {
        let px = self.px(x).max(0) as u32;
        let py = self.py(y).max(0) as u32;
        self.fb.fill_rect(px, py, self.len(w), self.len(h), c);
    }
    /// Filled rounded rectangle (radius in normalised units).
    fn round(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Pixel) {
        let px = self.px(x).max(0) as u32;
        let py = self.py(y).max(0) as u32;
        self.fb.fill_round_rect(px, py, self.len(w), self.len(h), self.len(r), c);
    }
    /// Filled disc centred at normalised `(cx, cy)`, radius `r`.
    fn disc(&mut self, cx: f32, cy: f32, r: f32, c: Pixel) {
        self.fb.fill_circle(self.px(cx), self.py(cy), self.len(r) as i32, c);
    }
    /// Stroke from `(x0,y0)` to `(x1,y1)`, width in normalised units.
    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, w: f32, c: Pixel) {
        self.fb.draw_line(
            self.px(x0), self.py(y0), self.px(x1), self.py(y1),
            (w * self.s).max(1.0), c,
        );
    }
}

/// Draw `kind` inside the square box at `(x, y)` of side `size` pixels.
/// `ink` is the silhouette colour; `bg` is the tile colour behind the
/// icon, used to carve crisp internal detail. Composites with alpha, so
/// call it on the back buffer.
pub fn draw_icon(fb: &mut Framebuffer, x: u32, y: u32, size: u32, kind: IconKind, ink: Pixel, bg: Pixel) {
    let mut p = Pen {
        ox: x as i32,
        oy: y as i32,
        s: size as f32,
        main: ink,
        soft: ink.lerp(bg, 95),
        carve: bg,
        fb,
    };
    match kind {
        IconKind::Folder => {
            // Tab on the back, then the front body; a carve seam between.
            p.round(0.12, 0.28, 0.40, 0.18, 0.05, p.soft);
            p.round(0.12, 0.36, 0.76, 0.44, 0.06, p.main);
            p.rect(0.12, 0.44, 0.76, 0.02, p.carve); // seam
        }
        IconKind::Document => {
            // Sheet of paper; folded top-right corner + carved text lines.
            p.round(0.22, 0.12, 0.56, 0.76, 0.04, p.main);
            p.rect(0.62, 0.12, 0.16, 0.16, p.carve); // fold notch
            p.line(0.62, 0.28, 0.78, 0.28, 0.03, p.carve);
            p.line(0.62, 0.12, 0.62, 0.28, 0.03, p.carve);
            for i in 0..4 {
                let yy = 0.40 + i as f32 * 0.11;
                p.rect(0.30, yy, 0.40, 0.04, p.carve); // text lines (cut out)
            }
        }
        IconKind::Note => {
            p.round(0.18, 0.16, 0.64, 0.68, 0.08, p.main);
            for i in 0..3 {
                let yy = 0.32 + i as f32 * 0.14;
                p.rect(0.30, yy, 0.40, 0.05, p.carve);
            }
            p.disc(0.50, 0.13, 0.06, p.soft); // pin
            p.disc(0.50, 0.13, 0.025, p.carve);
        }
        IconKind::Tasks => {
            p.round(0.18, 0.14, 0.64, 0.72, 0.07, p.main);
            for i in 0..3 {
                let yy = 0.28 + i as f32 * 0.18;
                // carved checkbox + ticked check + a carved label line
                p.round(0.26, yy, 0.13, 0.13, 0.03, p.carve);
                p.line(0.28, yy + 0.07, 0.31, yy + 0.10, 0.045, p.main);
                p.line(0.31, yy + 0.10, 0.37, yy + 0.02, 0.045, p.main);
                p.rect(0.46, yy + 0.04, 0.28, 0.05, p.carve);
            }
        }
        IconKind::Terminal => {
            p.round(0.12, 0.18, 0.76, 0.64, 0.07, p.main);
            p.rect(0.12, 0.30, 0.76, 0.02, p.carve); // title-bar seam
            // prompt chevron + cursor underscore, carved
            p.line(0.24, 0.44, 0.34, 0.52, 0.06, p.carve);
            p.line(0.34, 0.52, 0.24, 0.60, 0.06, p.carve);
            p.rect(0.40, 0.58, 0.22, 0.05, p.carve);
        }
        IconKind::Calculator => {
            p.round(0.20, 0.12, 0.60, 0.76, 0.07, p.main);
            p.round(0.27, 0.18, 0.46, 0.16, 0.02, p.carve); // screen (cut)
            for r in 0..3 {
                for c in 0..3 {
                    let bx = 0.28 + c as f32 * 0.155;
                    let by = 0.40 + r as f32 * 0.155;
                    p.round(bx, by, 0.11, 0.11, 0.02, p.carve); // keys (cut)
                }
            }
        }
        IconKind::Clock => {
            p.disc(0.5, 0.5, 0.42, p.main);
            p.disc(0.5, 0.5, 0.35, p.soft);
            p.line(0.5, 0.5, 0.5, 0.26, 0.05, p.carve); // hour hand
            p.line(0.5, 0.5, 0.69, 0.5, 0.04, p.carve); // minute hand
            p.disc(0.5, 0.5, 0.035, p.carve);
        }
        IconKind::Monitor => {
            p.round(0.12, 0.16, 0.76, 0.50, 0.05, p.main);
            p.round(0.18, 0.22, 0.64, 0.38, 0.02, p.carve); // screen (cut)
            // bar chart carved... no, draw bars in main over the cut screen
            p.rect(0.26, 0.44, 0.06, 0.10, p.main);
            p.rect(0.36, 0.38, 0.06, 0.16, p.main);
            p.rect(0.46, 0.32, 0.06, 0.22, p.main);
            p.rect(0.56, 0.42, 0.06, 0.12, p.main);
            p.rect(0.44, 0.66, 0.12, 0.08, p.main); // stand neck
            p.rect(0.32, 0.78, 0.36, 0.05, p.main); // base
        }
        IconKind::Info => {
            p.disc(0.5, 0.5, 0.42, p.main);
            p.disc(0.5, 0.30, 0.06, p.carve); // dot of the 'i'
            p.rect(0.45, 0.42, 0.10, 0.28, p.carve); // stem
        }
        IconKind::Chat => {
            p.round(0.12, 0.18, 0.76, 0.48, 0.10, p.main);
            p.line(0.30, 0.62, 0.30, 0.84, 0.10, p.main); // tail
            p.line(0.30, 0.84, 0.48, 0.62, 0.10, p.main);
            for i in 0..3 {
                p.disc(0.34 + i as f32 * 0.16, 0.42, 0.045, p.carve);
            }
        }
        IconKind::Paint => {
            p.disc(0.46, 0.50, 0.36, p.main); // palette
            p.disc(0.62, 0.58, 0.09, p.carve); // thumb hole
            p.disc(0.34, 0.34, 0.055, p.carve);
            p.disc(0.56, 0.28, 0.055, p.carve);
            p.disc(0.30, 0.56, 0.055, p.carve);
        }
        IconKind::Snake => {
            let pts = [(0.24, 0.66), (0.38, 0.66), (0.38, 0.50), (0.54, 0.50), (0.54, 0.34), (0.70, 0.34)];
            for (cx, cy) in pts {
                p.disc(cx, cy, 0.095, p.main);
            }
            p.disc(0.72, 0.31, 0.03, p.carve); // eye
        }
        IconKind::Dice2048 => {
            p.round(0.14, 0.14, 0.72, 0.72, 0.10, p.main);
            for r in 0..2 {
                for c in 0..2 {
                    p.round(0.22 + c as f32 * 0.32, 0.22 + r as f32 * 0.32, 0.24, 0.24, 0.04, p.carve);
                }
            }
        }
        IconKind::Mine => {
            for (dx, dy) in [(0.0, -0.42), (0.0, 0.42), (-0.42, 0.0), (0.42, 0.0),
                              (-0.30, -0.30), (0.30, -0.30), (-0.30, 0.30), (0.30, 0.30)] {
                p.line(0.5, 0.5, 0.5 + dx, 0.5 + dy, 0.07, p.main); // spikes
            }
            p.disc(0.5, 0.5, 0.26, p.main);
            p.disc(0.42, 0.42, 0.055, p.carve); // glint
        }
        IconKind::Disk => {
            p.round(0.16, 0.16, 0.68, 0.68, 0.06, p.main); // floppy body
            p.rect(0.30, 0.16, 0.34, 0.20, p.carve);       // metal shutter (cut)
            p.rect(0.50, 0.17, 0.06, 0.16, p.main);
            p.round(0.30, 0.50, 0.40, 0.30, 0.02, p.carve); // label (cut)
        }
        IconKind::Settings => {
            for i in 0..8 {
                let ang = i as f32 * std::f32::consts::PI / 4.0;
                let (dx, dy) = (ang.cos() * 0.40, ang.sin() * 0.40);
                p.disc(0.5 + dx, 0.5 + dy, 0.10, p.main); // gear teeth
            }
            p.disc(0.5, 0.5, 0.30, p.main); // body
            p.disc(0.5, 0.5, 0.12, p.carve); // hub hole
        }
        IconKind::Browser => {
            p.disc(0.5, 0.5, 0.42, p.main);
            p.line(0.12, 0.5, 0.88, 0.5, 0.035, p.carve); // equator
            p.line(0.18, 0.32, 0.82, 0.32, 0.03, p.carve); // parallels
            p.line(0.18, 0.68, 0.82, 0.68, 0.03, p.carve);
            p.line(0.5, 0.08, 0.5, 0.92, 0.035, p.carve); // central meridian
            p.line(0.30, 0.12, 0.30, 0.88, 0.03, p.carve);
            p.line(0.70, 0.12, 0.70, 0.88, 0.03, p.carve);
        }
        IconKind::Image => {
            p.round(0.14, 0.18, 0.72, 0.64, 0.05, p.main);
            p.disc(0.33, 0.36, 0.07, p.carve); // sun (cut)
            // mountains carved as triangles via lines
            p.line(0.30, 0.62, 0.46, 0.42, 0.10, p.carve);
            p.line(0.46, 0.42, 0.60, 0.62, 0.10, p.carve);
            p.line(0.54, 0.66, 0.66, 0.50, 0.09, p.carve);
            p.line(0.66, 0.50, 0.78, 0.66, 0.09, p.carve);
            p.rect(0.18, 0.66, 0.64, 0.16, p.carve);
        }
        IconKind::Network => {
            // Wi-Fi: three nested arcs (drawn as ring discs carved) + dot.
            p.disc(0.5, 0.62, 0.42, p.main);
            p.disc(0.5, 0.62, 0.34, p.carve);
            p.disc(0.5, 0.62, 0.27, p.main);
            p.disc(0.5, 0.62, 0.19, p.carve);
            p.rect(0.10, 0.62, 0.80, 0.40, p.carve); // mask lower half → arcs
            p.disc(0.5, 0.60, 0.06, p.main); // base dot
        }
        IconKind::Power => {
            p.disc(0.5, 0.54, 0.36, p.main);
            p.disc(0.5, 0.54, 0.24, p.carve);
            p.rect(0.30, 0.30, 0.40, 0.30, p.carve); // open the ring at top
            p.rect(0.455, 0.18, 0.09, 0.30, p.main); // the bar
        }
        IconKind::Music => {
            p.disc(0.34, 0.72, 0.09, p.main);
            p.disc(0.68, 0.64, 0.09, p.main);
            p.rect(0.41, 0.24, 0.05, 0.50, p.main);
            p.rect(0.73, 0.16, 0.05, 0.50, p.main);
            p.rect(0.41, 0.22, 0.37, 0.07, p.main); // beam
        }
        IconKind::Messages => {
            // WhatsApp-style: a rounded speech bubble with a down-left tail
            // and a carved phone handset.
            p.round(0.12, 0.14, 0.76, 0.56, 0.16, p.main);
            p.line(0.28, 0.62, 0.20, 0.86, 0.13, p.main); // tail
            p.line(0.20, 0.86, 0.46, 0.62, 0.13, p.main);
            p.disc(0.37, 0.36, 0.07, p.carve); // earpiece
            p.disc(0.60, 0.52, 0.07, p.carve); // mouthpiece
            p.line(0.37, 0.36, 0.60, 0.52, 0.075, p.carve); // handset body
        }
        IconKind::Discord => {
            // Discord-style "face": a rounded gamepad-ish body, two pulled
            // bottom corners, and two carved eyes.
            p.round(0.10, 0.24, 0.80, 0.50, 0.22, p.main);
            p.line(0.24, 0.70, 0.32, 0.84, 0.11, p.main); // left foot
            p.line(0.76, 0.70, 0.68, 0.84, 0.11, p.main); // right foot
            p.disc(0.37, 0.47, 0.085, p.carve); // left eye
            p.disc(0.63, 0.47, 0.085, p.carve); // right eye
        }
        IconKind::Mail => {
            // Envelope: a body with a carved V flap and a 1px seam.
            p.round(0.12, 0.24, 0.76, 0.52, 0.05, p.main);
            p.line(0.13, 0.27, 0.50, 0.53, 0.05, p.carve); // flap left
            p.line(0.50, 0.53, 0.87, 0.27, 0.05, p.carve); // flap right
            p.line(0.13, 0.73, 0.37, 0.52, 0.035, p.carve); // lower folds
            p.line(0.87, 0.73, 0.63, 0.52, 0.035, p.carve);
        }
        IconKind::Calendar => {
            p.round(0.14, 0.18, 0.72, 0.68, 0.06, p.main);
            p.rect(0.14, 0.20, 0.72, 0.14, p.soft);   // header band
            p.rect(0.14, 0.34, 0.72, 0.02, p.carve);  // seam under header
            p.rect(0.30, 0.10, 0.05, 0.16, p.main);   // binding ring (left)
            p.rect(0.65, 0.10, 0.05, 0.16, p.main);   // binding ring (right)
            for r in 0..2 {
                for c in 0..3 {
                    let bx = 0.25 + c as f32 * 0.18;
                    let by = 0.44 + r as f32 * 0.20;
                    p.rect(bx, by, 0.10, 0.10, p.carve); // day cells (cut)
                }
            }
        }
        IconKind::Contacts => {
            p.round(0.12, 0.16, 0.76, 0.68, 0.07, p.main); // card
            p.disc(0.34, 0.40, 0.10, p.carve);             // avatar head
            p.round(0.21, 0.55, 0.26, 0.20, 0.09, p.carve);// avatar shoulders
            p.rect(0.55, 0.36, 0.26, 0.05, p.carve);       // name line
            p.rect(0.55, 0.48, 0.26, 0.05, p.carve);       // detail line
            p.rect(0.55, 0.60, 0.17, 0.05, p.carve);       // detail line
        }
        IconKind::Gallery => {
            p.round(0.22, 0.16, 0.60, 0.46, 0.04, p.soft); // back photo
            p.round(0.14, 0.32, 0.62, 0.50, 0.04, p.main); // front photo
            p.disc(0.30, 0.46, 0.06, p.carve);             // sun (cut)
            p.line(0.18, 0.74, 0.36, 0.54, 0.08, p.carve); // peak 1
            p.line(0.36, 0.54, 0.52, 0.76, 0.08, p.carve);
            p.line(0.48, 0.72, 0.60, 0.58, 0.07, p.carve); // peak 2
            p.line(0.60, 0.58, 0.72, 0.74, 0.07, p.carve);
            p.rect(0.14, 0.74, 0.62, 0.08, p.carve);       // ground band
        }
        IconKind::Generic => {
            p.round(0.18, 0.18, 0.64, 0.64, 0.10, p.main);
            p.round(0.34, 0.34, 0.32, 0.32, 0.05, p.carve);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_maps_known_and_unknown() {
        assert_eq!(IconKind::from_name("files"), IconKind::Folder);
        assert_eq!(IconKind::from_name("png"), IconKind::Image);
        assert_eq!(IconKind::from_name("totally-unknown"), IconKind::Generic);
    }

    #[test]
    fn every_icon_paints_something() {
        // Each kind must light at least one pixel in its box — proof the
        // drawing isn't a no-op (a blank tile reads as a broken icon).
        let kinds = [
            IconKind::Folder, IconKind::Document, IconKind::Note, IconKind::Tasks,
            IconKind::Terminal, IconKind::Calculator, IconKind::Clock, IconKind::Monitor,
            IconKind::Info, IconKind::Chat, IconKind::Paint, IconKind::Snake,
            IconKind::Dice2048, IconKind::Mine, IconKind::Disk, IconKind::Settings,
            IconKind::Browser, IconKind::Image, IconKind::Network, IconKind::Power,
            IconKind::Music, IconKind::Messages, IconKind::Discord, IconKind::Mail,
            IconKind::Calendar, IconKind::Contacts, IconKind::Gallery, IconKind::Generic,
        ];
        for k in kinds {
            let mut fb = Framebuffer::in_memory(48, 48);
            draw_icon(&mut fb, 0, 0, 48, k, Pixel::WHITE, Pixel::rgb(0x20, 0x60, 0xC0));
            let lit = (0..48).any(|y| (0..48).any(|x| fb.get_pixel(x, y) != Pixel::BLACK));
            assert!(lit, "icon {k:?} painted nothing");
        }
    }
}
