//! Terminal color palette and ANSI color resolution.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui::{Hsla, Rgba};

/// Colors used to draw a terminal. Values follow the prototype's `TERM` tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalPalette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    /// Dimmed text color (`--tdim`).
    pub dim: Rgb,
    /// ANSI 0–15.
    pub ansi: [Rgb; 16],
    pub selection: Hsla,
    pub search_match: Hsla,
    pub search_active: Hsla,
}

const fn hex(v: u32) -> Rgb {
    Rgb { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 }
}

fn rgba(c: Rgb, a: f32) -> Hsla {
    Rgba { r: c.r as f32 / 255., g: c.g as f32 / 255., b: c.b as f32 / 255., a }.into()
}

pub fn to_hsla(c: Rgb) -> Hsla {
    rgba(c, 1.0)
}

impl TerminalPalette {
    pub fn light() -> Self {
        let (fg, bg) = (hex(0x24272E), hex(0xFBFBFD));
        let (r, g, y, b, m, c) =
            (hex(0xD7383D), hex(0x14874C), hex(0x9A6200), hex(0x2457D6), hex(0x8A3FC4), hex(0x0B7F8C));
        Self {
            foreground: fg,
            background: bg,
            cursor: fg,
            dim: hex(0x7D8490),
            ansi: [
                fg, r, g, y, b, m, c, hex(0xB8BDC6),
                hex(0x7D8490), hex(0xE5484D), hex(0x1FA463), hex(0xB87700), hex(0x2F6BF5), hex(0xA052D8), hex(0x0E96A6), hex(0xE6E8ED),
            ],
            selection: rgba(hex(0x2F6BF5), 0.22),
            search_match: rgba(hex(0xFFC400), 0.38),
            search_active: rgba(hex(0xFF8A00), 0.55),
        }
    }

    pub fn dark() -> Self {
        let (fg, bg) = (hex(0xD7DAE0), hex(0x1F2126));
        let (r, g, y, b, m, c) =
            (hex(0xFF6B6B), hex(0x5FD18B), hex(0xF2C063), hex(0x6CA6FF), hex(0xC792EA), hex(0x56D4DD));
        Self {
            foreground: fg,
            background: bg,
            cursor: fg,
            dim: hex(0x7A808B),
            ansi: [
                hex(0x2C2E34), r, g, y, b, m, c, fg,
                hex(0x7A808B), hex(0xFF8787), hex(0x7DE0A3), hex(0xF7D184), hex(0x8DBAFF), hex(0xD7A8F0), hex(0x7CE2E9), hex(0xFFFFFF),
            ],
            selection: rgba(hex(0x5B8CFF), 0.32),
            search_match: rgba(hex(0xF2C063), 0.38),
            search_active: rgba(hex(0xFF9F43), 0.6),
        }
    }

    /// Resolves an ANSI color to RGB, honoring OSC 4/10/11 overrides set by programs.
    pub fn rgb(&self, color: &Color, overrides: &Colors) -> Rgb {
        match *color {
            Color::Spec(rgb) => rgb,
            Color::Indexed(i) => overrides[i as usize].unwrap_or_else(|| self.indexed(i)),
            Color::Named(named) => overrides[named as usize].unwrap_or_else(|| self.named(named)),
        }
    }

    pub fn named(&self, named: NamedColor) -> Rgb {
        use NamedColor::*;
        match named {
            Foreground | BrightForeground => self.foreground,
            Background => self.background,
            Cursor => self.cursor,
            DimForeground => self.dim,
            DimBlack | DimRed | DimGreen | DimYellow | DimBlue | DimMagenta | DimCyan | DimWhite => {
                let base = self.ansi[named as usize - DimBlack as usize];
                mix(base, self.background, 0.33)
            }
            other => self.ansi[(other as usize).min(15)],
        }
    }

    pub fn indexed(&self, i: u8) -> Rgb {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
                let i = i - 16;
                Rgb { r: STEPS[(i / 36) as usize], g: STEPS[(i / 6 % 6) as usize], b: STEPS[(i % 6) as usize] }
            }
            232..=255 => {
                let v = 8 + 10 * (i - 232);
                Rgb { r: v, g: v, b: v }
            }
        }
    }
}

/// Linear blend of `a` toward `b` by `t` (0 = a, 1 = b).
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgb { r: f(a.r, b.r), g: f(a.g, b.g), b: f(a.b, b.b) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_cube_and_grayscale() {
        let p = TerminalPalette::dark();
        assert_eq!(p.indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(p.indexed(231), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(p.indexed(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(p.indexed(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(p.indexed(255), Rgb { r: 238, g: 238, b: 238 });
    }

    #[test]
    fn named_colors_map_to_palette() {
        let p = TerminalPalette::light();
        assert_eq!(p.named(NamedColor::Red), hex(0xD7383D));
        assert_eq!(p.named(NamedColor::BrightBlack), p.dim);
        assert_eq!(p.named(NamedColor::Foreground), p.foreground);
        assert_eq!(p.named(NamedColor::Background), p.background);
    }

    #[test]
    fn overrides_take_precedence() {
        let p = TerminalPalette::light();
        let mut colors = Colors::default();
        colors[NamedColor::Foreground] = Some(hex(0x123456));
        assert_eq!(p.rgb(&Color::Named(NamedColor::Foreground), &colors), hex(0x123456));
        assert_eq!(p.rgb(&Color::Indexed(1), &colors), hex(0xD7383D));
    }
}
