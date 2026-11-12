//! Monochrome UI icons (Lucide, ISC license).
//!
//! The SVGs use `stroke="currentColor"`; the UI tints them with the text
//! color. `scripts/gen_icons.py` copies every file listed here from
//! lucide-static into `assets/lucide/`.

macro_rules! ui_icons {
    ($($(#[$doc:meta])* $variant:ident => $file:literal,)*) => {
        /// A monochrome UI icon.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum UiIcon {
            $($(#[$doc])* $variant,)*
        }

        impl UiIcon {
            /// Every UI icon.
            pub const ALL: &'static [UiIcon] = &[$(UiIcon::$variant,)*];

            /// Stable kebab-case name (the Lucide file stem), e.g. `"chevron-right"`.
            pub const fn name(self) -> &'static str {
                match self {
                    $(UiIcon::$variant => $file,)*
                }
            }

            /// SVG bytes.
            pub const fn svg(self) -> &'static [u8] {
                match self {
                    $(UiIcon::$variant => include_bytes!(concat!(
                        env!("CARGO_MANIFEST_DIR"), "/assets/lucide/", $file, ".svg"
                    )),)*
                }
            }

            /// Icon by its kebab-case name.
            pub fn from_name(name: &str) -> Option<UiIcon> {
                match name {
                    $($file => Some(UiIcon::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

ui_icons! {
    /// `>` disclosure chevron.
    ChevronRight => "chevron-right",
    /// `v` disclosure chevron.
    ChevronDown => "chevron-down",
    /// `^` chevron.
    ChevronUp => "chevron-up",
    /// `<` chevron.
    ChevronLeft => "chevron-left",
    /// Expand/collapse vertical toggle.
    ChevronsUpDown => "chevrons-up-down",
    /// Collapse all.
    ChevronsDownUp => "chevrons-down-up",
    /// Close / dismiss.
    Close => "x",
    /// Search / find.
    Search => "search",
    /// Text search across files.
    TextSearch => "text-search",
    /// File explorer.
    Files => "files",
    /// A single file.
    File => "file",
    /// File with text lines.
    FileText => "file-text",
    /// Source file.
    FileCode => "file-code",
    /// File diff.
    FileDiff => "file-diff",
    /// Closed folder.
    Folder => "folder",
    /// Open folder.
    FolderOpen => "folder-open",
    /// Git branch.
    GitBranch => "git-branch",
    /// Git commit.
    GitCommit => "git-commit-horizontal",
    /// Compare / changes.
    GitCompare => "git-compare",
    /// Merge.
    GitMerge => "git-merge",
    /// Pull request / review.
    GitPullRequest => "git-pull-request",
    /// Fork.
    GitFork => "git-fork",
    /// History / log.
    History => "history",
    /// Outline (symbol tree).
    ListTree => "list-tree",
    /// Flat list.
    List => "list",
    /// Settings.
    Settings => "settings",
    /// Light appearance.
    Sun => "sun",
    /// Dark appearance.
    Moon => "moon",
    /// Split (side-by-side) layout.
    Columns => "columns-2",
    /// Stacked layout.
    Rows => "rows-2",
    /// Unified diff layout.
    AlignJustify => "align-justify",
    /// Visible.
    Eye => "eye",
    /// Hidden.
    EyeOff => "eye-off",
    /// Check mark.
    Check => "check",
    /// Checked checkbox.
    CheckSquare => "square-check",
    /// Unchecked checkbox.
    Square => "square",
    /// Copy.
    Copy => "copy",
    /// Link.
    Link => "link",
    /// External link.
    ExternalLink => "external-link",
    /// Left arrow (back).
    ArrowLeft => "arrow-left",
    /// Right arrow (forward).
    ArrowRight => "arrow-right",
    /// Up arrow.
    ArrowUp => "arrow-up",
    /// Down arrow.
    ArrowDown => "arrow-down",
    /// Fold.
    Fold => "fold-vertical",
    /// Unfold.
    Unfold => "unfold-vertical",
    /// Soft wrap.
    WrapText => "wrap-text",
    /// Minimap.
    Map => "map",
    /// Typography / language.
    Type => "type",
    /// Regex search option.
    Regex => "regex",
    /// Case-sensitive search option.
    CaseSensitive => "case-sensitive",
    /// Whole-word search option.
    WholeWord => "whole-word",
    /// Filter.
    Filter => "filter",
    /// Refresh.
    Refresh => "refresh-cw",
    /// Tag.
    Tag => "tag",
    /// Stash.
    Archive => "archive",
    /// Person / author.
    User => "user",
    /// Time.
    Clock => "clock",
    /// Add.
    Plus => "plus",
    /// Remove.
    Minus => "minus",
    /// Modified dot.
    CircleDot => "circle-dot",
    /// Filled-able circle.
    Circle => "circle",
    /// Success.
    CircleCheck => "circle-check",
    /// Error.
    CircleX => "circle-x",
    /// Warning.
    AlertTriangle => "triangle-alert",
    /// Info.
    Info => "info",
    /// Overflow menu.
    MoreHorizontal => "ellipsis",
    /// Hamburger menu.
    Menu => "menu",
    /// Toggle left panel.
    PanelLeft => "panel-left",
    /// Command palette.
    Command => "command",
    /// Hash / line number.
    Hash => "hash",
    /// Symbols.
    Braces => "braces",
    /// Code.
    Code => "code",
    /// Layers.
    Layers => "layers",
    /// Pin.
    Pin => "pin",
    /// Markdown preview / docs.
    BookOpen => "book-open",
    /// Binary file.
    Binary => "binary",
    /// Image file.
    Image => "image",
    /// Spinner.
    Loader => "loader-circle",
    /// Monitor / system appearance.
    Monitor => "monitor",
    /// Replace.
    Replace => "replace",
    /// Diff.
    Diff => "diff",
    /// Bot / agent-authored.
    Bot => "bot",
    /// Sparkles.
    Sparkles => "sparkles",
    /// Collapse list.
    ListCollapse => "list-collapse",
    /// Scroll / log text.
    ScrollText => "scroll-text",
    /// Globe / remote.
    Globe => "globe",
    /// Light bulb / hint.
    Lightbulb => "lightbulb",
    /// Locked (read-only).
    Lock => "lock",
    /// Unlocked (editable).
    LockOpen => "lock-open",
    /// Save.
    Save => "save",
    /// Keyboard (key bindings).
    Keyboard => "keyboard",
    /// Bug (report an issue).
    Bug => "bug",
    /// Open a folder.
    FolderPlus => "folder-plus",
    /// Close a folder.
    FolderX => "folder-x",
    /// Palette (themes).
    Palette => "palette",
    /// Enter full screen.
    Maximize => "maximize-2",
    /// Leave full screen.
    Minimize => "minimize-2",
    /// A window (new window).
    AppWindow => "app-window",
    /// Window caption: minimize (Lector's own 1px glyph, `assets/lucide/lector-*.svg`).
    WindowMinimize => "lector-win-minimize",
    /// Window caption: maximize.
    WindowMaximize => "lector-win-maximize",
    /// Window caption: restore.
    WindowRestore => "lector-win-restore",
    /// Window caption: close.
    WindowClose => "lector-win-close",
    /// Discard / revert changes.
    Undo => "undo-2",
    /// Delete.
    Trash => "trash-2",
    /// The integrated terminal.
    Terminal => "square-terminal",
}

/// SVG bytes of a UI icon.
pub fn ui_icon(icon: UiIcon) -> &'static [u8] {
    icon.svg()
}
