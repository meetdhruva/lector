//! sRGB color with alpha, parsing and WCAG contrast helpers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An 8-bit-per-channel sRGB color with straight (non-premultiplied) alpha.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Color {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel; 255 is opaque.
    pub a: u8,
}

/// Error returned when a color string is not `#rgb`, `#rrggbb` or `#rrggbbaa`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid color {0:?}: expected #rgb, #rrggbb or #rrggbbaa")]
pub struct ParseColorError(pub String);

impl Color {
    /// Fully transparent black.
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);
    /// Opaque black.
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    /// Opaque color from channels.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }

    /// Color from channels including alpha.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// Opaque color from a `0xRRGGBB` literal.
    pub const fn hex(rgb: u32) -> Color {
        Color::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }

    /// Color from a `0xRRGGBBAA` literal.
    pub const fn hexa(rgba: u32) -> Color {
        Color::rgba(
            (rgba >> 24) as u8,
            (rgba >> 16) as u8,
            (rgba >> 8) as u8,
            rgba as u8,
        )
    }

    /// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` (the `#` is optional).
    pub fn parse(s: &str) -> Result<Color, ParseColorError> {
        let err = || ParseColorError(s.to_string());
        let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err());
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| err());
        let nib = |i: usize| {
            u8::from_str_radix(&hex[i..i + 1], 16)
                .map(|v| v * 17)
                .map_err(|_| err())
        };
        match hex.len() {
            3 => Ok(Color::rgb(nib(0)?, nib(1)?, nib(2)?)),
            4 => Ok(Color::rgba(nib(0)?, nib(1)?, nib(2)?, nib(3)?)),
            6 => Ok(Color::rgb(byte(0)?, byte(2)?, byte(4)?)),
            8 => Ok(Color::rgba(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => Err(err()),
        }
    }

    /// `#rrggbb` when opaque, `#rrggbbaa` otherwise.
    pub fn to_hex(self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }

    /// Same color with a new alpha in `0.0..=1.0`.
    pub fn with_alpha(self, alpha: f32) -> Color {
        Color {
            a: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
            ..self
        }
    }

    /// Alpha as `0.0..=1.0`.
    pub fn alpha(self) -> f32 {
        self.a as f32 / 255.0
    }

    /// Composites `self` over an opaque-or-not `background` (source-over).
    pub fn over(self, background: Color) -> Color {
        let fa = self.alpha();
        let ba = background.alpha();
        let out_a = fa + ba * (1.0 - fa);
        if out_a <= 0.0 {
            return Color::TRANSPARENT;
        }
        let ch = |f: u8, b: u8| {
            let v = (f as f32 * fa + b as f32 * ba * (1.0 - fa)) / out_a;
            v.round().clamp(0.0, 255.0) as u8
        };
        Color {
            r: ch(self.r, background.r),
            g: ch(self.g, background.g),
            b: ch(self.b, background.b),
            a: (out_a * 255.0).round() as u8,
        }
    }

    /// Linear interpolation between two colors (including alpha); `t` in `0..=1`.
    pub fn mix(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let ch = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Color {
            r: ch(self.r, other.r),
            g: ch(self.g, other.g),
            b: ch(self.b, other.b),
            a: ch(self.a, other.a),
        }
    }

    /// WCAG 2.x relative luminance of the color's RGB (alpha ignored).
    pub fn relative_luminance(self) -> f32 {
        fn lin(c: u8) -> f32 {
            let c = c as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// WCAG contrast ratio (1..=21) of this color drawn over `background`.
    ///
    /// Translucent foregrounds are composited over the background first; a
    /// translucent background is composited over black (callers should pass
    /// an opaque, already-composited background for accurate results).
    pub fn contrast_ratio(self, background: Color) -> f32 {
        let bg = if background.a == 255 {
            background
        } else {
            background.over(Color::BLACK)
        };
        let fg = self.over(bg);
        let (l1, l2) = (fg.relative_luminance(), bg.relative_luminance());
        let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Hue (degrees), saturation and lightness, each `0..=1` except hue.
    pub fn to_hsl(self) -> (f32, f32, f32) {
        let r = self.r as f32 / 255.0;
        let g = self.g as f32 / 255.0;
        let b = self.b as f32 / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        if (max - min).abs() < f32::EPSILON {
            return (0.0, 0.0, l);
        }
        let d = max - min;
        let s = if l > 0.5 {
            d / (2.0 - max - min)
        } else {
            d / (max + min)
        };
        let h = if max == r {
            ((g - b) / d + if g < b { 6.0 } else { 0.0 }) * 60.0
        } else if max == g {
            ((b - r) / d + 2.0) * 60.0
        } else {
            ((r - g) / d + 4.0) * 60.0
        };
        (h, s, l)
    }
}

impl fmt::Debug for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Color({})", self.to_hex())
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for Color {
    type Err = ParseColorError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Color::parse(s)
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        assert_eq!(Color::parse("#fff").unwrap(), Color::WHITE);
        assert_eq!(Color::parse("#000000").unwrap(), Color::BLACK);
        assert_eq!(
            Color::parse("#11223344").unwrap(),
            Color::rgba(0x11, 0x22, 0x33, 0x44)
        );
        assert_eq!(Color::parse("abc").unwrap(), Color::rgb(0xaa, 0xbb, 0xcc));
        assert!(Color::parse("#12345").is_err());
        assert!(Color::parse("#gg0000").is_err());
        assert!(Color::parse("#ééé").is_err());
    }

    #[test]
    fn hex_roundtrip() {
        for s in ["#1c1e23", "#ffffff80"] {
            assert_eq!(Color::parse(s).unwrap().to_hex(), s);
        }
    }

    #[test]
    fn contrast_extremes() {
        let r = Color::BLACK.contrast_ratio(Color::WHITE);
        assert!((r - 21.0).abs() < 0.01, "{r}");
        assert!((Color::WHITE.contrast_ratio(Color::WHITE) - 1.0).abs() < 0.001);
    }

    #[test]
    fn compositing() {
        let c = Color::WHITE.with_alpha(0.5).over(Color::BLACK);
        assert_eq!(c, Color::rgb(128, 128, 128));
    }
}
