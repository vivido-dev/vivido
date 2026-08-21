//! Ordered one-pane tab model used by standalone Vivido.

use std::ops::Range;

use winit::window::WindowId;

/// One standalone Vivido tab.
#[derive(Clone, Debug)]
pub struct Tab {
    pub window_id: WindowId,
    pub title: String,
}

/// The visible portion of an overflowing tab strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleTabs {
    pub range: Range<usize>,
    pub has_previous: bool,
    pub has_next: bool,
}

/// Ordered tabs with a single active pane.
#[derive(Debug, Default)]
pub struct Tabs {
    tabs: Vec<Tab>,
    active: Option<usize>,
    visible_start: usize,
}

impl Tabs {
    pub fn as_slice(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub fn active(&self) -> Option<&Tab> {
        self.active.and_then(|index| self.tabs.get(index))
    }

    pub fn active_window(&self) -> Option<WindowId> {
        self.active().map(|tab| tab.window_id)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn add(&mut self, window_id: WindowId, title: String) {
        self.tabs.push(Tab { window_id, title });
        self.active = Some(self.tabs.len() - 1);
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() || self.active == Some(index) {
            return false;
        }
        self.active = Some(index);
        true
    }

    pub fn select_window(&mut self, window_id: WindowId) -> bool {
        self.tabs
            .iter()
            .position(|tab| tab.window_id == window_id)
            .is_some_and(|index| self.select(index))
    }

    pub fn cycle(&mut self, delta: isize) -> bool {
        let Some(active) = self.active else { return false };
        let Ok(len) = isize::try_from(self.tabs.len()) else { return false };
        if len < 2 {
            return false;
        }
        let next = (isize::try_from(active).unwrap_or_default() + delta).rem_euclid(len);
        self.select(usize::try_from(next).unwrap_or_default())
    }

    /// Remove a tab and select the tab which occupied its position, otherwise the previous tab.
    pub fn remove(&mut self, window_id: WindowId) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.window_id == window_id) else {
            return false;
        };
        self.tabs.remove(index);
        self.active =
            if self.tabs.is_empty() { None } else { Some(index.min(self.tabs.len() - 1)) };
        self.visible_start = self.visible_start.min(self.tabs.len().saturating_sub(1));
        true
    }

    pub fn update_title(&mut self, window_id: WindowId, title: String) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.window_id == window_id) else {
            return false;
        };
        if tab.title == title {
            return false;
        }
        tab.title = title;
        true
    }

    /// Return a contiguous visible range which always contains the active tab.
    pub fn visible(&mut self, capacity: usize) -> VisibleTabs {
        let capacity = capacity.max(1).min(self.tabs.len().max(1));
        let active = self.active.unwrap_or_default();
        if active < self.visible_start {
            self.visible_start = active;
        } else if active >= self.visible_start.saturating_add(capacity) {
            self.visible_start = active + 1 - capacity;
        }
        let max_start = self.tabs.len().saturating_sub(capacity);
        self.visible_start = self.visible_start.min(max_start);
        let end = self.visible_start.saturating_add(capacity).min(self.tabs.len());
        VisibleTabs {
            range: self.visible_start..end,
            has_previous: self.visible_start > 0,
            has_next: end < self.tabs.len(),
        }
    }

    pub fn shift_visible(&mut self, delta: isize, capacity: usize) {
        let max_start = self.tabs.len().saturating_sub(capacity.max(1));
        let current = isize::try_from(self.visible_start).unwrap_or(isize::MAX);
        self.visible_start = usize::try_from(current.saturating_add(delta).max(0))
            .unwrap_or(max_start)
            .min(max_start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_selects_right_then_left() {
        let mut tabs = Tabs::default();
        let ids = [WindowId::from(1), WindowId::from(2), WindowId::from(3)];
        for id in ids {
            tabs.add(id, String::new());
        }
        tabs.select(1);
        assert!(tabs.remove(ids[1]));
        assert_eq!(tabs.active_window(), Some(ids[2]));
        assert!(tabs.remove(ids[2]));
        assert_eq!(tabs.active_window(), Some(ids[0]));
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let mut tabs = Tabs::default();
        tabs.add(WindowId::from(1), String::new());
        tabs.add(WindowId::from(2), String::new());
        assert!(tabs.cycle(1));
        assert_eq!(tabs.active_index(), Some(0));
        assert!(tabs.cycle(-1));
        assert_eq!(tabs.active_index(), Some(1));
    }

    #[test]
    fn visible_range_follows_active_tab() {
        let mut tabs = Tabs::default();
        for id in 1..=8 {
            tabs.add(WindowId::from(id), id.to_string());
        }
        assert_eq!(tabs.visible(3).range, 5..8);
        tabs.select(1);
        assert_eq!(tabs.visible(3).range, 1..4);
    }

    #[test]
    fn insertion_and_title_updates_keep_stable_window_identity() {
        let first = WindowId::from(41);
        let second = WindowId::from(42);
        let mut tabs = Tabs::default();
        tabs.add(first, "first".into());
        tabs.add(second, "second".into());
        assert_eq!(tabs.active_window(), Some(second));
        assert!(tabs.update_title(first, "renamed".into()));
        assert_eq!(tabs.as_slice()[0].window_id, first);
        assert_eq!(tabs.as_slice()[0].title, "renamed");
    }

    #[test]
    fn removing_final_tab_clears_active_selection() {
        let id = WindowId::from(7);
        let mut tabs = Tabs::default();
        tabs.add(id, String::new());
        assert!(tabs.remove(id));
        assert!(tabs.is_empty());
        assert_eq!(tabs.active_index(), None);
    }
}
