use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};

use pfnc_core::{
    job, ConfirmDialog, ConfirmPurpose, ConnectForm, JobEvent, JobKind, JobOutcome, Location, Mode, ProgressState,
    SavedProfileSummary, SyncPlanCell, TextInputPrompt, TextInputPurpose, VfsPath,
};
use pfnc_vfs_sftp::{AuthMethod, ConnectionProfile};

use crate::app::{reload, selected_items, App, PaneSide};
use crate::editor::EditTarget;

pub fn handle_key(app: &mut App, key: KeyEvent) {
    match std::mem::replace(&mut app.mode, Mode::Browsing) {
        Mode::Browsing => handle_browsing_key(app, key.code),
        Mode::Confirm(dialog) => handle_confirm_key(app, dialog, key.code),
        Mode::TextInput(prompt) => handle_text_input_key(app, prompt, key.code),
        Mode::Connect(form) => handle_connect_key(app, form, key.code),
        Mode::Progress(state) => handle_progress_key(app, state, key.code),
        // Already reset to `Mode::Browsing` above; any key dismisses it.
        Mode::Help => {}
    }
}

fn handle_browsing_key(app: &mut App, key: KeyCode) {
    // Configurable actions (F-keys by default; see `keymap`) take priority
    // over the fixed navigation aliases below, so a remap always wins.
    if let Some(action) = app.keymap.action_for(key) {
        match action {
            "quit" => {
                app.should_quit = true;
                return;
            }
            "edit" => {
                start_edit(app);
                return;
            }
            "copy" => {
                start_copy(app);
                return;
            }
            "move" => {
                start_move(app);
                return;
            }
            "mkdir" => {
                start_mkdir(app);
                return;
            }
            "delete" => {
                start_delete(app);
                return;
            }
            "connect" => {
                start_connect(app);
                return;
            }
            "sync" => {
                start_sync(app);
                return;
            }
            "help" => {
                app.mode = Mode::Help;
                return;
            }
            _ => {}
        }
    }

    match key {
        // Fixed aliases: not remappable, so a config mistake can't lock
        // someone out of quitting or moving the cursor.
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Tab => {
            app.active = match app.active {
                PaneSide::Left => PaneSide::Right,
                PaneSide::Right => PaneSide::Left,
            };
        }
        KeyCode::Up | KeyCode::Char('k') => app.active_panel_mut().move_cursor(-1),
        KeyCode::Down | KeyCode::Char('j') => app.active_panel_mut().move_cursor(1),
        KeyCode::PageUp => {
            let page = page_size(app.active_panel());
            app.active_panel_mut().move_cursor(-page);
        }
        KeyCode::PageDown => {
            let page = page_size(app.active_panel());
            app.active_panel_mut().move_cursor(page);
        }
        KeyCode::Char(' ') => toggle_selection(app),
        KeyCode::Enter => descend(app),
        KeyCode::Backspace | KeyCode::Left => ascend(app),
        _ => {}
    }
}

/// How far PageUp/PageDown move the cursor: a full screen's worth of rows,
/// from the panel's last rendered `viewport_height` (set by
/// `pfnc-tui::render_panel` each frame). Falls back to 1 for the
/// vanishingly unlikely case a page key arrives before the first render.
fn page_size(panel: &pfnc_core::PanelState) -> isize {
    panel.viewport_height.max(1) as isize
}

fn toggle_selection(app: &mut App) {
    let panel = app.active_panel_mut();
    if let Some(entry) = panel.cursor_entry().cloned() {
        if !panel.selected.remove(&entry.path) {
            panel.selected.insert(entry.path);
        }
        panel.move_cursor(1);
    }
}

fn descend(app: &mut App) {
    let panel = app.active_panel();
    let Some(entry) = panel.cursor_entry().cloned() else {
        return;
    };

    if entry.kind.is_dir() {
        let panel = app.active_panel_mut();
        panel.cwd = entry.path;
        panel.cursor = 0;
        let registry = Arc::clone(&app.registry);
        reload(&registry, app.active_panel_mut());
        return;
    }

    if pfnc_vfs_archive::detect_format(&entry.path).is_some() {
        let location = Location::Archive {
            base: Box::new(app.active_panel().location.clone()),
            archive_path: entry.path,
        };
        start_open_archive(app, location);
    }
}

fn ascend(app: &mut App) {
    // At an archive's own virtual root, "up" means popping back out to the
    // real location the archive file lives in, not treating "/" as having
    // no parent (which the generic case below would otherwise do).
    if app.active_panel().cwd == "/" {
        if let Location::Archive { base, archive_path } = &app.active_panel().location {
            let new_location = (**base).clone();
            let new_cwd = archive_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| VfsPath::from("/"));
            // The archive file itself is what we're "leaving" here, so the
            // cursor should land on it in the parent listing, same as the
            // generic directory case below.
            let archive_name = archive_path.file_name().map(str::to_string);
            let panel = app.active_panel_mut();
            panel.location = new_location;
            panel.cwd = new_cwd;
            panel.cursor = 0;
            let registry = Arc::clone(&app.registry);
            reload(&registry, app.active_panel_mut());
            if let Some(name) = archive_name {
                app.active_panel_mut().select_by_name(&name);
            }
            return;
        }
    }

    let panel = app.active_panel_mut();
    if let Some(parent) = panel.cwd.parent().map(|p| p.to_path_buf()) {
        if parent != panel.cwd {
            // Midnight-Commander behavior: land the cursor back on the
            // directory we just came from, not at the top of the listing.
            let left_name = panel.cwd.file_name().map(str::to_string);
            panel.cwd = parent;
            panel.cursor = 0;
            let registry = Arc::clone(&app.registry);
            reload(&registry, app.active_panel_mut());
            if let Some(name) = left_name {
                app.active_panel_mut().select_by_name(&name);
            }
        }
    }
}

fn start_open_archive(app: &mut App, location: Location) {
    let target_left = app.active == PaneSide::Left;
    let registry = Arc::clone(&app.registry);
    let job_location = location.clone();
    let job_id = app.jobs.spawn("Open archive", move |_cancel, report| {
        report(Default::default());
        registry.open_archive_and_cache(&job_location).map_err(Into::into)
    });
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Opening archive".into(),
        progress: Default::default(),
        kind: JobKind::OpenArchive { target_left, location },
    });
}

fn start_copy(app: &mut App) {
    let items = selected_items(app.active_panel());
    if items.is_empty() {
        return;
    }
    let dest_default = app.inactive_panel().cwd.clone();
    app.mode = Mode::TextInput(TextInputPrompt::new(
        "Copy to:",
        dest_default.to_string(),
        TextInputPurpose::CopyToDir { items },
    ));
}

fn start_move(app: &mut App) {
    let panel = app.active_panel();
    if !panel.selected.is_empty() {
        let items: Vec<VfsPath> = panel.selected.iter().cloned().collect();
        let dest_default = app.inactive_panel().cwd.clone();
        app.mode = Mode::TextInput(TextInputPrompt::new(
            "Move to:",
            dest_default.to_string(),
            TextInputPurpose::MoveToDir { items },
        ));
    } else if let Some(entry) = panel.cursor_entry() {
        let item = entry.path.clone();
        let prefill = panel.cwd.join(&entry.name);
        app.mode = Mode::TextInput(TextInputPrompt::new(
            "Rename/move to:",
            prefill.to_string(),
            TextInputPurpose::RenameOrMove { item },
        ));
    }
}

fn start_edit(app: &mut App) {
    let panel = app.active_panel();
    if let Some(entry) = panel.cursor_entry() {
        if !entry.kind.is_dir() {
            app.edit_request = Some(EditTarget {
                location: panel.location.clone(),
                path: entry.path.clone(),
            });
        }
    }
}

fn start_mkdir(app: &mut App) {
    app.mode = Mode::TextInput(TextInputPrompt::new(
        "New directory name:",
        "",
        TextInputPurpose::Mkdir,
    ));
}

fn start_delete(app: &mut App) {
    let items = selected_items(app.active_panel());
    if items.is_empty() {
        return;
    }
    if !app.config.general.confirm_delete {
        spawn_delete(app, items);
        return;
    }
    let message = if items.len() == 1 {
        format!("Delete '{}'?", items[0])
    } else {
        format!("Delete {} items?", items.len())
    };
    app.mode = Mode::Confirm(ConfirmDialog {
        message,
        purpose: ConfirmPurpose::Delete { items },
    });
}

fn spawn_delete(app: &mut App, items: Vec<VfsPath>) {
    let location = app.active_panel().location.clone();
    let registry = Arc::clone(&app.registry);
    let job_id = app.jobs.spawn("Delete", move |cancel, report| {
        let vfs = registry.resolve(&location)?;
        job::delete_job(vfs.as_ref(), &items, cancel, report)
    });
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Deleting".into(),
        progress: Default::default(),
        kind: JobKind::FileOp,
    });
}

fn start_connect(app: &mut App) {
    let default_username = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
    let saved = app
        .config
        .profiles
        .iter()
        .map(|p| SavedProfileSummary {
            id: p.id.clone(),
            host: p.host.clone(),
            port: p.port,
            username: p.username.clone(),
        })
        .collect();
    app.mode = Mode::Connect(ConnectForm::new(default_username, saved));
}

/// Starts a directory sync of the whole current directories of both panels
/// (active = source, inactive = destination) — `rsync src/ dst/`
/// semantics, not the per-selected-item semantics of copy/move/delete.
/// Only scans and builds a plan here; nothing is copied or deleted until
/// the user confirms the summary (see `JobKind::ScanSync`'s handling in
/// `handle_job_event`).
fn start_sync(app: &mut App) {
    let src_location = app.active_panel().location.clone();
    let src_root = app.active_panel().cwd.clone();
    let dst_location = app.inactive_panel().location.clone();
    let dst_root = app.inactive_panel().cwd.clone();
    let delete_extraneous = app.config.general.sync_delete_extraneous;
    let registry = Arc::clone(&app.registry);

    let (src, dst) = match (registry.resolve(&src_location), registry.resolve(&dst_location)) {
        (Ok(s), Ok(d)) => (s, d),
        (Err(e), _) | (_, Err(e)) => {
            app.status = Some(format!("sync failed: {e}"));
            return;
        }
    };

    let plan_cell = SyncPlanCell::new();
    let plan_cell_for_job = plan_cell.clone();
    let job_id = app.jobs.spawn("Scan sync", move |cancel, report| {
        let plan =
            job::build_sync_plan(src.as_ref(), dst.as_ref(), &src_root, &dst_root, delete_extraneous, cancel, report)?;
        plan_cell_for_job.set(plan);
        Ok(())
    });
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Scanning for sync".into(),
        progress: Default::default(),
        kind: JobKind::ScanSync { src_location, dst_location, plan_cell },
    });
}

fn spawn_execute_sync(app: &mut App, plan: pfnc_core::SyncPlan, src_location: Location, dst_location: Location) {
    let registry = Arc::clone(&app.registry);
    let job_id = app.jobs.spawn("Sync", move |cancel, report| {
        let src = registry.resolve(&src_location)?;
        let dst = registry.resolve(&dst_location)?;
        job::execute_sync_plan(src.as_ref(), dst.as_ref(), &src_location, &dst_location, &plan, cancel, report)
    });
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Syncing".into(),
        progress: Default::default(),
        kind: JobKind::ExecuteSync,
    });
}

fn handle_confirm_key(app: &mut App, dialog: ConfirmDialog, key: KeyCode) {
    match key {
        KeyCode::Char('y') | KeyCode::Enter => match dialog.purpose {
            ConfirmPurpose::Delete { items } => spawn_delete(app, items),
            ConfirmPurpose::Sync { plan, src_location, dst_location } => {
                spawn_execute_sync(app, plan, src_location, dst_location)
            }
        },
        KeyCode::Char('n') | KeyCode::Esc => app.mode = Mode::Browsing,
        _ => app.mode = Mode::Confirm(dialog),
    }
}

fn handle_text_input_key(app: &mut App, mut prompt: TextInputPrompt, key: KeyCode) {
    match key {
        KeyCode::Enter => submit_text_input(app, prompt),
        KeyCode::Esc => app.mode = Mode::Browsing,
        KeyCode::Backspace => {
            prompt.text.backspace();
            app.mode = Mode::TextInput(prompt);
        }
        KeyCode::Left => {
            prompt.text.move_left();
            app.mode = Mode::TextInput(prompt);
        }
        KeyCode::Right => {
            prompt.text.move_right();
            app.mode = Mode::TextInput(prompt);
        }
        KeyCode::Home => {
            prompt.text.move_home();
            app.mode = Mode::TextInput(prompt);
        }
        KeyCode::End => {
            prompt.text.move_end();
            app.mode = Mode::TextInput(prompt);
        }
        KeyCode::Char(c) => {
            prompt.text.insert_char(c);
            app.mode = Mode::TextInput(prompt);
        }
        _ => app.mode = Mode::TextInput(prompt),
    }
}

fn submit_text_input(app: &mut App, prompt: TextInputPrompt) {
    let value = prompt.text.value.trim().to_string();
    if value.is_empty() {
        app.mode = Mode::Browsing;
        return;
    }

    match prompt.purpose {
        TextInputPurpose::Mkdir => {
            let path = app.active_panel().cwd.join(&value);
            let location = app.active_panel().location.clone();
            match app.registry.resolve(&location).and_then(|vfs| vfs.mkdir(&path, None)) {
                Ok(()) => app.status = None,
                Err(e) => app.status = Some(format!("mkdir failed: {e}")),
            }
            let registry = Arc::clone(&app.registry);
            reload(&registry, app.active_panel_mut());
            app.mode = Mode::Browsing;
        }
        TextInputPurpose::CopyToDir { items } => {
            let dest_dir = VfsPath::from(value);
            spawn_copy(app, items, dest_dir);
        }
        TextInputPurpose::MoveToDir { items } => {
            let dest_dir = VfsPath::from(value);
            spawn_move_to_dir(app, items, dest_dir);
        }
        TextInputPurpose::RenameOrMove { item } => {
            let dest = VfsPath::from(value);
            let location = app.active_panel().location.clone();
            spawn_move_same_backend(app, location, vec![(item, dest)]);
        }
    }
}

fn spawn_copy(app: &mut App, items: Vec<VfsPath>, dest_dir: VfsPath) {
    let src_location = app.active_panel().location.clone();
    let dst_location = app.inactive_panel().location.clone();
    let registry = Arc::clone(&app.registry);

    let (src, dst) = match (registry.resolve(&src_location), registry.resolve(&dst_location)) {
        (Ok(s), Ok(d)) => (s, d),
        (Err(e), _) | (_, Err(e)) => {
            app.status = Some(format!("copy failed: {e}"));
            app.mode = Mode::Browsing;
            return;
        }
    };

    let job_id = app.jobs.spawn("Copy", move |cancel, report| {
        job::copy_job(src.as_ref(), dst.as_ref(), &src_location, &dst_location, &items, &dest_dir, cancel, report)
    });
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Copying".into(),
        progress: Default::default(),
        kind: JobKind::FileOp,
    });
}

/// A move where source and destination are both within `location` (same
/// backend instance) — the fast `rename` path.
fn spawn_move_same_backend(app: &mut App, location: Location, pairs: Vec<(VfsPath, VfsPath)>) {
    let registry = Arc::clone(&app.registry);
    let job_id = app.jobs.spawn("Move", move |cancel, report| {
        let vfs = registry.resolve(&location)?;
        job::move_job(vfs.as_ref(), &pairs, cancel, report)
    });
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Moving".into(),
        progress: Default::default(),
        kind: JobKind::FileOp,
    });
}

/// A move into `dest_dir` on the *other* panel, which may or may not share
/// a backend with the active panel — falls back to copy+delete when it
/// doesn't (rename can't cross backends/connections).
fn spawn_move_to_dir(app: &mut App, items: Vec<VfsPath>, dest_dir: VfsPath) {
    let src_location = app.active_panel().location.clone();
    let dst_location = app.inactive_panel().location.clone();

    if src_location == dst_location {
        let pairs: Vec<(VfsPath, VfsPath)> = items
            .iter()
            .filter_map(|item| item.file_name().map(|name| (item.clone(), dest_dir.join(name))))
            .collect();
        spawn_move_same_backend(app, src_location, pairs);
        return;
    }

    let registry = Arc::clone(&app.registry);
    let (src, dst) = match (registry.resolve(&src_location), registry.resolve(&dst_location)) {
        (Ok(s), Ok(d)) => (s, d),
        (Err(e), _) | (_, Err(e)) => {
            app.status = Some(format!("move failed: {e}"));
            app.mode = Mode::Browsing;
            return;
        }
    };
    let job_id = app.jobs.spawn("Move", move |cancel, report| {
        job::move_cross_backend(
            src.as_ref(),
            dst.as_ref(),
            &src_location,
            &dst_location,
            &items,
            &dest_dir,
            cancel,
            report,
        )
    });
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Moving".into(),
        progress: Default::default(),
        kind: JobKind::FileOp,
    });
}

fn handle_connect_key(app: &mut App, mut form: ConnectForm, key: KeyCode) {
    // F1..F9 fill the form from a saved profile (never the password) —
    // deliberately F-keys, not digit keys, so typing a port number or a
    // numeric username still works normally.
    if let KeyCode::F(n) = key {
        if let Some(profile) = (n as usize).checked_sub(1).and_then(|i| form.saved.get(i)).cloned() {
            form.apply_saved(&profile);
            app.mode = Mode::Connect(form);
            return;
        }
    }

    match key {
        KeyCode::Esc => app.mode = Mode::Browsing,
        KeyCode::Enter => submit_connect(app, form),
        KeyCode::Tab | KeyCode::Down => {
            form.next_field();
            app.mode = Mode::Connect(form);
        }
        KeyCode::Up => {
            form.prev_field();
            app.mode = Mode::Connect(form);
        }
        KeyCode::Backspace => {
            form.focused_mut().text.backspace();
            app.mode = Mode::Connect(form);
        }
        KeyCode::Left => {
            form.focused_mut().text.move_left();
            app.mode = Mode::Connect(form);
        }
        KeyCode::Right => {
            form.focused_mut().text.move_right();
            app.mode = Mode::Connect(form);
        }
        KeyCode::Home => {
            form.focused_mut().text.move_home();
            app.mode = Mode::Connect(form);
        }
        KeyCode::End => {
            form.focused_mut().text.move_end();
            app.mode = Mode::Connect(form);
        }
        KeyCode::Char(c) => {
            form.focused_mut().text.insert_char(c);
            app.mode = Mode::Connect(form);
        }
        _ => app.mode = Mode::Connect(form),
    }
}

fn submit_connect(app: &mut App, mut form: ConnectForm) {
    let host = form.host().trim().to_string();
    if host.is_empty() {
        form.error = Some("Host is required".to_string());
        app.mode = Mode::Connect(form);
        return;
    }
    let port: u16 = match form.port().trim().parse() {
        Ok(p) => p,
        Err(_) => {
            form.error = Some("Port must be a number".to_string());
            app.mode = Mode::Connect(form);
            return;
        }
    };
    let username = form.username().trim().to_string();
    let password = form.password().to_string();

    let profile_id = format!("{username}@{host}:{port}");
    let saved_summary = SavedProfileSummary {
        id: profile_id.clone(),
        host: host.clone(),
        port,
        username: username.clone(),
    };
    let profile = ConnectionProfile {
        id: profile_id,
        host,
        port,
        username,
        auth: AuthMethod::Auto {
            password: if password.is_empty() { None } else { Some(password) },
        },
    };

    let target_left = app.active == PaneSide::Left;
    let registry = Arc::clone(&app.registry);
    let job_id = app.jobs.spawn("Connect", move |_cancel, report| {
        report(Default::default());
        registry
            .connect_and_cache(&profile)
            .map_err(|e| pfnc_core::VfsError::ConnectionLost(e.to_string()).into())
    });
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Connecting".into(),
        progress: Default::default(),
        kind: JobKind::Connect { target_left, profile: saved_summary },
    });
}

fn handle_progress_key(app: &mut App, state: ProgressState, key: KeyCode) {
    if matches!(key, KeyCode::Esc) {
        app.jobs.cancel(state.job_id);
    }
    app.mode = Mode::Progress(state);
}

pub fn handle_job_event(app: &mut App, event: JobEvent) {
    match event {
        JobEvent::Progress { job_id, progress } => {
            if let Mode::Progress(state) = &mut app.mode {
                if state.job_id == job_id {
                    state.progress = progress;
                }
            }
        }
        JobEvent::Finished { job_id, outcome } => {
            app.jobs.reap(job_id);
            let current = match &app.mode {
                Mode::Progress(state) if state.job_id == job_id => Some(state.kind.clone()),
                _ => None,
            };
            let Some(kind) = current else {
                return;
            };
            app.mode = Mode::Browsing;

            app.status = match &outcome {
                JobOutcome::Completed => None,
                JobOutcome::Cancelled => Some("Cancelled".to_string()),
                JobOutcome::Failed(e) => Some(format!("Job failed: {e}")),
            };

            match kind {
                JobKind::FileOp => app.reload_both(),
                JobKind::Connect { target_left, profile } => {
                    if matches!(outcome, JobOutcome::Completed) {
                        // Only mutated in memory here; `main.rs` persists
                        // `app.config` to disk once on exit. Keeps
                        // "successful connect" free of a real filesystem
                        // write, which matters for tests exercising this
                        // path against an in-memory `App`.
                        app.config.upsert_profile(pfnc_config::ConnectionProfileConfig {
                            id: profile.id.clone(),
                            host: profile.host,
                            port: profile.port,
                            username: profile.username,
                        });

                        let panel = if target_left { &mut app.left } else { &mut app.right };
                        panel.location = Location::Remote { profile_id: profile.id };
                        panel.cwd = VfsPath::from("/");
                        panel.cursor = 0;
                        reload(&app.registry, panel);
                    }
                }
                JobKind::OpenArchive { target_left, location } => {
                    if matches!(outcome, JobOutcome::Completed) {
                        let panel = if target_left { &mut app.left } else { &mut app.right };
                        panel.location = location;
                        panel.cwd = VfsPath::from("/");
                        panel.cursor = 0;
                        reload(&app.registry, panel);
                    }
                }
                JobKind::ScanSync { src_location, dst_location, plan_cell } => {
                    if !matches!(outcome, JobOutcome::Completed) {
                        return;
                    }
                    let Some(plan) = plan_cell.take() else {
                        app.status = Some("sync scan produced no plan".to_string());
                        return;
                    };
                    if plan.is_empty() {
                        app.status = Some("Nothing to sync".to_string());
                        return;
                    }
                    let mut message = format!("{} to copy ({})", plan.files_to_copy, format_bytes(plan.bytes_total));
                    if plan.items_to_delete > 0 {
                        message.push_str(&format!(", {} to delete", plan.items_to_delete));
                    }
                    message.push_str(" — proceed?");
                    app.mode = Mode::Confirm(ConfirmDialog {
                        message,
                        purpose: ConfirmPurpose::Sync { plan, src_location, dst_location },
                    });
                }
                JobKind::ExecuteSync => app.reload_both(),
            }
        }
    }
}

/// Human-readable byte count for the sync confirmation summary (e.g.
/// `"3.4 MB"`).
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
