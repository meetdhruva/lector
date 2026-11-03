//! The four built-in Lector themes, derived from small palettes so the UI,
//! syntax and diff colors stay coherent.

use crate::{Appearance, Color, DiffColors, GitColors, SyntaxTheme, TextStyle, Theme, UiColors};

/// Name of the default dark theme.
pub const LECTOR_DARK: &str = "Lector Dark";
/// Name of the default light theme.
pub const LECTOR_LIGHT: &str = "Lector Light";
/// Name of the high-contrast dark theme.
pub const LECTOR_HC_DARK: &str = "Lector High Contrast Dark";
/// Name of the high-contrast light theme.
pub const LECTOR_HC_LIGHT: &str = "Lector High Contrast Light";

struct Palette {
    name: &'static str,
    appearance: Appearance,
    high_contrast: bool,
    // Surfaces, from the editor outwards.
    bg: u32,
    surface: u32,
    chrome: u32,
    elevated: u32,
    border: u32,
    border_variant: u32,
    // Text.
    text: u32,
    muted: u32,
    faint: u32,
    accent: u32,
    accent_fg: u32,
    // Syntax.
    keyword: u32,
    function: u32,
    ty: u32,
    string: u32,
    number: u32,
    comment: u32,
    property: u32,
    macro_: u32,
    tag: u32,
    operator: u32,
    punct: u32,
    module: u32,
    escape: u32,
    builtin: u32,
    param: u32,
    // Status.
    green: u32,
    red: u32,
    amber: u32,
    blue: u32,
    purple: u32,
}

const DARK: Palette = Palette {
    name: LECTOR_DARK,
    appearance: Appearance::Dark,
    high_contrast: false,
    bg: 0x1c1e23,
    surface: 0x18191d,
    chrome: 0x151619,
    elevated: 0x24262c,
    border: 0x2b2e35,
    border_variant: 0x23252a,
    text: 0xd6d9df,
    muted: 0xa3a9b4,
    faint: 0x717783,
    accent: 0x7aa8ff,
    accent_fg: 0x0d1117,
    keyword: 0xc79bf2,
    function: 0x7fb4ff,
    ty: 0xe8c47c,
    string: 0xa6d38a,
    number: 0xf2a571,
    comment: 0x979dab,
    property: 0x8ccbd9,
    macro_: 0x6fd1c0,
    tag: 0xf2867f,
    operator: 0xb8bfca,
    punct: 0x9ca2ad,
    module: 0xe3a8d9,
    escape: 0xf2c06f,
    builtin: 0xef8f9b,
    param: 0xe6b9a1,
    green: 0x56c46b,
    red: 0xf0706a,
    amber: 0xe0b45a,
    blue: 0x6aa6ff,
    purple: 0xb592f5,
};

const LIGHT: Palette = Palette {
    name: LECTOR_LIGHT,
    appearance: Appearance::Light,
    high_contrast: false,
    bg: 0xfbfbfa,
    surface: 0xf4f4f2,
    chrome: 0xefefec,
    elevated: 0xffffff,
    border: 0xdcdcd8,
    border_variant: 0xe7e7e3,
    text: 0x24292f,
    muted: 0x555d68,
    faint: 0x7d8590,
    accent: 0x2563c9,
    accent_fg: 0xffffff,
    keyword: 0x7238bd,
    function: 0x1a59b8,
    ty: 0x805200,
    string: 0x226c2d,
    number: 0xa3450a,
    comment: 0x5d6570,
    property: 0x0b6880,
    macro_: 0x0d6b5e,
    tag: 0xb0342a,
    operator: 0x4f5864,
    punct: 0x5d6570,
    module: 0x983682,
    escape: 0x855400,
    builtin: 0xab2f4a,
    param: 0x8c4a28,
    green: 0x1a7f37,
    red: 0xcf222e,
    amber: 0x8a5d00,
    blue: 0x0969da,
    purple: 0x8250df,
};

const HC_DARK: Palette = Palette {
    name: LECTOR_HC_DARK,
    appearance: Appearance::Dark,
    high_contrast: true,
    bg: 0x0a0b0d,
    surface: 0x0a0b0d,
    chrome: 0x000000,
    elevated: 0x15171b,
    border: 0x5c626c,
    border_variant: 0x3a3e45,
    text: 0xffffff,
    muted: 0xd0d4da,
    faint: 0xa7adb6,
    accent: 0x9cc3ff,
    accent_fg: 0x000000,
    keyword: 0xdcb8ff,
    function: 0xa5cdff,
    ty: 0xffd98a,
    string: 0xb9f29a,
    number: 0xffbb8a,
    comment: 0xc5cbd4,
    property: 0xa8e6f2,
    macro_: 0x8ff0dc,
    tag: 0xffa9a3,
    operator: 0xe4e8ee,
    punct: 0xd0d4da,
    module: 0xf7c3ef,
    escape: 0xffd27a,
    builtin: 0xffaab4,
    param: 0xf7cfba,
    green: 0x7ee08f,
    red: 0xff8f87,
    amber: 0xffd36b,
    blue: 0x8cbcff,
    purple: 0xd2b6ff,
};

const HC_LIGHT: Palette = Palette {
    name: LECTOR_HC_LIGHT,
    appearance: Appearance::Light,
    high_contrast: true,
    bg: 0xffffff,
    surface: 0xffffff,
    chrome: 0xf2f2f2,
    elevated: 0xffffff,
    border: 0x6b7280,
    border_variant: 0xa0a6ae,
    text: 0x000000,
    muted: 0x30363d,
    faint: 0x545b64,
    accent: 0x0842a6,
    accent_fg: 0xffffff,
    keyword: 0x5a1aa8,
    function: 0x0b3f9a,
    ty: 0x553500,
    string: 0x0f4f19,
    number: 0x7a2d00,
    comment: 0x3b4149,
    property: 0x004d61,
    macro_: 0x00544a,
    tag: 0x8c170e,
    operator: 0x2a3038,
    punct: 0x343a42,
    module: 0x7a1d66,
    escape: 0x5c3a00,
    builtin: 0x8f1530,
    param: 0x6a3416,
    green: 0x116329,
    red: 0xa40e26,
    amber: 0x7a4d00,
    blue: 0x0842a6,
    purple: 0x6232b8,
};

/// All built-in themes, in display order.
pub fn builtin_themes() -> Vec<Theme> {
    [DARK, LIGHT, HC_DARK, HC_LIGHT].iter().map(build).collect()
}

fn c(rgb: u32) -> Color {
    Color::hex(rgb)
}

fn build(p: &Palette) -> Theme {
    let dark = p.appearance.is_dark();
    let hc = p.high_contrast;
    let bg = c(p.bg);
    let text = c(p.text);
    let accent = c(p.accent);
    // Overlays are tinted with the foreground so they work on any surface.
    let fg_overlay = |a: f32| text.with_alpha(a);

    let ui = UiColors {
        background: bg,
        surface: c(p.surface),
        elevated: c(p.elevated),
        panel: c(p.surface),
        border: c(p.border),
        border_variant: c(p.border_variant),
        border_focused: accent,
        text,
        text_muted: c(p.muted),
        text_faint: c(p.faint),
        text_accent: accent,
        accent,
        accent_foreground: c(p.accent_fg),
        selection: accent.with_alpha(if dark { 0.28 } else { 0.22 }),
        selection_inactive: fg_overlay(if dark { 0.10 } else { 0.08 }),
        hover: fg_overlay(if dark { 0.055 } else { 0.05 }),
        active: fg_overlay(if dark { 0.09 } else { 0.085 }),
        active_line: fg_overlay(if hc {
            0.07
        } else if dark {
            0.035
        } else {
            0.03
        }),
        cursor: accent,
        line_number: c(p.faint),
        line_number_active: text,
        indent_guide: fg_overlay(if hc { 0.22 } else { 0.07 }),
        indent_guide_active: fg_overlay(if hc { 0.45 } else { 0.16 }),
        scrollbar_thumb: fg_overlay(if hc { 0.35 } else { 0.14 }),
        scrollbar_thumb_hover: fg_overlay(if hc { 0.55 } else { 0.26 }),
        scrollbar_track: Color::TRANSPARENT,
        tab_bar_bg: c(p.chrome),
        tab_active_bg: bg,
        tab_active_fg: text,
        tab_inactive_bg: c(p.chrome),
        tab_inactive_fg: c(p.muted),
        status_bar_bg: c(p.chrome),
        status_bar_fg: c(p.muted),
        title_bar_bg: c(p.chrome),
        title_bar_fg: text,
        search_match: c(p.amber).with_alpha(if dark { 0.22 } else { 0.25 }),
        search_match_active: c(p.amber).with_alpha(match (dark, hc) {
            (true, true) => 0.30,
            (true, false) => 0.36,
            _ => 0.42,
        }),
        search_match_active_border: c(p.amber),
        find_highlight: c(p.amber).with_alpha(0.16),
        word_highlight: fg_overlay(if dark { 0.09 } else { 0.08 }),
        bracket_match: accent.with_alpha(0.25),
        error: c(p.red),
        warning: c(p.amber),
        info: c(p.blue),
        hint: c(p.faint),
        success: c(p.green),
        link: accent,
        shadow: Color::BLACK.with_alpha(if dark { 0.45 } else { 0.14 }),
        overlay: Color::BLACK.with_alpha(if dark { 0.18 } else { 0.06 }),
        minimap_bg: Color::TRANSPARENT,
        minimap_text: fg_overlay(if dark { 0.22 } else { 0.25 }),
        minimap_slider: fg_overlay(0.07),
        minimap_slider_hover: fg_overlay(0.12),
        sticky_header_bg: bg,
        sticky_header_border: c(p.border),
        tooltip_bg: c(p.elevated),
        tooltip_fg: text,
        tooltip_border: c(p.border),
        inline_annotation: c(p.faint),
        keybinding_bg: fg_overlay(if dark { 0.08 } else { 0.06 }),
        input_bg: if dark {
            Color::BLACK.with_alpha(0.18).over(c(p.elevated))
        } else {
            c(p.surface)
        },
        fold_marker: c(p.faint),
    };

    let s = |rgb: u32| TextStyle::color(c(rgb));
    let syntax = SyntaxTheme::from_pairs([
        ("attribute", s(p.ty)),
        ("boolean", s(p.number)),
        ("comment", s(p.comment).italic()),
        ("comment.doc", s(p.comment).italic()),
        ("constant", s(p.number)),
        ("constant.builtin", s(p.number)),
        ("constructor", s(p.ty)),
        ("embedded", s(p.text)),
        ("function", s(p.function)),
        ("function.builtin", s(p.function)),
        ("function.macro", s(p.macro_)),
        ("function.method", s(p.function)),
        ("keyword", s(p.keyword)),
        ("keyword.control", s(p.keyword)),
        ("keyword.import", s(p.keyword)),
        ("keyword.return", s(p.keyword)),
        ("label", s(p.module)),
        ("module", s(p.module)),
        ("number", s(p.number)),
        ("operator", s(p.operator)),
        ("property", s(p.property)),
        ("punctuation", s(p.punct)),
        ("punctuation.bracket", s(p.punct)),
        ("punctuation.delimiter", s(p.punct)),
        ("punctuation.special", s(p.keyword)),
        ("string", s(p.string)),
        ("string.escape", s(p.escape)),
        ("string.regex", s(p.escape)),
        ("string.special", s(p.escape)),
        ("tag", s(p.tag)),
        ("type", s(p.ty)),
        ("type.builtin", s(p.ty)),
        ("variable", s(p.text)),
        ("variable.builtin", s(p.builtin).italic()),
        ("variable.parameter", s(p.param)),
        ("variable.member", s(p.property)),
        ("markup.heading", s(p.function).bold()),
        ("markup.bold", s(p.text).bold()),
        ("markup.italic", s(p.text).italic()),
        ("markup.link", s(p.accent)),
        ("markup.link.url", s(p.accent)),
        ("markup.raw", s(p.string)),
        ("markup.list", s(p.keyword)),
        ("markup.quote", s(p.comment).italic()),
    ]);

    let (line_a, word_a) = match (dark, hc) {
        (true, false) => (0.11, 0.28),
        (false, false) => (0.09, 0.22),
        (true, true) => (0.16, 0.28),
        (false, true) => (0.09, 0.22),
    };
    let diff = DiffColors {
        added_line_bg: c(p.green).with_alpha(line_a),
        removed_line_bg: c(p.red).with_alpha(line_a),
        added_word_bg: c(p.green).with_alpha(word_a),
        removed_word_bg: c(p.red).with_alpha(word_a),
        added_gutter: c(p.green),
        removed_gutter: c(p.red),
        modified_gutter: c(p.blue),
        added_text: c(p.green),
        removed_text: c(p.red),
        filler: fg_overlay(if hc { 0.16 } else { 0.06 }),
        hunk_header_bg: accent.with_alpha(if dark { 0.10 } else { 0.07 }),
        hunk_header_fg: c(p.muted),
    };

    let git = GitColors {
        added: c(p.green),
        modified: c(p.amber),
        deleted: c(p.red),
        renamed: c(p.blue),
        untracked: c(p.green),
        conflicted: c(p.purple),
        ignored: c(p.faint),
    };

    Theme {
        name: p.name.to_string(),
        appearance: p.appearance,
        ui,
        syntax,
        diff,
        git,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CAPTURE_NAMES;

    thread_local! {
        static FAILURES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn assert_contrast(theme: &str, what: &str, fg: Color, bg: Color, min: f32) {
        let ratio = fg.contrast_ratio(bg);
        if ratio < min {
            FAILURES.with(|f| {
                f.borrow_mut().push(format!(
                    "{theme}: {what} {fg} on {bg} has contrast {ratio:.2}, need {min}"
                ))
            });
        }
    }

    fn finish() {
        let failures = FAILURES.with(|f| std::mem::take(&mut *f.borrow_mut()));
        assert!(
            failures.is_empty(),
            "contrast failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn four_builtins_with_distinct_names() {
        let themes = builtin_themes();
        let names: Vec<_> = themes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [LECTOR_DARK, LECTOR_LIGHT, LECTOR_HC_DARK, LECTOR_HC_LIGHT]
        );
    }

    #[test]
    fn every_capture_is_styled() {
        for theme in builtin_themes() {
            for name in CAPTURE_NAMES {
                assert!(
                    theme.syntax.styles.contains_key(*name),
                    "{} lacks {name}",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn text_contrast_meets_wcag() {
        for theme in builtin_themes() {
            let hc = theme.name.contains("High Contrast");
            // AA (4.5) for body text; AAA (7) for high contrast themes.
            let body = if hc { 7.0 } else { 4.5 };
            let ui = &theme.ui;
            let n = theme.name.as_str();
            for (what, bg) in [
                ("background", ui.background),
                ("surface", ui.surface),
                ("elevated", ui.elevated),
                ("tab_active_bg", ui.tab_active_bg),
                ("title_bar_bg", ui.title_bar_bg),
            ] {
                assert_contrast(n, &format!("text on {what}"), ui.text, bg, body);
                assert_contrast(n, &format!("text_muted on {what}"), ui.text_muted, bg, body);
            }
            assert_contrast(
                n,
                "tab inactive",
                ui.tab_inactive_fg,
                ui.tab_inactive_bg,
                body,
            );
            assert_contrast(n, "status bar", ui.status_bar_fg, ui.status_bar_bg, body);
            assert_contrast(n, "tooltip", ui.tooltip_fg, ui.tooltip_bg, body);
            // Line numbers / placeholders: non-essential text, 3:1 (AA large / UI).
            let faint = if hc { 4.5 } else { 3.0 };
            assert_contrast(n, "line numbers", ui.line_number, ui.background, faint);
            assert_contrast(n, "text_faint", ui.text_faint, ui.surface, faint);
            assert_contrast(
                n,
                "accent text",
                ui.text_accent,
                ui.background,
                body.min(4.5),
            );
            assert_contrast(n, "accent fg", ui.accent_foreground, ui.accent, 4.5);
            // Text stays readable inside selections and highlights.
            for (what, overlay) in [
                ("selection", ui.selection),
                ("active_line", ui.active_line),
                ("search_match_active", ui.search_match_active),
            ] {
                assert_contrast(n, what, ui.text, overlay.over(ui.background), body);
            }
        }
        finish();
    }

    #[test]
    fn syntax_contrast_meets_wcag() {
        for theme in builtin_themes() {
            let hc = theme.name.contains("High Contrast");
            let min = if hc { 7.0 } else { 4.5 };
            let bgs = [
                ("background", theme.ui.background),
                (
                    "added line",
                    theme.diff.added_line_bg.over(theme.ui.background),
                ),
                (
                    "removed line",
                    theme.diff.removed_line_bg.over(theme.ui.background),
                ),
                (
                    "active line",
                    theme.ui.active_line.over(theme.ui.background),
                ),
            ];
            for name in CAPTURE_NAMES {
                let style = theme.syntax.style_for(name).unwrap();
                for (what, bg) in bgs {
                    assert_contrast(
                        &theme.name,
                        &format!("{name} on {what}"),
                        style.color,
                        bg,
                        min,
                    );
                }
            }
        }
        finish();
    }

    #[test]
    fn diff_word_highlights_keep_text_readable() {
        for theme in builtin_themes() {
            let hc = theme.name.contains("High Contrast");
            let min = if hc { 7.0 } else { 4.5 };
            for theme in [theme.clone(), theme.with_colorblind_diff()] {
                let bg = theme.ui.background;
                for (what, word, line) in [
                    ("added", theme.diff.added_word_bg, theme.diff.added_line_bg),
                    (
                        "removed",
                        theme.diff.removed_word_bg,
                        theme.diff.removed_line_bg,
                    ),
                ] {
                    let composed = word.over(line.over(bg));
                    assert_contrast(&theme.name, what, theme.ui.text, composed, min);
                }
                // Gutter marks are UI graphics: 3:1 against the background.
                for (what, mark) in [
                    ("added gutter", theme.diff.added_gutter),
                    ("removed gutter", theme.diff.removed_gutter),
                    ("modified gutter", theme.diff.modified_gutter),
                ] {
                    assert_contrast(&theme.name, what, mark, bg, 3.0);
                }
            }
        }
        finish();
    }

    #[test]
    fn git_colors_visible_on_sidebar() {
        for theme in builtin_themes() {
            let g = &theme.git;
            for (what, color) in [
                ("added", g.added),
                ("modified", g.modified),
                ("deleted", g.deleted),
                ("renamed", g.renamed),
                ("untracked", g.untracked),
                ("conflicted", g.conflicted),
            ] {
                assert_contrast(&theme.name, what, color, theme.ui.surface, 4.5);
            }
        }
        finish();
    }
}
