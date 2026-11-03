//! Small helpers shared by the git views: relative dates, status letters,
//! colors.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{App, Hsla};
use lector_git::{ChangeKind, RefKind};
use lector_ui::{ActiveTheme, ColorExt};

/// Seconds since the epoch, now.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "just now", "5 min ago", "3 hours ago", "2 days ago", "4 months ago".
pub fn relative_time(time: i64, now: i64) -> String {
    let d = (now - time).max(0);
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => plural(d / 3600, "hour"),
        86_400..=604_799 => plural(d / 86_400, "day"),
        604_800..=2_629_799 => plural(d / 604_800, "week"),
        2_629_800..=31_557_599 => plural(d / 2_629_800, "month"),
        _ => plural(d / 31_557_600, "year"),
    }
}

/// Compact relative time for narrow columns: "now", "5m", "3h", "2d", "4w", "5mo", "2y".
pub fn short_relative_time(time: i64, now: i64) -> String {
    let d = (now - time).max(0);
    match d {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", d / 60),
        3600..=86_399 => format!("{}h", d / 3600),
        86_400..=604_799 => format!("{}d", d / 86_400),
        604_800..=2_629_799 => format!("{}w", d / 604_800),
        2_629_800..=31_557_599 => format!("{}mo", d / 2_629_800),
        _ => format!("{}y", d / 31_557_600),
    }
}

/// Absolute date "2026-11-03 14:05" in the signature's own offset.
pub fn format_date(time: i64, offset_minutes: i32) -> String {
    let t = time + offset_minutes as i64 * 60;
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let off = offset_minutes.abs();
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} {sign}{:02}{:02}",
        secs / 3600,
        (secs / 60) % 60,
        off / 60,
        off % 60
    )
}

/// Days since 1970-01-01 → (year, month, day). (Howard Hinnant's algorithm.)
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

/// One-letter status code.
pub fn kind_letter(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
        ChangeKind::Copied => "C",
        ChangeKind::TypeChange => "T",
    }
}

/// Human label for a change kind.
pub fn kind_label(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "Added",
        ChangeKind::Modified => "Modified",
        ChangeKind::Deleted => "Deleted",
        ChangeKind::Renamed => "Renamed",
        ChangeKind::Copied => "Copied",
        ChangeKind::TypeChange => "Type changed",
    }
}

/// Theme color for a change kind.
pub fn kind_color(kind: ChangeKind, cx: &App) -> Hsla {
    let git = &cx.theme().git;
    match kind {
        ChangeKind::Added | ChangeKind::Copied => git.added.hsla(),
        ChangeKind::Modified | ChangeKind::TypeChange => git.modified.hsla(),
        ChangeKind::Deleted => git.deleted.hsla(),
        ChangeKind::Renamed => git.renamed.hsla(),
    }
}

/// Graph lane colors (theme-aware, distinguishable in light and dark).
pub fn lane_color(lane: usize, cx: &App) -> Hsla {
    const HUES: [f32; 8] = [210., 140., 30., 280., 175., 330., 50., 0.];
    let dark = cx.theme().appearance.is_dark();
    let h = HUES[lane % HUES.len()] / 360.;
    if dark {
        gpui::hsla(h, 0.65, 0.62, 1.)
    } else {
        gpui::hsla(h, 0.70, 0.42, 1.)
    }
}

/// Background / foreground for a ref pill.
pub fn ref_colors(kind: RefKind, cx: &App) -> (Hsla, Hsla) {
    let theme = cx.theme();
    let base = match kind {
        RefKind::Head => theme.ui.accent.hsla(),
        RefKind::LocalBranch => theme.git.added.hsla(),
        RefKind::RemoteBranch => theme.git.renamed.hsla(),
        RefKind::Tag => theme.git.modified.hsla(),
    };
    (base.opacity(0.18), base)
}

/// Splits "dir/sub/file.rs" into ("file.rs", "dir/sub").
pub fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (name, dir),
        None => (path, ""),
    }
}

/// "12 KB", "1.4 MB".
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / KB / KB)
    } else {
        format!("{:.1} GB", b / KB / KB / KB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative() {
        assert_eq!(relative_time(100, 110), "just now");
        assert_eq!(relative_time(0, 3 * 3600), "3 hours ago");
        assert_eq!(relative_time(0, 86_400), "1 day ago");
        assert_eq!(relative_time(0, 40 * 86_400), "1 month ago");
        assert_eq!(short_relative_time(0, 2 * 86_400), "2d");
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(0, 0), "1970-01-01 00:00 +0000");
        assert_eq!(format_date(1_700_000_000, 60), "2023-11-14 23:13 +0100");
    }

    #[test]
    fn paths_and_sizes() {
        assert_eq!(split_path("a/b/c.rs"), ("c.rs", "a/b"));
        assert_eq!(split_path("c.rs"), ("c.rs", ""));
        assert_eq!(format_size(12 * 1024), "12 KB");
    }
}
