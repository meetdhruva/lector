//! Terminal palettes per theme and mapping of the core's [`TermColor`]s to colors.
//!
//! The four built-in themes have hand-picked ANSI palettes (tested for contrast against
//! their background); any other theme gets one derived from its editor colors. Kept in this
//! crate, keyed by theme name, so `lector-theme` stays untouched.

use lector_terminal::TermColor;
use lector_theme::{Color, Theme};

/// Colors a terminal is drawn with.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalPalette {
    /// ANSI 0-15 (normal 0-7, bright 8-15).
    pub ansi: [Color; 16],
    /// Default foreground.
    pub foreground: Color,
    /// Default background.
    pub background: Color,
    /// Cursor.
    pub cursor: Color,
    /// Text under a block cursor.
    pub cursor_text: Color,
    /// Selection highlight (translucent, painted over cell backgrounds).
    pub selection: Color,
    /// Search matches.
    pub search_match: Color,
    /// The current search match.
    pub search_match_active: Color,
    /// Hovered links.
    pub link: Color,
    /// Failed-command marker.
    pub error: Color,
    /// Succeeded-command marker.
    pub success: Color,
}

fn rgb(hex: u32) -> Color {
    Color {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a: 255,
    }
}

fn ansi(hex: [u32; 16]) -> [Color; 16] {
    hex.map(rgb)
}

/// The hand-picked ANSI colors of a built-in theme.
fn builtin_ansi(name: &str) -> Option<[Color; 16]> {
    Some(match name {
        lector_theme::LECTOR_DARK => ansi([
            0x3f4451, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
            0x6b7280, 0xf08a92, 0xb5d99c, 0xf0d29a, 0x8cc4f5, 0xd7a1e7, 0x7fcbd5, 0xe6e8ec,
        ]),
        lector_theme::LECTOR_LIGHT => ansi([
            0x24292f, 0xc4314b, 0x1a7f37, 0x8a5d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
            0x57606a, 0xa40e26, 0x116329, 0x704b00, 0x0550ae, 0x6639ba, 0x136067, 0x8c959f,
        ]),
        lector_theme::LECTOR_HC_DARK => ansi([
            0x4d4d4d, 0xff6b6b, 0x5cf27a, 0xffe066, 0x7cc0ff, 0xff8ce6, 0x5ef2f2, 0xe6e6e6,
            0xa6a6a6, 0xff9e9e, 0x9dffaf, 0xfff0a8, 0xaad6ff, 0xffb3f0, 0xa3fafa, 0xffffff,
        ]),
        lector_theme::LECTOR_HC_LIGHT => ansi([
            0x000000, 0xa1001d, 0x005c12, 0x5c4300, 0x0030b8, 0x7a0099, 0x00525c, 0x3d3d3d,
            0x2e2e2e, 0x8a0019, 0x004d0f, 0x4a3600, 0x002590, 0x620080, 0x00424a, 0x575757,
        ]),
        _ => return None,
    })
}

/// ANSI colors derived from a theme's editor and status colors.
fn derived_ansi(theme: &Theme) -> [Color; 16] {
    let ui = &theme.ui;
    let syntax = |capture: &str, fallback: Color| {
        theme
            .syntax
            .style_for(capture)
            .map(|s| s.color)
            .unwrap_or(fallback)
    };
    let dark = theme.appearance.is_dark();
    let (black, white) = if dark {
        (
            ui.background.mix(ui.text, 0.18),
            ui.text.mix(ui.background, 0.2),
        )
    } else {
        (ui.text, ui.text.mix(ui.background, 0.45))
    };
    let normal = [
        black,
        ui.error,
        ui.success,
        ui.warning,
        ui.info,
        syntax("keyword", ui.accent),
        syntax("string.escape", ui.link),
        white,
    ];
    let toward = if dark { ui.text } else { ui.background };
    let mut out = [Color::TRANSPARENT; 16];
    for (i, c) in normal.iter().enumerate() {
        out[i] = Color { a: 255, ..*c };
        out[i + 8] = Color {
            a: 255,
            ..c.mix(toward, 0.25)
        };
    }
    out[8] = Color {
        a: 255,
        ..black.mix(ui.text, 0.3)
    };
    out[15] = Color {
        a: 255,
        ..if dark {
            ui.text
        } else {
            ui.text.mix(ui.background, 0.3)
        }
    };
    out
}

impl TerminalPalette {
    /// The palette for `theme` (built-in palettes by name, else derived).
    pub fn for_theme(theme: &Theme) -> Self {
        let ui = &theme.ui;
        let ansi = builtin_ansi(&theme.name).unwrap_or_else(|| derived_ansi(theme));
        let background = Color {
            a: 255,
            ..ui.background
        };
        TerminalPalette {
            ansi,
            foreground: ui.text,
            background,
            cursor: ui.cursor,
            cursor_text: background,
            selection: ui.selection,
            search_match: ui.search_match,
            search_match_active: ui.search_match_active,
            link: ui.link,
            error: ui.error,
            success: ui.success,
        }
    }

    /// Color of a 256-color palette index.
    pub fn indexed(&self, index: u8) -> Color {
        match index {
            0..=15 => self.ansi[usize::from(index)],
            16..=231 => {
                let i = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                Color {
                    r: level(i / 36),
                    g: level((i / 6) % 6),
                    b: level(i % 6),
                    a: 255,
                }
            }
            232..=255 => {
                let v = 8 + (index - 232) * 10;
                Color {
                    r: v,
                    g: v,
                    b: v,
                    a: 255,
                }
            }
        }
    }

    /// Resolves a cell color.
    pub fn resolve(&self, color: TermColor) -> Color {
        match color {
            TermColor::Foreground => self.foreground,
            TermColor::Background => self.background,
            TermColor::Cursor => self.cursor,
            TermColor::BrightForeground => self.ansi[15].mix(self.foreground, 0.5),
            TermColor::DimForeground => self.dim(self.foreground),
            TermColor::Indexed(i) => self.indexed(i),
            TermColor::DimIndexed(i) => self.dim(self.indexed(i & 7)),
            TermColor::Rgb(r, g, b) => Color { r, g, b, a: 255 },
        }
    }

    /// A dimmed (SGR 2) version of `color`, halfway to the background.
    pub fn dim(&self, color: Color) -> Color {
        color.mix(self.background, 0.4)
    }

    /// The (foreground, background) of a cell after inverse and dim. Background is `None`
    /// when it is the terminal's default (nothing to paint).
    pub fn cell_colors(
        &self,
        fg: TermColor,
        bg: TermColor,
        inverse: bool,
        dim: bool,
    ) -> (Color, Option<Color>) {
        let mut f = self.resolve(fg);
        let mut b = (bg != TermColor::Background).then(|| self.resolve(bg));
        if inverse {
            let old_f = f;
            f = b.unwrap_or(self.background);
            b = Some(old_f);
        }
        if dim {
            f = self.dim(f);
        }
        (f, b)
    }

    /// The colors answered to programs asking (OSC 10/11/12/4 queries).
    pub fn query_colors(&self) -> lector_terminal::QueryColors {
        let t = |c: Color| (c.r, c.g, c.b);
        lector_terminal::QueryColors {
            foreground: t(self.foreground),
            background: t(self.background),
            cursor: t(self.cursor),
            ansi: self.ansi.map(t),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_theme::builtin_themes;

    #[test]
    fn builtin_palettes_are_readable() {
        for theme in builtin_themes() {
            let p = TerminalPalette::for_theme(&theme);
            assert!(builtin_ansi(&theme.name).is_some(), "{}", theme.name);
            let min = if theme.name.contains("High Contrast") {
                7.0
            } else {
                4.5
            };
            for i in (1..=6).chain(9..=14) {
                let ratio = p.ansi[i].contrast_ratio(p.background);
                assert!(ratio >= min, "{} ansi {i}: {ratio:.2}", theme.name);
            }
            assert!(p.foreground.contrast_ratio(p.background) >= min);
            // Normal and bright differ.
            for i in 0..8 {
                assert_ne!(p.ansi[i], p.ansi[i + 8], "{} {i}", theme.name);
            }
        }
    }

    #[test]
    fn derived_palette_for_unknown_themes() {
        let mut theme = builtin_themes().remove(0);
        theme.name = "Some User Theme".into();
        let p = TerminalPalette::for_theme(&theme);
        assert!(p.ansi.iter().all(|c| c.a == 255));
        assert_eq!(
            p.ansi[1],
            Color {
                a: 255,
                ..theme.ui.error
            }
        );
    }

    #[test]
    fn color_mapping() {
        let theme = builtin_themes().remove(0);
        let p = TerminalPalette::for_theme(&theme);
        assert_eq!(p.resolve(TermColor::Indexed(1)), p.ansi[1]);
        assert_eq!(p.resolve(TermColor::Indexed(9)), p.ansi[9]);
        assert_eq!(
            p.indexed(16),
            Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(
            p.indexed(21),
            Color {
                r: 0,
                g: 0,
                b: 255,
                a: 255
            }
        );
        assert_eq!(
            p.indexed(196),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(
            p.indexed(231),
            Color {
                r: 255,
                g: 255,
                b: 255,
                a: 255
            }
        );
        assert_eq!(
            p.indexed(232),
            Color {
                r: 8,
                g: 8,
                b: 8,
                a: 255
            }
        );
        assert_eq!(
            p.indexed(255),
            Color {
                r: 238,
                g: 238,
                b: 238,
                a: 255
            }
        );
        assert_eq!(
            p.resolve(TermColor::Rgb(1, 2, 3)),
            Color {
                r: 1,
                g: 2,
                b: 3,
                a: 255
            }
        );
        assert_eq!(p.resolve(TermColor::Foreground), p.foreground);
        // Default background paints nothing; inverse swaps.
        let (f, b) = p.cell_colors(TermColor::Foreground, TermColor::Background, false, false);
        assert_eq!((f, b), (p.foreground, None));
        let (f, b) = p.cell_colors(TermColor::Foreground, TermColor::Background, true, false);
        assert_eq!((f, b), (p.background, Some(p.foreground)));
        let (f, _) = p.cell_colors(TermColor::Indexed(2), TermColor::Background, false, true);
        assert_ne!(f, p.ansi[2], "dim changes the color");
        assert_eq!(
            p.query_colors().ansi[3],
            (p.ansi[3].r, p.ansi[3].g, p.ansi[3].b)
        );
    }
}
