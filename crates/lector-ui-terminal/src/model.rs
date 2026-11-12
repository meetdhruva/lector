//! The Terminal panel's tab and split model, independent of GPUI (tested directly).
//!
//! A panel has tabs; a tab has one or more terminals laid out along one axis (splits). One
//! terminal per tab has the focus; one tab is active.

/// Split direction of a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SplitAxis {
    /// Side by side.
    #[default]
    Horizontal,
    /// One above the other.
    Vertical,
}

/// One tab.
#[derive(Debug, Clone, PartialEq)]
pub struct TabModel<T> {
    /// Stable id (for element ids and drag and drop).
    pub id: u64,
    /// Terminals, in layout order (at least one).
    pub panes: Vec<T>,
    /// Split axis (meaningful with two or more panes).
    pub axis: SplitAxis,
    /// Focused pane.
    pub focused: usize,
    /// Name given by the user (Rename), else the focused terminal's title is shown.
    pub custom_title: Option<String>,
}

impl<T> TabModel<T> {
    /// The focused terminal.
    pub fn focused_pane(&self) -> Option<&T> {
        self.panes.get(self.focused)
    }
}

/// Tabs of the panel.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelModel<T> {
    tabs: Vec<TabModel<T>>,
    active: usize,
    next_id: u64,
}

impl<T> Default for PanelModel<T> {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
        }
    }
}

impl<T: PartialEq + Clone> PanelModel<T> {
    /// Tabs in order.
    pub fn tabs(&self) -> &[TabModel<T>] {
        &self.tabs
    }

    /// Index of the active tab.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The active tab.
    pub fn active_tab(&self) -> Option<&TabModel<T>> {
        self.tabs.get(self.active)
    }

    /// The focused terminal of the active tab.
    pub fn active_terminal(&self) -> Option<&T> {
        self.active_tab().and_then(TabModel::focused_pane)
    }

    /// Whether there are no tabs.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Every terminal, tab by tab.
    pub fn all(&self) -> impl Iterator<Item = &T> {
        self.tabs.iter().flat_map(|t| t.panes.iter())
    }

    /// (tab index, pane index) of `item`.
    pub fn position(&self, item: &T) -> Option<(usize, usize)> {
        self.tabs
            .iter()
            .enumerate()
            .find_map(|(ti, t)| t.panes.iter().position(|p| p == item).map(|pi| (ti, pi)))
    }

    /// Adds a tab after the active one and activates it; returns its id.
    pub fn add_tab(&mut self, item: T) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let ix = if self.tabs.is_empty() {
            0
        } else {
            self.active + 1
        };
        self.tabs.insert(
            ix,
            TabModel {
                id,
                panes: vec![item],
                axis: SplitAxis::Horizontal,
                focused: 0,
                custom_title: None,
            },
        );
        self.active = ix;
        id
    }

    /// Splits the active tab: `item` goes after the focused pane and gets the focus. The
    /// first split sets the tab's axis. Without tabs, opens a tab.
    pub fn split(&mut self, item: T, axis: SplitAxis) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            self.add_tab(item);
            return;
        };
        if tab.panes.len() == 1 {
            tab.axis = axis;
        }
        let at = (tab.focused + 1).min(tab.panes.len());
        tab.panes.insert(at, item);
        tab.focused = at;
    }

    /// Removes `item`. Its tab goes away with its last pane. Returns whether it was found.
    pub fn remove(&mut self, item: &T) -> bool {
        let Some((ti, pi)) = self.position(item) else {
            return false;
        };
        let tab = &mut self.tabs[ti];
        tab.panes.remove(pi);
        if tab.panes.is_empty() {
            self.tabs.remove(ti);
            if self.active > ti || self.active >= self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
        } else if tab.focused >= pi && tab.focused > 0 {
            tab.focused -= 1;
        }
        true
    }

    /// Activates tab `ix`.
    pub fn activate(&mut self, ix: usize) {
        if ix < self.tabs.len() {
            self.active = ix;
        }
    }

    /// Activates and focuses `item`.
    pub fn focus(&mut self, item: &T) {
        if let Some((ti, pi)) = self.position(item) {
            self.active = ti;
            self.tabs[ti].focused = pi;
        }
    }

    /// Next (`delta` = 1) / previous (-1) tab, wrapping.
    pub fn activate_relative(&mut self, delta: isize) {
        let n = self.tabs.len() as isize;
        if n > 0 {
            self.active = (self.active as isize + delta).rem_euclid(n) as usize;
        }
    }

    /// Next / previous split of the active tab, wrapping.
    pub fn focus_split_relative(&mut self, delta: isize) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let n = tab.panes.len() as isize;
            if n > 0 {
                tab.focused = (tab.focused as isize + delta).rem_euclid(n) as usize;
            }
        }
    }

    /// Moves tab `from` to index `to` (drag and drop), keeping the active tab active.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || from == to {
            return;
        }
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        let tab = self.tabs.remove(from);
        let to = to.min(self.tabs.len());
        self.tabs.insert(to, tab);
        if let Some(id) = active_id {
            if let Some(ix) = self.tabs.iter().position(|t| t.id == id) {
                self.active = ix;
            }
        }
    }

    /// Index of the tab with `id`.
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Renames tab `ix` (`None` or blank: back to the automatic title).
    pub fn rename(&mut self, ix: usize, title: Option<String>) {
        if let Some(tab) = self.tabs.get_mut(ix) {
            tab.custom_title = title
                .map(|t| t.trim().chars().take(80).collect::<String>())
                .filter(|t| !t.is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_and_splits() {
        let mut m = PanelModel::<u32>::default();
        assert!(m.active_terminal().is_none());
        m.add_tab(1);
        m.add_tab(2);
        assert_eq!(m.active_terminal(), Some(&2));
        m.split(3, SplitAxis::Vertical);
        assert_eq!(m.active_tab().map(|t| t.panes.clone()), Some(vec![2, 3]));
        assert_eq!(m.active_tab().map(|t| t.axis), Some(SplitAxis::Vertical));
        assert_eq!(m.active_terminal(), Some(&3));
        // A second split keeps the axis.
        m.split(4, SplitAxis::Horizontal);
        assert_eq!(m.active_tab().map(|t| t.axis), Some(SplitAxis::Vertical));
        assert_eq!(m.active_tab().map(|t| t.panes.clone()), Some(vec![2, 3, 4]));
        m.focus_split_relative(1);
        assert_eq!(m.active_terminal(), Some(&2));
        m.focus_split_relative(-1);
        assert_eq!(m.active_terminal(), Some(&4));
        // Closing the focused split focuses its neighbour.
        assert!(m.remove(&4));
        assert_eq!(m.active_terminal(), Some(&3));
        assert!(m.remove(&3));
        assert!(m.remove(&2));
        assert_eq!(m.tabs().len(), 1);
        assert_eq!(m.active_terminal(), Some(&1));
        assert!(!m.remove(&9));
        m.activate_relative(1);
        assert_eq!(m.active_terminal(), Some(&1));
        assert!(m.remove(&1));
        assert!(m.is_empty());
        // Splitting with no tab opens one.
        m.split(5, SplitAxis::Horizontal);
        assert_eq!(m.active_terminal(), Some(&5));
    }

    #[test]
    fn reorder_rename_focus() {
        let mut m = PanelModel::<u32>::default();
        for i in 1..=3 {
            m.add_tab(i);
        }
        m.activate(0);
        m.move_tab(0, 2);
        let order: Vec<u32> = m.tabs().iter().map(|t| t.panes[0]).collect();
        assert_eq!(order, vec![2, 3, 1]);
        assert_eq!(m.active_terminal(), Some(&1), "the active tab moves along");
        m.rename(2, Some("  build  ".into()));
        assert_eq!(m.tabs()[2].custom_title.as_deref(), Some("build"));
        m.rename(2, Some("   ".into()));
        assert_eq!(m.tabs()[2].custom_title, None);
        m.focus(&3);
        assert_eq!(m.active_index(), 1);
        m.activate_relative(-1);
        assert_eq!(m.active_terminal(), Some(&2));
        m.activate_relative(-1);
        assert_eq!(m.active_terminal(), Some(&1));
        // Removing a tab before the active one keeps the same tab active.
        m.focus(&3);
        assert!(m.remove(&2));
        assert_eq!(m.active_terminal(), Some(&3));
        assert_eq!(m.all().copied().collect::<Vec<_>>(), vec![3, 1]);
        let id = m.tabs()[1].id;
        assert_eq!(m.index_of(id), Some(1));
    }
}
