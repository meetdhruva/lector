//! Output helpers: colors (only on a terminal, never with `NO_COLOR`), JSON printing,
//! timestamps.

use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::args::ColorChoice;

/// Streams and terminal facts every subcommand writes to.
pub struct Io<'a> {
    /// Standard output.
    pub out: &'a mut dyn Write,
    /// Standard error.
    pub err: &'a mut dyn Write,
    /// Colors for human output.
    pub style: Style,
}

/// ANSI colors, switched on or off once.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    /// Emit escape sequences.
    pub color: bool,
}

impl Style {
    /// Decides from the user's choice and the environment. `stdout_is_tty` is whether
    /// standard output is a terminal.
    pub fn resolve(choice: ColorChoice, stdout_is_tty: bool) -> Style {
        let color = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
                let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
                stdout_is_tty && !no_color && !dumb
            }
        };
        Style { color }
    }

    /// No colors.
    pub fn plain() -> Style {
        Style { color: false }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// Bold.
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    /// Dim.
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    /// Red.
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    /// Green.
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    /// Yellow.
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    /// Blue.
    pub fn blue(&self, s: &str) -> String {
        self.paint("34", s)
    }
    /// Magenta.
    pub fn magenta(&self, s: &str) -> String {
        self.paint("35", s)
    }
    /// Cyan.
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

/// Prints `value` as one pretty JSON document with a trailing newline. A `"schema"` field
/// with the output format version is added to objects that lack one.
pub fn print_json(out: &mut dyn Write, mut value: Value) -> io::Result<()> {
    if let Value::Object(map) = &mut value {
        if !map.contains_key("schema") {
            let mut with_schema = serde_json::Map::new();
            with_schema.insert("schema".into(), Value::from(crate::JSON_SCHEMA_VERSION));
            with_schema.extend(std::mem::take(map));
            *map = with_schema;
        }
    }
    let text = serde_json::to_string_pretty(&value).map_err(io::Error::other)?;
    writeln!(out, "{text}")
}

/// `2026-11-06T10:22:33Z` for a point in time (UTC).
pub fn rfc3339(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Pads or truncates (with `…`) `s` to exactly `width` characters.
pub fn fit(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        format!("{s}{}", " ".repeat(width - n))
    } else {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Truncates to at most `width` characters, ending in `…` when cut.
pub fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)), // 2000-02-29
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn colors_follow_choice() {
        assert_eq!(Style::plain().red("x"), "x");
        assert_eq!(Style { color: true }.red("x"), "\x1b[31mx\x1b[0m");
        assert!(Style::resolve(ColorChoice::Always, false).color);
        assert!(!Style::resolve(ColorChoice::Never, true).color);
        assert!(!Style::resolve(ColorChoice::Auto, false).color);
    }

    #[test]
    fn fitting() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("ab", 4), "ab");
    }
}
