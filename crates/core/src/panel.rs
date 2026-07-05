//! `PanelState`: what a single pane knows about itself. Backend-agnostic —
//! rendered by `pfnc-tui`, mutated by `pfnc` (the app crate) via the `Vfs`
//! trait, never tied to a concrete backend type.

use std::collections::HashSet;

use crate::vfs::{EntryMeta, Location, VfsPath};

#[derive(Clone, Debug)]
pub struct PanelState {
    pub location: Location,
    pub cwd: VfsPath,
    pub entries: Vec<EntryMeta>,
    pub selected: HashSet<VfsPath>,
    pub cursor: usize,
    /// Index of the first entry currently shown on screen. Kept in sync
    /// with `cursor` by `sync_viewport`, which `pfnc-tui`'s `render_panel`
    /// calls once per frame — never mutated directly by input handling.
    pub scroll_offset: usize,
    /// Number of entry rows that fit in this panel's last rendered area
    /// (i.e. excluding borders/title). Updated by `sync_viewport`; lets
    /// PageUp/PageDown jump by a full screen instead of a fixed guess.
    pub viewport_height: usize,
    /// Whether this panel's connection (if remote) has a QUIC fast-path
    /// agent available — set once by the app layer right after a connect
    /// (or archive-open) job completes, from the already-cached result of
    /// `Vfs::fast_transport()`, never recomputed on every render. Always
    /// `false` for `Location::Local`. Purely a UI hint (which glyph
    /// `pfnc-tui` shows); `negotiate_transport` makes its own independent
    /// decision at transfer time.
    pub quic_available: bool,
}

impl PanelState {
    pub fn new(location: Location, cwd: VfsPath) -> Self {
        Self {
            location,
            cwd,
            entries: Vec::new(),
            selected: HashSet::new(),
            cursor: 0,
            scroll_offset: 0,
            viewport_height: 0,
            quic_available: false,
        }
    }

    pub fn cursor_entry(&self) -> Option<&EntryMeta> {
        self.entries.get(self.cursor)
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            return;
        }
        let max = self.entries.len() as isize - 1;
        let next = (self.cursor as isize + delta).clamp(0, max);
        self.cursor = next as usize;
    }

    /// Moves the cursor onto the entry named `name`, if present — used
    /// after reloading a parent directory so the cursor lands back on the
    /// child just left, Midnight-Commander-style, rather than resetting to
    /// the top. Leaves `cursor` untouched if no entry matches.
    pub fn select_by_name(&mut self, name: &str) {
        if let Some(index) = self.entries.iter().position(|e| e.name == name) {
            self.cursor = index;
        }
    }

    /// Clamp the cursor after `entries` changes (e.g. after a reload),
    /// so a panel that shrank doesn't leave a stale out-of-range cursor.
    pub fn clamp_cursor(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len() - 1;
        }
    }

    /// Keeps `scroll_offset` tracking `cursor` for a panel rendered
    /// `height` entry-rows tall. Scrolls the minimum amount needed to keep
    /// the cursor visible (rather than re-centering it), and never leaves
    /// blank trailing space when there are enough entries to fill the
    /// screen. Called once per frame by `pfnc-tui::render_panel`, which is
    /// the only place that knows the actual terminal size.
    pub fn sync_viewport(&mut self, height: usize) {
        self.viewport_height = height;
        if height == 0 {
            self.scroll_offset = 0;
            return;
        }
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset + height {
            self.scroll_offset = self.cursor + 1 - height;
        }
        let max_offset = self.entries.len().saturating_sub(height);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{EntryKind, EntryMeta};

    fn panel_with_entries(count: usize) -> PanelState {
        let mut panel = PanelState::new(Location::Local, VfsPath::from("/"));
        panel.entries = (0..count)
            .map(|i| EntryMeta {
                name: format!("f{i}"),
                path: VfsPath::from(format!("/f{i}")),
                kind: EntryKind::File,
                size: 0,
                modified: None,
                permissions: None,
                owner: None,
                group: None,
            })
            .collect();
        panel
    }

    #[test]
    fn sync_viewport_does_nothing_when_everything_fits() {
        let mut panel = panel_with_entries(5);
        panel.cursor = 4;
        panel.sync_viewport(10);
        assert_eq!(panel.scroll_offset, 0);
    }

    #[test]
    fn sync_viewport_scrolls_down_to_keep_cursor_visible() {
        let mut panel = panel_with_entries(50);
        panel.cursor = 20;
        panel.sync_viewport(10);
        // Cursor must be the last visible row, not off-screen.
        assert_eq!(panel.scroll_offset, 11);
        assert!(panel.cursor >= panel.scroll_offset && panel.cursor < panel.scroll_offset + 10);
    }

    #[test]
    fn sync_viewport_scrolls_up_minimally_rather_than_recentering() {
        let mut panel = panel_with_entries(50);
        panel.cursor = 30;
        panel.sync_viewport(10); // scrolls down first
        assert_eq!(panel.scroll_offset, 21);

        // Move up by one, just above the visible window's top edge.
        panel.cursor = 20;
        panel.sync_viewport(10);
        assert_eq!(panel.scroll_offset, 20, "should scroll up by exactly 1, not jump to re-center");
    }

    #[test]
    fn sync_viewport_never_leaves_blank_trailing_space() {
        let mut panel = panel_with_entries(15);
        panel.cursor = 5;
        panel.sync_viewport(10);
        // Only 15 entries total; offset must not exceed 5 (15 - 10) even
        // though the cursor alone wouldn't require scrolling.
        assert_eq!(panel.scroll_offset, 0);

        panel.cursor = 14;
        panel.sync_viewport(10);
        assert_eq!(panel.scroll_offset, 5);
    }
}
