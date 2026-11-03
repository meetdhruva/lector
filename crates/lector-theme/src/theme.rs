//! Theme data model.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::Color;

/// Whether a theme is meant for a light or a dark environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    /// Dark text on a light background.
    Light,
    /// Light text on a dark background.
    Dark,
}

impl Appearance {
    /// True for [`Appearance::Dark`].
    pub fn is_dark(self) -> bool {
        matches!(self, Appearance::Dark)
    }
}

/// A complete theme: UI chrome, syntax, diff and git status colors.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    /// Display name, unique within a registry.
    pub name: String,
    /// Light or dark.
    pub appearance: Appearance,
    /// UI chrome colors.
    pub ui: UiColors,
    /// Syntax highlighting styles.
    pub syntax: SyntaxTheme,
    /// Diff rendering colors.
    pub diff: DiffColors,
    /// Git status colors (file tree, change lists).
    pub git: GitColors,
}

impl Theme {
    /// A copy of this theme whose diff and git colors use a blue (added) /
    /// orange (removed) scheme that remains distinguishable for the common
    /// forms of color-vision deficiency.
    pub fn with_colorblind_diff(&self) -> Theme {
        let mut theme = self.clone();
        let dark = self.appearance.is_dark();
        let (blue, orange, purple) = if dark {
            (
                Color::hex(0x5ca4ff),
                Color::hex(0xf0883e),
                Color::hex(0xb592f5),
            )
        } else {
            (
                Color::hex(0x0b63d1),
                Color::hex(0xb35400),
                Color::hex(0x7a4fd0),
            )
        };
        let (line_a, word_a) = if dark { (0.16, 0.38) } else { (0.11, 0.26) };
        let d = &mut theme.diff;
        d.added_line_bg = blue.with_alpha(line_a);
        d.added_word_bg = blue.with_alpha(word_a);
        d.removed_line_bg = orange.with_alpha(line_a);
        d.removed_word_bg = orange.with_alpha(word_a);
        d.added_gutter = blue;
        d.removed_gutter = orange;
        d.modified_gutter = purple;
        d.added_text = blue;
        d.removed_text = orange;
        let g = &mut theme.git;
        g.added = blue;
        g.untracked = blue;
        g.deleted = orange;
        g.modified = purple;
        theme
    }

    /// Opaque editor background with a translucent overlay composited on top.
    pub fn editor_over(&self, overlay: Color) -> Color {
        overlay.over(self.ui.background)
    }
}

/// Colors for the application chrome and the code view.
///
/// Translucent colors (selection, highlights, hover) are meant to be
/// composited over whatever surface they decorate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiColors {
    /// Editor (code view) background.
    pub background: Color,
    /// Sidebar / secondary panel surfaces.
    pub surface: Color,
    /// Floating surfaces: popovers, menus, pickers, modals.
    pub elevated: Color,
    /// Bottom/side panels distinct from the sidebar.
    pub panel: Color,
    /// Default 1px borders and separators.
    pub border: Color,
    /// Subtle borders within a surface (list separators, inner dividers).
    pub border_variant: Color,
    /// Border of the focused element.
    pub border_focused: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text (labels, inactive items).
    pub text_muted: Color,
    /// Tertiary text (hints, placeholders, disabled).
    pub text_faint: Color,
    /// Accent-colored text (matches, links in chrome).
    pub text_accent: Color,
    /// Brand / focus accent.
    pub accent: Color,
    /// Text drawn on top of `accent`.
    pub accent_foreground: Color,
    /// Selection background in the focused view.
    pub selection: Color,
    /// Selection background in an unfocused view.
    pub selection_inactive: Color,
    /// Hover background for list rows, buttons.
    pub hover: Color,
    /// Pressed / selected-row background.
    pub active: Color,
    /// Current-line highlight in the code view.
    pub active_line: Color,
    /// Caret color.
    pub cursor: Color,
    /// Gutter line numbers.
    pub line_number: Color,
    /// Line number of the active line.
    pub line_number_active: Color,
    /// Indent guides.
    pub indent_guide: Color,
    /// Indent guide of the scope that contains the cursor.
    pub indent_guide_active: Color,
    /// Scrollbar thumb.
    pub scrollbar_thumb: Color,
    /// Scrollbar thumb while hovered or dragged.
    pub scrollbar_thumb_hover: Color,
    /// Scrollbar track.
    pub scrollbar_track: Color,
    /// Tab bar background.
    pub tab_bar_bg: Color,
    /// Active tab background.
    pub tab_active_bg: Color,
    /// Active tab text.
    pub tab_active_fg: Color,
    /// Inactive tab background.
    pub tab_inactive_bg: Color,
    /// Inactive tab text.
    pub tab_inactive_fg: Color,
    /// Status bar background.
    pub status_bar_bg: Color,
    /// Status bar text.
    pub status_bar_fg: Color,
    /// Title bar background.
    pub title_bar_bg: Color,
    /// Title bar text.
    pub title_bar_fg: Color,
    /// Background of search matches.
    pub search_match: Color,
    /// Background of the current search match.
    pub search_match_active: Color,
    /// Border drawn around the current search match.
    pub search_match_active_border: Color,
    /// Background of find-in-file highlights in non-focused contexts.
    pub find_highlight: Color,
    /// Background of other occurrences of the word under the cursor.
    pub word_highlight: Color,
    /// Background of the matching bracket.
    pub bracket_match: Color,
    /// Errors.
    pub error: Color,
    /// Warnings.
    pub warning: Color,
    /// Informational messages.
    pub info: Color,
    /// Hints.
    pub hint: Color,
    /// Success / positive state.
    pub success: Color,
    /// Hyperlinks.
    pub link: Color,
    /// Drop shadows of floating surfaces.
    pub shadow: Color,
    /// Modal backdrop overlay.
    pub overlay: Color,
    /// Minimap background.
    pub minimap_bg: Color,
    /// Minimap text-density blocks.
    pub minimap_text: Color,
    /// Minimap viewport slider.
    pub minimap_slider: Color,
    /// Minimap viewport slider when hovered.
    pub minimap_slider_hover: Color,
    /// Sticky header background.
    pub sticky_header_bg: Color,
    /// Sticky header bottom border.
    pub sticky_header_border: Color,
    /// Tooltip background.
    pub tooltip_bg: Color,
    /// Tooltip text.
    pub tooltip_fg: Color,
    /// Tooltip border.
    pub tooltip_border: Color,
    /// Inline annotation (blame) text at the end of lines.
    pub inline_annotation: Color,
    /// Keyboard shortcut hint badge background.
    pub keybinding_bg: Color,
    /// Input field background.
    pub input_bg: Color,
    /// Fold marker and folded-region placeholder color.
    pub fold_marker: Color,
}

/// Style of one syntax capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextStyle {
    /// Foreground color.
    pub color: Color,
    /// Bold weight.
    #[serde(default)]
    pub bold: bool,
    /// Italic slant.
    #[serde(default)]
    pub italic: bool,
}

impl TextStyle {
    /// Plain style with only a color.
    pub const fn color(color: Color) -> TextStyle {
        TextStyle {
            color,
            bold: false,
            italic: false,
        }
    }

    /// Same style, bold.
    pub const fn bold(mut self) -> TextStyle {
        self.bold = true;
        self
    }

    /// Same style, italic.
    pub const fn italic(mut self) -> TextStyle {
        self.italic = true;
        self
    }
}

/// Canonical capture names a theme may style. Lookups of other names fall
/// back by stripping dotted suffixes.
pub const CAPTURE_NAMES: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "keyword",
    "keyword.control",
    "keyword.import",
    "keyword.return",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "string.regex",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
    "variable.member",
    "markup.heading",
    "markup.bold",
    "markup.italic",
    "markup.link",
    "markup.link.url",
    "markup.raw",
    "markup.list",
    "markup.quote",
];

/// Syntax highlighting styles keyed by capture name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SyntaxTheme {
    /// Styles keyed by (possibly dotted) capture name.
    pub styles: HashMap<String, TextStyle>,
}

impl SyntaxTheme {
    /// Builds a syntax theme from `(capture, style)` pairs.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, TextStyle)>) -> Self {
        SyntaxTheme {
            styles: pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    /// Style for a capture, falling back `"a.b.c"` → `"a.b"` → `"a"`.
    pub fn style_for(&self, capture: &str) -> Option<TextStyle> {
        let mut name = capture;
        loop {
            if let Some(style) = self.styles.get(name) {
                return Some(*style);
            }
            let ix = name.rfind('.')?;
            name = &name[..ix];
        }
    }

    /// Resolves styles for a list of capture names (e.g. a highlighter's
    /// name table), so per-span lookups become an index.
    pub fn resolve_all(&self, names: &[&str]) -> Vec<Option<TextStyle>> {
        names.iter().map(|n| self.style_for(n)).collect()
    }
}

/// Colors used to render diffs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffColors {
    /// Background of added lines (translucent).
    pub added_line_bg: Color,
    /// Background of removed lines (translucent).
    pub removed_line_bg: Color,
    /// Background of changed words inside added lines.
    pub added_word_bg: Color,
    /// Background of changed words inside removed lines.
    pub removed_word_bg: Color,
    /// Gutter bar / sign for added lines.
    pub added_gutter: Color,
    /// Gutter mark for deleted lines.
    pub removed_gutter: Color,
    /// Gutter bar for modified lines.
    pub modified_gutter: Color,
    /// "+" counts and added-line sign text.
    pub added_text: Color,
    /// "-" counts and removed-line sign text.
    pub removed_text: Color,
    /// Diagonal hatch color for filler rows (the empty side of a split diff).
    pub filler: Color,
    /// Hunk header background.
    pub hunk_header_bg: Color,
    /// Hunk header text.
    pub hunk_header_fg: Color,
}

/// Git status colors.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GitColors {
    /// Added / staged-new files.
    pub added: Color,
    /// Modified files.
    pub modified: Color,
    /// Deleted files.
    pub deleted: Color,
    /// Renamed files.
    pub renamed: Color,
    /// Untracked files.
    pub untracked: Color,
    /// Files with merge conflicts.
    pub conflicted: Color,
    /// Ignored files.
    pub ignored: Color,
}
