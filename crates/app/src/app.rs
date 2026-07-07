use std::collections::HashMap;
use std::sync::Arc;

use pfnc_config::{Config, PanelLocationConfig, PanelSessionConfig};
use pfnc_core::{JobId, JobManager, Location, Mode, PanelState, ProfileId, VfsPath};

use crate::editor::EditTarget;
use crate::keymap::Keymap;
use crate::registry::VfsRegistry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneSide {
    Left,
    Right,
}

/// A job that runs on a background thread (via `App::jobs`, same as any
/// other) but — unlike every `JobKind` — never takes over `app.mode` with a
/// modal `Mode::Progress` dialog. `actions::handle_job_event` checks
/// `App::background_jobs` before falling back to the `Mode::Progress`-based
/// dispatch, so a background job's completion is handled without disturbing
/// whatever the UI is currently showing.
#[derive(Clone, Debug)]
pub enum BackgroundJob {
    /// Deploys/connects a QUIC fast-path agent for an already-established
    /// SFTP connection, silently, after the panel is already browsable —
    /// see `actions::submit_connect`. On completion, whichever panel(s)
    /// currently point at `profile_id` (directly or via an archive layered
    /// over it) get their `quic_available` glyph updated to match.
    WarmQuicFastPath { profile_id: ProfileId },
}

pub struct App {
    pub registry: Arc<VfsRegistry>,
    pub left: PanelState,
    pub right: PanelState,
    pub active: PaneSide,
    pub mode: Mode,
    pub jobs: JobManager,
    /// Job IDs spawned via `jobs` that should *not* be treated as the
    /// current `Mode::Progress` job when they finish — see `BackgroundJob`.
    pub background_jobs: HashMap<JobId, BackgroundJob>,
    pub status: Option<String>,
    pub should_quit: bool,
    /// Set by F3 ("Edit"); the main loop takes this and runs
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
            background_jobs: HashMap::new(),
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

/// Whether `location`'s connection (or, for an archive, whatever it's
/// layered over) has a QUIC fast-path agent available — used to set
/// `PanelState::quic_available` right after a connect/archive-open job
/// completes, and again once a `BackgroundJob::WarmQuicFastPath` job
/// finishes (see `update_quic_available_for_profile`). Calling this before
/// that warm-up has run (or completed) is safe — it just triggers
/// `Vfs::fast_transport()`'s own probe-and-cache — but `submit_connect`
/// deliberately defers the actual probe to a background job so it never
/// blocks the UI thread; this function itself doesn't guarantee that.
pub fn quic_available_for(registry: &VfsRegistry, location: &Location) -> bool {
    match location {
        Location::Local => false,
        Location::Remote { .. } => {
            registry.resolve(location).ok().and_then(|vfs| vfs.fast_transport()).is_some()
        }
        Location::Archive { base, .. } => quic_available_for(registry, base),
    }
}

/// Whether `location` is (or, for an archive, is layered over) exactly
/// `profile_id` — used to find which panel(s) a finished
/// `BackgroundJob::WarmQuicFastPath` job is relevant to.
fn location_uses_profile(location: &Location, profile_id: &ProfileId) -> bool {
    match location {
        Location::Local => false,
        Location::Remote { profile_id: id } => id == profile_id,
        Location::Archive { base, .. } => location_uses_profile(base, profile_id),
    }
}

/// Re-derives `quic_available` (via the already-cached
/// `Vfs::fast_transport()`, see `quic_available_for`) for whichever of
/// `app.left`/`app.right` currently point at `profile_id` — called once a
/// `BackgroundJob::WarmQuicFastPath` job finishes, so the panel glyph
/// flips from plain-remote to QUIC-fast-path asynchronously, without ever
/// having blocked the Connect flow on the probe itself.
pub fn update_quic_available_for_profile(app: &mut App, profile_id: &ProfileId) {
    if location_uses_profile(&app.left.location, profile_id) {
        app.left.quic_available = quic_available_for(&app.registry, &app.left.location);
    }
    if location_uses_profile(&app.right.location, profile_id) {
        app.right.quic_available = quic_available_for(&app.registry, &app.right.location);
    }
}

/// Diagnostic info about `location`'s connection (or, for an archive,
/// whatever it's layered over), for the F1 Help box — `None` for
/// `Location::Local`. Only reads already-cached facts (see
/// `Vfs::connection_info`), so this is safe to call directly from render
/// code on every keypress that opens the Help dialog.
pub fn connection_info_for(registry: &VfsRegistry, location: &Location) -> Option<pfnc_core::ConnectionInfo> {
    match location {
        Location::Local => None,
        Location::Remote { .. } => registry.resolve(location).ok().and_then(|vfs| vfs.connection_info()),
        Location::Archive { base, .. } => connection_info_for(registry, base),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_uses_profile_is_false_for_local() {
        assert!(!location_uses_profile(&Location::Local, &"p1".to_string()));
    }

    #[test]
    fn location_uses_profile_matches_direct_remote() {
        let location = Location::Remote { profile_id: "p1".to_string() };
        assert!(location_uses_profile(&location, &"p1".to_string()));
        assert!(!location_uses_profile(&location, &"p2".to_string()));
    }

    #[test]
    fn location_uses_profile_recurses_through_archive() {
        let location = Location::Archive {
            base: Box::new(Location::Remote { profile_id: "p1".to_string() }),
            archive_path: VfsPath::from("/some.tar"),
        };
        assert!(location_uses_profile(&location, &"p1".to_string()));
        assert!(!location_uses_profile(&location, &"p2".to_string()));
    }
}
