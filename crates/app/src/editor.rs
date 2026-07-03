//! F4 "Edit": suspends the TUI, runs an external editor on the cursor
//! file, and resumes. Local files are edited in place; remote (SFTP)
//! files are downloaded to a temp file first and only uploaded back if
//! their content actually changed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use pfnc_core::{Location, Vfs, VfsPath};

use crate::app::App;

/// Editors tried in order — the first one found on `PATH` wins.
const EDITOR_CANDIDATES: [&str; 3] = ["nano", "pico", "vi"];

/// Lets the background input-reader thread be told "don't consume
/// keystrokes right now". Without this, while an external editor has been
/// handed the terminal, our own input thread would still be reading from
/// the same terminal and would race the child process for keystrokes —
/// the editor would then appear to randomly drop input.
#[derive(Clone)]
pub struct InputGate(Arc<AtomicBool>);

impl InputGate {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn is_paused(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn pause(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn resume(&self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Default for InputGate {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct EditTarget {
    pub location: Location,
    pub path: VfsPath,
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn find_editor() -> Option<PathBuf> {
    EDITOR_CANDIDATES.iter().find_map(|name| which(name))
}

fn fs_snapshot(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn download_to_temp(vfs: &dyn Vfs, remote_path: &VfsPath) -> anyhow::Result<tempfile::TempPath> {
    let suffix = remote_path.extension().map(|e| format!(".{e}")).unwrap_or_default();
    let mut named = tempfile::Builder::new().suffix(&suffix).tempfile()?;
    let mut reader = vfs.open_read(remote_path)?;
    std::io::copy(&mut reader, named.as_file_mut())?;
    Ok(named.into_temp_path())
}

fn upload_temp(vfs: &dyn Vfs, local_path: &Path, remote_path: &VfsPath) -> anyhow::Result<()> {
    let mut reader = std::fs::File::open(local_path)?;
    let mut writer = vfs.create_write(remote_path, None)?;
    std::io::copy(&mut reader, &mut writer)?;
    Ok(())
}

/// Runs the whole F4 flow: find an editor, suspend the TUI, edit (fetching
/// to / pushing from a temp file for non-local backends), resume, reload.
pub fn edit_file(terminal: &mut ratatui::DefaultTerminal, app: &mut App, target: EditTarget, gate: &InputGate) {
    let Some(editor) = find_editor() else {
        app.status = Some("no editor found on PATH (tried nano, pico, vi)".to_string());
        return;
    };

    let vfs = match app.registry.resolve(&target.location) {
        Ok(v) => v,
        Err(e) => {
            app.status = Some(format!("edit failed: {e}"));
            return;
        }
    };

    let is_local = matches!(target.location, Location::Local);
    let (edit_path, _temp_guard) = if is_local {
        (target.path.as_std_path().to_path_buf(), None)
    } else {
        match download_to_temp(vfs.as_ref(), &target.path) {
            Ok(temp) => {
                let path = temp.to_path_buf();
                (path, Some(temp))
            }
            Err(e) => {
                app.status = Some(format!("could not fetch file for editing: {e}"));
                return;
            }
        }
    };

    let before = fs_snapshot(&edit_path);

    // Stop the input thread from reading before we hand the terminal to
    // the child process, and give any poll() call already in flight a
    // moment to return so it observes the pause before the next read.
    gate.pause();
    std::thread::sleep(Duration::from_millis(80));

    ratatui::restore();
    let spawn_result = std::process::Command::new(&editor).arg(&edit_path).status();
    *terminal = ratatui::init();
    let _ = terminal.clear();

    gate.resume();

    if let Err(e) = spawn_result {
        app.status = Some(format!("failed to launch {}: {e}", editor.display()));
        return;
    }

    if !is_local && fs_snapshot(&edit_path) != before {
        if let Err(e) = upload_temp(vfs.as_ref(), &edit_path, &target.path) {
            app.status = Some(format!("failed to save changes back: {e}"));
        }
    }

    app.reload_both();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_editor_returns_an_executable_when_one_exists() {
        // At least one of nano/pico/vi is essentially always present on a
        // real Linux dev box; this environment has `vi`. If none are
        // installed, `find_editor` should return `None` rather than panic.
        if let Some(path) = find_editor() {
            assert!(is_executable(&path));
        }
    }

    #[test]
    fn which_finds_a_known_coreutil_on_path() {
        assert!(which("ls").is_some());
    }

    #[test]
    fn which_returns_none_for_a_bogus_name() {
        assert!(which("definitely-not-a-real-binary-name-xyz").is_none());
    }
}
