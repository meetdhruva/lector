//! Markdown: parsing (pulldown-cmark, off the main thread) into a small
//! block model with syntax-highlighted code, and rendering to GPUI elements.
//! Used by the Markdown preview and by hover popovers.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    div, img, prelude::*, px, AnyElement, App, FontStyle, FontWeight, InteractiveText, ObjectFit,
    SharedString, StrikethroughStyle, StyledText, TextRun, UnderlineStyle, Window,
};
use lector_syntax::{LanguageRegistry, Syntax};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::code_view::EditorSettings;
use crate::components::{h_flex, v_flex};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Inline style flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Inline code.
    pub code: bool,
    /// Strikethrough.
    pub strike: bool,
    /// Part of a link.
    pub link: bool,
}

/// Styled inline text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inline {
    /// Plain text.
    pub text: String,
    /// Style runs (byte ranges into `text`, non-overlapping, sorted).
    pub runs: Vec<(Range<usize>, InlineStyle)>,
    /// Links (byte ranges into `text`, target).
    pub links: Vec<(Range<usize>, String)>,
    /// Inline images (src, alt).
    pub images: Vec<(String, String)>,
}

/// Column alignment of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    /// Left / none.
    Left,
    /// Centered.
    Center,
    /// Right.
    Right,
}

/// A list item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListItem {
    /// Task list state.
    pub checked: Option<bool>,
    /// Content.
    pub blocks: Vec<Block>,
}

/// A block element. `line` is the 0-based source line where it starts.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// `#`..`######`.
    Heading {
        /// 1..=6.
        level: u8,
        /// Text.
        text: Inline,
        /// Source line.
        line: u32,
    },
    /// Paragraph.
    Paragraph {
        /// Text.
        text: Inline,
        /// Source line.
        line: u32,
    },
    /// Fenced or indented code, with highlight spans (byte range, capture).
    Code {
        /// Info string language.
        lang: Option<String>,
        /// Code.
        code: String,
        /// Highlights.
        spans: Vec<(Range<usize>, &'static str)>,
        /// Source line.
        line: u32,
    },
    /// Bulleted (`start == None`) or numbered list.
    List {
        /// First number.
        start: Option<u64>,
        /// Items.
        items: Vec<ListItem>,
        /// Source line.
        line: u32,
    },
    /// Block quote.
    Quote {
        /// Content.
        blocks: Vec<Block>,
        /// Source line.
        line: u32,
    },
    /// Table.
    Table {
        /// Column alignment.
        aligns: Vec<Align>,
        /// Header cells.
        header: Vec<Inline>,
        /// Body rows.
        rows: Vec<Vec<Inline>>,
        /// Source line.
        line: u32,
    },
    /// Horizontal rule.
    Rule {
        /// Source line.
        line: u32,
    },
    /// Standalone image.
    Image {
        /// Source (URL or path).
        src: String,
        /// Alt text.
        alt: String,
        /// Source line.
        line: u32,
    },
}

impl Block {
    /// Source line of the block.
    pub fn line(&self) -> u32 {
        match self {
            Block::Heading { line, .. }
            | Block::Paragraph { line, .. }
            | Block::Code { line, .. }
            | Block::List { line, .. }
            | Block::Quote { line, .. }
            | Block::Table { line, .. }
            | Block::Rule { line }
            | Block::Image { line, .. } => *line,
        }
    }
}

enum Container {
    Root(Vec<Block>),
    Quote(Vec<Block>, u32),
    List(Option<u64>, Vec<ListItem>, u32),
    Item(ListItem),
    Table {
        aligns: Vec<Align>,
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
        row: Vec<Inline>,
        line: u32,
    },
}

#[derive(Default)]
struct InlineBuilder {
    inline: Inline,
    style: InlineStyle,
    bold: u32,
    italic: u32,
    strike: u32,
    links: Vec<(usize, String)>,
    image: Option<(String, String)>,
    line: u32,
}

impl InlineBuilder {
    fn push(&mut self, text: &str, code: bool) {
        if let Some((_, alt)) = &mut self.image {
            alt.push_str(text);
            return;
        }
        let start = self.inline.text.len();
        self.inline.text.push_str(text);
        let style = InlineStyle {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            link: !self.links.is_empty(),
            code,
        };
        let end = self.inline.text.len();
        match self.inline.runs.last_mut() {
            Some((r, s)) if *s == style && r.end == start => r.end = end,
            _ => self.inline.runs.push((start..end, style)),
        }
        let _ = &self.style;
    }
}

struct Builder<'a> {
    source: &'a str,
    line_starts: Vec<usize>,
    stack: Vec<Container>,
    inline: Option<InlineBuilder>,
    heading: Option<u8>,
    code: Option<(Option<String>, String, u32)>,
    html: Option<(String, u32)>,
    registry: LanguageRegistry,
}

impl Builder<'_> {
    fn line_of(&self, offset: usize) -> u32 {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i as u32,
            Err(i) => i.saturating_sub(1) as u32,
        }
    }

    fn push_block(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Container::Root(b)) | Some(Container::Quote(b, _)) => b.push(block),
            Some(Container::Item(item)) => item.blocks.push(block),
            Some(Container::List(_, items, _)) => items.push(ListItem {
                checked: None,
                blocks: vec![block],
            }),
            Some(Container::Table { .. }) | None => {}
        }
    }

    fn begin_inline(&mut self, offset: usize) {
        if self.inline.is_none() {
            self.inline = Some(InlineBuilder {
                line: self.line_of(offset),
                ..Default::default()
            });
        }
    }

    fn flush_paragraph(&mut self) {
        let Some(b) = self.inline.take() else { return };
        let line = b.line;
        let inline = b.inline;
        let only_images = inline.text.trim().is_empty() && !inline.images.is_empty();
        if only_images {
            for (src, alt) in inline.images {
                self.push_block(Block::Image { src, alt, line });
            }
        } else if !inline.text.trim().is_empty() {
            let images = inline.images.clone();
            self.push_block(Block::Paragraph { text: inline, line });
            for (src, alt) in images {
                self.push_block(Block::Image { src, alt, line });
            }
        }
    }

    fn highlight_code(&self, lang: Option<&str>, code: &str) -> Vec<(Range<usize>, &'static str)> {
        let Some(lang) = lang.and_then(|l| {
            let name = l.split([',', ' ', '{']).next().unwrap_or(l).trim();
            self.registry.by_name(name)
        }) else {
            return Vec::new();
        };
        if code.len() > 512 * 1024 {
            return Vec::new();
        }
        match Syntax::parse(&lang, code) {
            Ok(syntax) => syntax
                .highlights(code, 0..code.len())
                .into_iter()
                .map(|s| (s.range, s.id.name()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, code, _)) = &mut self.code {
                    code.push_str(&text);
                } else if let Some((html, _)) = &mut self.html {
                    html.push_str(&text);
                } else {
                    self.begin_inline(range.start);
                    if let Some(b) = &mut self.inline {
                        b.push(&text, false);
                    }
                }
            }
            Event::Code(text) => {
                self.begin_inline(range.start);
                if let Some(b) = &mut self.inline {
                    b.push(&text, true);
                }
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.begin_inline(range.start);
                if let Some(b) = &mut self.inline {
                    b.push(&text, true);
                }
            }
            Event::SoftBreak => {
                if let Some(b) = &mut self.inline {
                    b.push(" ", false);
                }
            }
            Event::HardBreak => {
                if let Some(b) = &mut self.inline {
                    b.push("\n", false);
                }
            }
            Event::Html(text) => {
                if let Some((html, _)) = &mut self.html {
                    html.push_str(&text);
                }
            }
            Event::InlineHtml(text) => {
                if let Some(src) = html_img_src(&text) {
                    self.begin_inline(range.start);
                    if let Some(b) = &mut self.inline {
                        b.inline.images.push((src, String::new()));
                    }
                } else if text.trim_start().starts_with("<br") {
                    if let Some(b) = &mut self.inline {
                        b.push("\n", false);
                    }
                }
            }
            Event::Rule => {
                let line = self.line_of(range.start);
                self.flush_paragraph();
                self.push_block(Block::Rule { line });
            }
            Event::TaskListMarker(checked) => {
                if let Some(Container::Item(item)) = self.stack.last_mut() {
                    item.checked = Some(checked);
                }
            }
            Event::FootnoteReference(label) => {
                self.begin_inline(range.start);
                if let Some(b) = &mut self.inline {
                    b.push(&format!("[{label}]"), false);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        let line = self.line_of(range.start);
        match tag {
            Tag::Paragraph => {
                self.flush_paragraph();
                self.begin_inline(range.start);
            }
            Tag::Heading { level, .. } => {
                self.flush_paragraph();
                self.heading = Some(level as u8);
                self.begin_inline(range.start);
            }
            Tag::BlockQuote(_) => {
                self.flush_paragraph();
                self.stack.push(Container::Quote(Vec::new(), line));
            }
            Tag::CodeBlock(kind) => {
                self.flush_paragraph();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) if !info.trim().is_empty() => {
                        Some(info.trim().to_string())
                    }
                    _ => None,
                };
                self.code = Some((lang, String::new(), line));
            }
            Tag::HtmlBlock => {
                self.flush_paragraph();
                self.html = Some((String::new(), line));
            }
            Tag::List(start) => {
                self.flush_paragraph();
                self.stack.push(Container::List(start, Vec::new(), line));
            }
            Tag::Item => {
                self.flush_paragraph();
                self.stack.push(Container::Item(ListItem::default()));
            }
            Tag::Table(aligns) => {
                self.flush_paragraph();
                self.stack.push(Container::Table {
                    aligns: aligns
                        .iter()
                        .map(|a| match a {
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                            _ => Align::Left,
                        })
                        .collect(),
                    header: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    line,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(Container::Table { row, .. }) = self.stack.last_mut() {
                    row.clear();
                }
            }
            Tag::TableCell => {
                self.inline = Some(InlineBuilder {
                    line,
                    ..Default::default()
                });
            }
            Tag::Emphasis => {
                self.begin_inline(range.start);
                if let Some(b) = &mut self.inline {
                    b.italic += 1;
                }
            }
            Tag::Strong => {
                self.begin_inline(range.start);
                if let Some(b) = &mut self.inline {
                    b.bold += 1;
                }
            }
            Tag::Strikethrough => {
                self.begin_inline(range.start);
                if let Some(b) = &mut self.inline {
                    b.strike += 1;
                }
            }
            Tag::Link { dest_url, .. } => {
                self.begin_inline(range.start);
                if let Some(b) = &mut self.inline {
                    let start = b.inline.text.len();
                    b.links.push((start, dest_url.to_string()));
                }
            }
            Tag::Image { dest_url, .. } => {
                self.begin_inline(range.start);
                if let Some(b) = &mut self.inline {
                    b.image = Some((dest_url.to_string(), String::new()));
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_paragraph(),
            TagEnd::Heading(_) => {
                if let Some(b) = self.inline.take() {
                    let level = self.heading.take().unwrap_or(1);
                    self.push_block(Block::Heading {
                        level,
                        text: b.inline,
                        line: b.line,
                    });
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut code, line)) = self.code.take() {
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    let spans = self.highlight_code(lang.as_deref(), &code);
                    self.push_block(Block::Code {
                        lang,
                        code,
                        spans,
                        line,
                    });
                }
            }
            TagEnd::HtmlBlock => {
                if let Some((html, line)) = self.html.take() {
                    let mut rest = html.as_str();
                    while let Some(ix) = rest.find("<img") {
                        rest = &rest[ix + 4..];
                        if let Some(src) = html_img_src(rest) {
                            self.push_block(Block::Image {
                                src,
                                alt: String::new(),
                                line,
                            });
                        }
                    }
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                if let Some(Container::Quote(blocks, line)) = self.stack.pop() {
                    self.push_block(Block::Quote { blocks, line });
                }
            }
            TagEnd::Item => {
                self.flush_paragraph();
                if let Some(Container::Item(item)) = self.stack.pop() {
                    if let Some(Container::List(_, items, _)) = self.stack.last_mut() {
                        items.push(item);
                    }
                }
            }
            TagEnd::List(_) => {
                self.flush_paragraph();
                if let Some(Container::List(start, items, line)) = self.stack.pop() {
                    self.push_block(Block::List { start, items, line });
                }
            }
            TagEnd::TableCell => {
                let inline = self.inline.take().map(|b| b.inline).unwrap_or_default();
                if let Some(Container::Table { row, .. }) = self.stack.last_mut() {
                    row.push(inline);
                }
            }
            TagEnd::TableHead => {
                if let Some(Container::Table { row, header, .. }) = self.stack.last_mut() {
                    *header = std::mem::take(row);
                }
            }
            TagEnd::TableRow => {
                if let Some(Container::Table { row, rows, .. }) = self.stack.last_mut() {
                    rows.push(std::mem::take(row));
                }
            }
            TagEnd::Table => {
                if let Some(Container::Table {
                    aligns,
                    header,
                    rows,
                    line,
                    ..
                }) = self.stack.pop()
                {
                    self.push_block(Block::Table {
                        aligns,
                        header,
                        rows,
                        line,
                    });
                }
            }
            TagEnd::Emphasis => {
                if let Some(b) = &mut self.inline {
                    b.italic = b.italic.saturating_sub(1);
                }
            }
            TagEnd::Strong => {
                if let Some(b) = &mut self.inline {
                    b.bold = b.bold.saturating_sub(1);
                }
            }
            TagEnd::Strikethrough => {
                if let Some(b) = &mut self.inline {
                    b.strike = b.strike.saturating_sub(1);
                }
            }
            TagEnd::Link => {
                if let Some(b) = &mut self.inline {
                    if let Some((start, url)) = b.links.pop() {
                        let end = b.inline.text.len();
                        if end > start {
                            b.inline.links.push((start..end, url));
                        }
                    }
                }
            }
            TagEnd::Image => {
                if let Some(b) = &mut self.inline {
                    if let Some(image) = b.image.take() {
                        b.inline.images.push(image);
                    }
                }
            }
            _ => {}
        }
    }
}

fn html_img_src(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let ix = lower.find("src=")?;
    let rest = &html[ix + 4..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let rest = &rest[1..];
        let end = rest.find(quote)?;
        Some(rest[..end].to_string())
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// Parses Markdown into blocks (blocking; run on a background thread for
/// large documents). Code blocks are highlighted with `lector-syntax`.
pub fn parse(source: &str) -> Vec<Block> {
    let mut line_starts = vec![0];
    line_starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
    let mut builder = Builder {
        source,
        line_starts,
        stack: vec![Container::Root(Vec::new())],
        inline: None,
        heading: None,
        code: None,
        html: None,
        registry: LanguageRegistry::new(),
    };
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        builder.event(event, range);
    }
    builder.flush_paragraph();
    let _ = builder.source;
    // Close anything left open (malformed input never panics).
    while builder.stack.len() > 1 {
        match builder.stack.pop() {
            Some(Container::Quote(blocks, line)) => {
                builder.push_block(Block::Quote { blocks, line })
            }
            Some(Container::List(start, items, line)) => {
                builder.push_block(Block::List { start, items, line })
            }
            Some(Container::Item(item)) => {
                if let Some(Container::List(_, items, _)) = builder.stack.last_mut() {
                    items.push(item);
                }
            }
            _ => {}
        }
    }
    match builder.stack.pop() {
        Some(Container::Root(blocks)) => blocks,
        _ => Vec::new(),
    }
}

/// What to do when a link is clicked: `(target, window, cx)`.
pub type LinkHandler = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// Rendering options.
#[derive(Clone)]
pub struct RenderOptions {
    /// Directory relative links and images resolve against.
    pub base_dir: Option<PathBuf>,
    /// Link click handler.
    pub on_link: Option<LinkHandler>,
    /// Smaller spacing and headings (hover popovers).
    pub compact: bool,
    /// Element id prefix (unique per view).
    pub id: SharedString,
}

/// Resolves a link target: URLs pass through, relative paths are joined to
/// `base_dir` (fragment and query removed). Returns `(target, is_url)`.
pub fn resolve_link(target: &str, base_dir: Option<&Path>) -> (String, bool) {
    if target.contains("://") || target.starts_with("mailto:") {
        return (target.to_string(), true);
    }
    let path = target.split(['#', '?']).next().unwrap_or(target);
    match base_dir {
        Some(base) if !path.is_empty() => {
            // Markdown links use '/'; join per segment for native separators.
            let joined = path
                .split('/')
                .filter(|segment| !segment.is_empty())
                .fold(base.to_path_buf(), |acc, segment| acc.join(segment));
            (joined.display().to_string(), false)
        }
        _ => (path.to_string(), false),
    }
}

struct Ctx<'a> {
    opts: &'a RenderOptions,
    next_id: usize,
}

impl Ctx<'_> {
    fn id(&mut self) -> SharedString {
        self.next_id += 1;
        format!("{}-{}", self.opts.id, self.next_id).into()
    }
}

/// Renders `blocks` to elements (one per top-level block).
pub fn render_blocks(
    blocks: &[Block],
    opts: &RenderOptions,
    _window: &mut Window,
    cx: &mut App,
) -> Vec<AnyElement> {
    let mut ctx = Ctx { opts, next_id: 0 };
    blocks
        .iter()
        .map(|b| render_block(b, &mut ctx, cx))
        .collect()
}

fn ui_font(cx: &App) -> gpui::Font {
    gpui::font(crate::settings::UiFont::get(cx).family)
}

fn inline_element(inline: &Inline, ctx: &mut Ctx, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let ui = &theme.ui;
    let base = ui_font(cx);
    let mono = EditorSettings::get(cx).font();
    let text_color = ui.text.hsla();
    let mut runs = Vec::new();
    let mut pos = 0;
    let push = |len: usize, style: InlineStyle, runs: &mut Vec<TextRun>| {
        if len == 0 {
            return;
        }
        let mut font = if style.code {
            mono.clone()
        } else {
            base.clone()
        };
        if style.bold {
            font.weight = FontWeight::BOLD;
        }
        if style.italic {
            font.style = FontStyle::Italic;
        }
        runs.push(TextRun {
            len,
            font,
            color: if style.link {
                ui.text_accent.hsla()
            } else if style.code {
                theme
                    .syntax
                    .style_for("string")
                    .map(|s| s.color.hsla())
                    .unwrap_or(text_color)
            } else {
                text_color
            },
            background_color: style.code.then(|| ui.hover.hsla()),
            underline: style.link.then(|| UnderlineStyle {
                thickness: px(1.),
                color: Some(ui.text_accent.hsla().opacity(0.5)),
                wavy: false,
            }),
            strikethrough: style.strike.then(|| StrikethroughStyle {
                thickness: px(1.),
                color: Some(text_color),
            }),
        });
    };
    for (range, style) in &inline.runs {
        if range.start > pos {
            push(range.start - pos, InlineStyle::default(), &mut runs);
        }
        push(range.end - range.start, *style, &mut runs);
        pos = range.end;
    }
    if inline.text.len() > pos {
        push(inline.text.len() - pos, InlineStyle::default(), &mut runs);
    }
    let styled = StyledText::new(SharedString::from(inline.text.clone())).with_runs(runs);
    if inline.links.is_empty() {
        return styled.into_any_element();
    }
    let ranges: Vec<Range<usize>> = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let targets: Vec<String> = inline.links.iter().map(|(_, t)| t.clone()).collect();
    let on_link = ctx.opts.on_link.clone();
    InteractiveText::new(ctx.id(), styled)
        .on_click(ranges, move |ix, window, cx| {
            if let (Some(handler), Some(target)) = (&on_link, targets.get(ix)) {
                handler(target, window, cx);
            }
        })
        .into_any_element()
}

fn render_block(block: &Block, ctx: &mut Ctx, cx: &mut App) -> AnyElement {
    let theme = cx.theme().clone();
    let ui = &theme.ui;
    let compact = ctx.opts.compact;
    let gap = if compact { px(6.) } else { px(12.) };
    match block {
        Block::Heading { level, text, .. } => {
            let size = match (compact, level) {
                (true, _) => 14.,
                (_, 1) => 26.,
                (_, 2) => 21.,
                (_, 3) => 17.,
                (_, 4) => 15.,
                _ => 14.,
            };
            div()
                .mt(if compact { px(2.) } else { px(10.) })
                .mb(gap)
                .text_size(px(size))
                .line_height(px(size * 1.35))
                .font_weight(FontWeight::SEMIBOLD)
                .when(*level <= 2 && !compact, |el| {
                    el.pb(px(6.))
                        .border_b_1()
                        .border_color(ui.border_variant.hsla())
                })
                .child(inline_element(text, ctx, cx))
                .into_any_element()
        }
        Block::Paragraph { text, .. } => div()
            .mb(gap)
            .line_height(rems_line(compact))
            .child(inline_element(text, ctx, cx))
            .into_any_element(),
        Block::Code {
            lang, code, spans, ..
        } => {
            let settings = EditorSettings::get(cx);
            let mono = settings.font();
            let default = ui.text.hsla();
            let mut runs = Vec::new();
            let mut pos = 0;
            for (range, capture) in spans {
                if range.start < pos || range.end > code.len() {
                    continue;
                }
                if range.start > pos {
                    runs.push(text_run(range.start - pos, &mono, default, false, false));
                }
                let style = theme.syntax.style_for(capture);
                runs.push(text_run(
                    range.end - range.start,
                    &mono,
                    style.map(|s| s.color.hsla()).unwrap_or(default),
                    style.is_some_and(|s| s.bold),
                    style.is_some_and(|s| s.italic),
                ));
                pos = range.end;
            }
            if code.len() > pos {
                runs.push(text_run(code.len() - pos, &mono, default, false, false));
            }
            let code_text: SharedString = if code.is_empty() {
                " ".into()
            } else {
                code.clone().into()
            };
            if code.is_empty() {
                runs = vec![text_run(1, &mono, default, false, false)];
            }
            let font_size = f32::from(settings.font_size) - 0.5;
            div()
                .relative()
                .mb(gap)
                .rounded(px(6.))
                .bg(ui.surface.hsla())
                .border_1()
                .border_color(ui.border_variant.hsla())
                .child(
                    div()
                        .id(ctx.id())
                        .overflow_x_scroll()
                        .px(px(12.))
                        .py(px(10.))
                        .text_size(px(font_size))
                        .line_height(px((font_size * 1.5).round()))
                        .whitespace_nowrap()
                        .child(StyledText::new(code_text).with_runs(runs)),
                )
                .when_some(lang.clone().filter(|_| !compact), |el, lang| {
                    el.child(
                        div()
                            .absolute()
                            .top(px(6.))
                            .right(px(10.))
                            .text_size(px(10.5))
                            .text_color(ui.text_faint.hsla())
                            .child(lang),
                    )
                })
                .into_any_element()
        }
        Block::List { start, items, .. } => {
            let mut list = v_flex().mb(gap).gap(px(3.));
            for (i, item) in items.iter().enumerate() {
                let marker: AnyElement = match (item.checked, start) {
                    (Some(checked), _) => Icon::new(if checked {
                        IconName::CheckSquare
                    } else {
                        IconName::Square
                    })
                    .size(IconSize::Small)
                    .color(if checked {
                        ui.text_accent.hsla()
                    } else {
                        ui.text_faint.hsla()
                    })
                    .into_any_element(),
                    (None, Some(n)) => div()
                        .text_color(ui.text_muted.hsla())
                        .child(format!("{}.", n + i as u64))
                        .into_any_element(),
                    (None, None) => div()
                        .mt(px(8.))
                        .size(px(5.))
                        .rounded_full()
                        .bg(ui.text_muted.hsla())
                        .into_any_element(),
                };
                let content: Vec<AnyElement> = item
                    .blocks
                    .iter()
                    .map(|b| render_block(b, ctx, cx))
                    .collect();
                list = list.child(
                    h_flex()
                        .items_start()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(18.))
                                .flex()
                                .justify_end()
                                .child(marker),
                        )
                        .child(v_flex().flex_1().min_w_0().children(content)),
                );
            }
            list.into_any_element()
        }
        Block::Quote { blocks, .. } => {
            let content: Vec<AnyElement> =
                blocks.iter().map(|b| render_block(b, ctx, cx)).collect();
            div()
                .mb(gap)
                .pl(px(14.))
                .border_l_4()
                .border_color(ui.border.hsla())
                .text_color(ui.text_muted.hsla())
                .children(content)
                .into_any_element()
        }
        Block::Table {
            aligns,
            header,
            rows,
            ..
        } => {
            let row_el = |cells: &[Inline], head: bool, ctx: &mut Ctx, cx: &App| {
                let mut row = h_flex()
                    .items_start()
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .when(head, |el| {
                        el.bg(ui.surface.hsla()).font_weight(FontWeight::SEMIBOLD)
                    });
                for (i, cell) in cells.iter().enumerate() {
                    let align = aligns.get(i).copied().unwrap_or(Align::Left);
                    row = row.child(
                        div()
                            .flex_1()
                            .min_w(px(60.))
                            .px(px(10.))
                            .py(px(5.))
                            .when(i > 0, |el| {
                                el.border_l_1().border_color(ui.border_variant.hsla())
                            })
                            .flex()
                            .when(align == Align::Center, |el| el.justify_center())
                            .when(align == Align::Right, |el| el.justify_end())
                            .child(inline_element(cell, ctx, cx)),
                    );
                }
                row
            };
            let mut table = v_flex()
                .mb(gap)
                .border_1()
                .border_color(ui.border_variant.hsla())
                .rounded(px(6.))
                .overflow_hidden()
                .child(row_el(header, true, ctx, cx));
            for r in rows {
                table = table.child(row_el(r, false, ctx, cx));
            }
            table.into_any_element()
        }
        Block::Rule { .. } => div()
            .my(gap)
            .h(px(1.))
            .bg(ui.border.hsla())
            .into_any_element(),
        Block::Image { src, alt, .. } => {
            let (target, is_url) = resolve_link(src, ctx.opts.base_dir.as_deref());
            if is_url || !Path::new(&target).exists() {
                // Remote images are not fetched (no network access by design).
                let label = if alt.is_empty() {
                    src.clone()
                } else {
                    alt.clone()
                };
                return h_flex()
                    .mb(gap)
                    .gap(px(6.))
                    .text_color(ui.text_faint.hsla())
                    .child(Icon::new(IconName::Image).size(IconSize::Small))
                    .child(label)
                    .into_any_element();
            }
            div()
                .mb(gap)
                .child(
                    img(PathBuf::from(target))
                        .max_w(px(900.))
                        .max_h(px(600.))
                        .object_fit(ObjectFit::ScaleDown),
                )
                .into_any_element()
        }
    }
}

fn rems_line(compact: bool) -> gpui::Pixels {
    if compact {
        px(19.)
    } else {
        px(22.)
    }
}

fn text_run(len: usize, font: &gpui::Font, color: gpui::Hsla, bold: bool, italic: bool) -> TextRun {
    let mut font = font.clone();
    if bold {
        font.weight = FontWeight::BOLD;
    }
    if italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_blocks() {
        let md = "# Title\n\nSome *em* and **strong** and `code` [link](a.md).\n\n- one\n- [x] two\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n> quote\n\n---\n\n![alt](img.png)\n";
        let blocks = parse(md);
        assert!(
            matches!(&blocks[0], Block::Heading { level: 1, text, .. } if text.text == "Title")
        );
        match &blocks[1] {
            Block::Paragraph { text, line } => {
                assert_eq!(*line, 2);
                assert_eq!(text.text, "Some em and strong and code link.");
                assert_eq!(text.links.len(), 1);
                assert!(text.runs.iter().any(|(_, s)| s.code));
            }
            other => panic!("{other:?}"),
        }
        match &blocks[2] {
            Block::List {
                items, start: None, ..
            } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[1].checked, Some(true));
            }
            other => panic!("{other:?}"),
        }
        match &blocks[3] {
            Block::Code { lang, spans, .. } => {
                assert_eq!(lang.as_deref(), Some("rust"));
                assert!(spans.iter().any(|(_, c)| c.starts_with("keyword")));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&blocks[4], Block::Table { rows, header, .. } if rows.len() == 1 && header.len() == 2)
        );
        assert!(matches!(&blocks[5], Block::Quote { .. }));
        assert!(matches!(&blocks[6], Block::Rule { .. }));
        assert!(matches!(&blocks[7], Block::Image { src, .. } if src == "img.png"));
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for md in [
            "",
            "```",
            "- a\n  - b\n    ```x",
            "| a |\n|",
            "<img src=x>",
            "[x](",
            "\u{0}",
        ] {
            let _ = parse(md);
        }
    }

    #[test]
    fn links_resolve() {
        assert_eq!(
            resolve_link("https://x.y", None),
            ("https://x.y".into(), true)
        );
        let (p, url) = resolve_link("docs/a.md#h", Some(Path::new("/r")));
        assert!(!url);
        assert_eq!(
            p,
            Path::new("/r")
                .join("docs")
                .join("a.md")
                .display()
                .to_string()
        );
    }
}
