//! Owner-keyed overlay layers on a [`CodeView`]: full-width row backgrounds
//! (e.g. merge-conflict regions) and small interactive widgets anchored to a
//! row (e.g. "Accept Current | Accept Incoming" above a conflict, inline
//! hints). Several owners (git, LSP, ...) can each set their own layer
//! without clobbering one another; setting an empty list removes it.
//!
//! Widgets are real GPUI elements laid out and painted by the code element
//! after the text, clipped to the text area, on top of the code's hitboxes
//! (clicks on a widget never move the caret).

use std::collections::BTreeMap;
use std::ops::Range;
use std::rc::Rc;

use gpui::{AnyElement, App, Context, Hsla, Pixels, Window};

use super::CodeView;

/// A full-width background over `rows` (text area), painted under the text
/// and above the source's own row decorations.
#[derive(Clone, Debug, PartialEq)]
pub struct RowBackground {
    /// Rows covered (0-based, end exclusive).
    pub rows: Range<u32>,
    /// Fill color (use a translucent color).
    pub color: Hsla,
    /// Optional 2 px marker in the left edge of the text area.
    pub edge: Option<Hsla>,
}

/// Where a [`RowWidget`] sits horizontally on its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WidgetAnchor {
    /// Two columns after the end of the row's text (scrolls with the text).
    #[default]
    AfterText,
    /// Right edge of the text area (stays put while scrolling horizontally).
    Right,
}

/// Builds a widget's element each frame.
pub type RenderWidget = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// An element drawn on a row. Its height should not exceed the line height.
#[derive(Clone)]
pub struct RowWidget {
    /// Row (0-based). Hidden while folded away or under a sticky header.
    pub row: u32,
    /// Horizontal placement.
    pub anchor: WidgetAnchor,
    /// The element (give interactive children unique ids).
    pub render: RenderWidget,
}

impl std::fmt::Debug for RowWidget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowWidget")
            .field("row", &self.row)
            .field("anchor", &self.anchor)
            .finish_non_exhaustive()
    }
}

/// Per-owner layers of one view.
#[derive(Default)]
pub(crate) struct Layers {
    backgrounds: BTreeMap<&'static str, Rc<Vec<RowBackground>>>,
    widgets: BTreeMap<&'static str, Rc<Vec<RowWidget>>>,
}

impl Layers {
    /// Every background, all owners.
    pub(crate) fn backgrounds(&self) -> Vec<Rc<Vec<RowBackground>>> {
        self.backgrounds.values().cloned().collect()
    }

    /// Every widget, all owners.
    pub(crate) fn widgets(&self) -> Vec<RowWidget> {
        self.widgets
            .values()
            .flat_map(|w| w.iter().cloned())
            .collect()
    }
}

impl CodeView {
    /// Replaces `owner`'s row backgrounds (empty = remove).
    pub fn set_row_backgrounds(
        &mut self,
        owner: &'static str,
        backgrounds: Vec<RowBackground>,
        cx: &mut Context<Self>,
    ) {
        if backgrounds.is_empty() {
            if self.layers.backgrounds.remove(owner).is_none() {
                return;
            }
        } else {
            self.layers.backgrounds.insert(owner, Rc::new(backgrounds));
        }
        cx.notify();
    }

    /// Replaces `owner`'s row widgets (empty = remove).
    pub fn set_row_widgets(
        &mut self,
        owner: &'static str,
        widgets: Vec<RowWidget>,
        cx: &mut Context<Self>,
    ) {
        if widgets.is_empty() {
            if self.layers.widgets.remove(owner).is_none() {
                return;
            }
        } else {
            self.layers.widgets.insert(owner, Rc::new(widgets));
        }
        cx.notify();
    }

    /// Whether `owner` has widgets or backgrounds set.
    pub fn has_layer(&self, owner: &'static str) -> bool {
        self.layers.widgets.contains_key(owner) || self.layers.backgrounds.contains_key(owner)
    }

    /// Width of the text area in the last painted frame (row widgets that span
    /// the view, e.g. inline comment threads).
    pub fn text_width(&self) -> Option<Pixels> {
        Some(self.last_layout.as_ref()?.text_bounds.size.width)
    }

    /// Top of `row` relative to the view's top edge in the last painted
    /// frame (`None` when it was not visible).
    pub fn row_top(&self, row: u32) -> Option<Pixels> {
        let l = self.last_layout.as_ref()?;
        let rl = l.rows.iter().find(|r| r.row == row && r.first_seg)?;
        Some(rl.y - l.bounds.top())
    }
}
