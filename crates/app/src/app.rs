use std::sync::Arc;

use pfnc_config::{Config, PanelLocationConfig, PanelSessionConfig};
use pfnc_core::{JobManager, Location, Mode, PanelState, VfsPath};

use crate::editor::EditTarget;
use crate::keymap::Keymap;
use crate::registry::VfsRegistry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneSide {
    Left,
    Right,
}

pub struct App {
    pub registry: Arc<VfsRegistry>,
    pub left: PanelState,
    pub right: PanelState,
    pub active: PaneSide,
    pub mode: Mode,
    pub jobs: JobManager,
    pub status: Option<String>,
    pub should_quit: bool,
    /// Set by F4 ("Edit"); the main loop takes this and runs
    /// `editor::edit_file`, which needs `&mut DefaultTerminal` — something
    /// `actions::handle_key` deliberately doesn't have access to, keeping
    /// terminal suspend/resume out of the input-handling layer.
    pub edit_request: Option<EditTarget>,
    pub keymap: Keymap,
    /// Loaded once at startup and saved on quit (see `main.rs`) — holds
    /// general settings, keybinding overrides, saved connection profiles,
    /// and (a subset of) the session to restore next launch.
    pub config: Config,
}

impl App {
    /// Used by tests and anywhere else that wants deterministic, isolated
    /// startup: both panels start at `start_dir`, no config file involved.
    pub fn new(start_dir: VfsPath) -> Self {
        Self::new_with_config(Config::default(), start_dir)
    }

    /// The real startup path: restores each panel's last local cwd from
    /// `config.session` when available (falling back to `fallback_dir`
    /// otherwise), and applies `config.keybindings`.
    ///
    /// Only `Location::Local` panels are ever restored — a remote panel
    /// would need a saved password to silently reconnect, and we never
    /// persist one (see `pfnc_config::ConnectionProfileConfig`), so
    /// restoring one automatically isn't possible without either a
    /// surprising blocking prompt at startup or a silent failure. Users
    /// just reconnect via F9.
    pub fn new_with_config(config: Config, fallback_dir: VfsPath) -> Self {
        let registry = Arc::new(VfsRegistry::new());
        let keymap = Keymap::from_overrides(&config.keybindings);

        let left_start = restorable_local_cwd(config.session.left.as_ref()).unwrap_or_else(|| fallback_dir.clone());
        let right_start = restorable_local_cwd(config.session.right.as_ref()).unwrap_or(fallback_dir);

        let mut left = PanelState::new(Location::Local, left_start);
        let mut right = PanelState::new(Location::Local, right_start);
        reload(&registry, &mut left);
        reload(&registry, &mut right);

        Self {
            registry,
            left,
            right,
            active: PaneSide::Left,
            mode: Mode::Browsing,
            jobs: JobManager::new(),
            status: None,
            should_quit: false,
            edit_request: None,
            keymap,
            config,
        }
    }

    pub fn active_panel(&self) -> &PanelState {
        match self.active {
            PaneSide::Left => &self.left,
            PaneSide::Right => &self.right,
        }
    }

    pub fn active_panel_mut(&mut self) -> &mut PanelState {
        match self.active {
            PaneSide::Left => &mut self.left,
            PaneSide::Right => &mut self.right,
        }
    }

    pub fn inactive_panel(&self) -> &PanelState {
        match self.active {
            PaneSide::Left => &self.right,
            PaneSide::Right => &self.left,
        }
    }

    pub fn reload_both(&mut self) {
        reload(&self.registry, &mut self.left);
        reload(&self.registry, &mut self.right);
    }

    /// Snapshots the current session (local panel locations only — see
    /// `new_with_config`) into `self.config`, ready for `Config::save`.
    pub fn sync_session_into_config(&mut self) {
        self.config.session.left = panel_session_config(&self.left);
        self.config.session.right = panel_session_config(&self.right);
    }
}

fn panel_session_config(panel: &PanelState) -> Option<PanelSessionConfig> {
    if panel.location != Location::Local {
        return None;
    }
    Some(PanelSessionConfig {
        location: PanelLocationConfig::Local,
        cwd: panel.cwd.to_string(),
    })
}

fn restorable_local_cwd(saved: Option<&PanelSessionConfig>) -> Option<VfsPath> {
    let saved = saved?;
    if saved.location != PanelLocationConfig::Local {
        return None;
    }
    let path = VfsPath::from(saved.cwd.clone());
    // Don't restore into a directory that's since been deleted/renamed —
    // better to fall back to a known-good default than show a silently
    // empty panel with a confusing path in the title.
    if path.as_std_path().is_dir() {
        Some(path)
    } else {
        None
    }
}

/// Reload a panel's directory listing from its backend, clearing selection
/// and keeping the cursor in range if the listing shrank. Resolving the
/// backend or listing the directory can fail (e.g. a dropped SFTP
/// connection) without taking the app down — the panel just shows empty
/// and the error is logged.
pub fn reload(registry: &VfsRegistry, panel: &mut PanelState) {
    let result = registry.resolve(&panel.location).and_then(|vfs| vfs.list_dir(&panel.cwd));
    match result {
        Ok(entries) => panel.entries = entries,
        Err(e) => {
            tracing::warn!(cwd = %panel.cwd, error = %e, "failed to list directory");
            panel.entries.clear();
        }
    }
    panel.selected.clear();
    panel.clamp_cursor();
}

/// Items an action should operate on: the multi-selection if non-empty,
/// otherwise just the item under the cursor.
pub fn selected_items(panel: &PanelState) -> Vec<VfsPath> {
    if !panel.selected.is_empty() {
        panel.selected.iter().cloned().collect()
    } else if let Some(entry) = panel.cursor_entry() {
        vec![entry.path.clone()]
    } else {
        vec![]
    }
}
