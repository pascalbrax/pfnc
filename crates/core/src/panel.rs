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
}

impl PanelState {
    pub fn new(location: Location, cwd: VfsPath) -> Self {
        Self {
            location,
            cwd,
            entries: Vec::new(),
            selected: HashSet::new(),
            cursor: 0,
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

    /// Clamp the cursor after `entries` changes (e.g. after a reload),
    /// so a panel that shrank doesn't leave a stale out-of-range cursor.
    pub fn clamp_cursor(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len() - 1;
        }
    }
}
