//! `Mode` and the dialog state it carries: pure data, mutated by the app
//! crate's input handling and rendered by `pfnc-tui`. Keeping these as plain
//! structs (no behavior beyond small, self-contained text editing) is what
//! lets `pfnc-tui` stay "rendering only".

use std::sync::{Arc, Mutex};

use crate::job::{JobId, JobProgress, SyncPlan};
use crate::vfs::{Location, VfsPath};

#[derive(Clone, Debug, Default)]
pub enum Mode {
    #[default]
    Browsing,
    Confirm(ConfirmDialog),
    TextInput(TextInputPrompt),
    Connect(ConnectForm),
    Progress(ProgressState),
}

#[derive(Clone, Debug)]
pub struct ConfirmDialog {
    pub message: String,
    pub purpose: ConfirmPurpose,
}

/// What happens when the user confirms a `Mode::Confirm` dialog. Mirrors
/// `TextInputPurpose`'s "one mode, many purposes" shape so adding a new
/// kind of confirmation doesn't need a new `Mode` variant.
#[derive(Clone, Debug)]
pub enum ConfirmPurpose {
    Delete {
        items: Vec<VfsPath>,
    },
    /// Approving an already-scanned directory sync: `plan` is exactly what
    /// gets applied (no rescanning on confirm), `src_location`/
    /// `dst_location` identify the endpoints for transport negotiation.
    Sync {
        plan: SyncPlan,
        src_location: Location,
        dst_location: Location,
    },
}

/// A single line of editable text with a char-index cursor. Shared by
/// `TextInputPrompt` (one field) and `ConnectForm` (several fields) so the
/// edit primitives (insert/backspace/cursor movement) live in one place.
#[derive(Clone, Debug, Default)]
pub struct EditableText {
    pub value: String,
    /// Char index (not byte index) of the edit cursor within `value`.
    pub cursor: usize,
}

impl EditableText {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn insert_char(&mut self, c: char) {
        let byte_idx = self.byte_index(self.cursor);
        self.value.insert(byte_idx, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        let len = self.value.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.value.chars().count();
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }
}

#[derive(Clone, Debug)]
pub enum TextInputPurpose {
    Mkdir,
    /// Entered text is treated as a destination *directory* for a copy.
    CopyToDir { items: Vec<VfsPath> },
    /// Entered text is treated as a destination *directory* for a move of
    /// multiple selected items.
    MoveToDir { items: Vec<VfsPath> },
    /// Entered text is the exact destination path, prefilled with the
    /// item's current path — editing just the trailing name renames it in
    /// place, editing the directory portion moves it. Used for a single
    /// unselected cursor item.
    RenameOrMove { item: VfsPath },
}

#[derive(Clone, Debug)]
pub struct TextInputPrompt {
    pub title: String,
    pub text: EditableText,
    pub purpose: TextInputPurpose,
}

impl TextInputPrompt {
    pub fn new(title: impl Into<String>, value: impl Into<String>, purpose: TextInputPurpose) -> Self {
        Self {
            title: title.into(),
            text: EditableText::new(value),
            purpose,
        }
    }
}

/// One field of a `ConnectForm`. `secret` fields (the password) are
/// rendered masked by `pfnc-tui` but stored in plain text here — the value
/// only ever leaves this struct to become part of a one-shot connect
/// attempt, never persisted (see M6 for saved-profile storage, which will
/// exclude passwords).
#[derive(Clone, Debug)]
pub struct ConnectField {
    pub label: &'static str,
    pub text: EditableText,
    pub secret: bool,
}

impl ConnectField {
    fn new(label: &'static str, value: impl Into<String>, secret: bool) -> Self {
        Self {
            label,
            text: EditableText::new(value),
            secret,
        }
    }
}

/// A saved connection, enough to refill the form's host/port/username
/// fields — never a password (`pfnc-config` never persists one either).
#[derive(Clone, Debug)]
pub struct SavedProfileSummary {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// The host/port/username/password connect form. Deliberately simple for
/// Phase 1: no explicit key-file picker UI yet (agent + default `~/.ssh`
/// keys + this password are tried in order via `AuthMethod::Auto`), which
/// covers the large majority of real-world SSH auth setups without needing
/// a mode-switcher widget.
#[derive(Clone, Debug)]
pub struct ConnectForm {
    pub fields: Vec<ConnectField>,
    pub focus: usize,
    pub error: Option<String>,
    /// Selectable via F1..F9 while this form is open (see
    /// `pfnc::actions::handle_connect_key`) — plain digit keys stay
    /// available for typing into the Port field instead.
    pub saved: Vec<SavedProfileSummary>,
}

pub const CONNECT_FIELD_HOST: usize = 0;
pub const CONNECT_FIELD_PORT: usize = 1;
pub const CONNECT_FIELD_USERNAME: usize = 2;
pub const CONNECT_FIELD_PASSWORD: usize = 3;

impl ConnectForm {
    pub fn new(default_username: impl Into<String>, saved: Vec<SavedProfileSummary>) -> Self {
        Self {
            fields: vec![
                ConnectField::new("Host", "", false),
                ConnectField::new("Port", "22", false),
                ConnectField::new("Username", default_username, false),
                ConnectField::new("Password", "", true),
            ],
            focus: CONNECT_FIELD_HOST,
            error: None,
            saved,
        }
    }

    /// Fills the host/port/username fields from a saved profile, leaving
    /// the password field untouched (never saved, always re-entered).
    pub fn apply_saved(&mut self, profile: &SavedProfileSummary) {
        self.fields[CONNECT_FIELD_HOST].text = EditableText::new(profile.host.clone());
        self.fields[CONNECT_FIELD_PORT].text = EditableText::new(profile.port.to_string());
        self.fields[CONNECT_FIELD_USERNAME].text = EditableText::new(profile.username.clone());
    }

    pub fn focused_mut(&mut self) -> &mut ConnectField {
        &mut self.fields[self.focus]
    }

    pub fn next_field(&mut self) {
        self.focus = (self.focus + 1) % self.fields.len();
    }

    pub fn prev_field(&mut self) {
        self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
    }

    pub fn host(&self) -> &str {
        &self.fields[CONNECT_FIELD_HOST].text.value
    }

    pub fn port(&self) -> &str {
        &self.fields[CONNECT_FIELD_PORT].text.value
    }

    pub fn username(&self) -> &str {
        &self.fields[CONNECT_FIELD_USERNAME].text.value
    }

    pub fn password(&self) -> &str {
        &self.fields[CONNECT_FIELD_PASSWORD].text.value
    }
}

/// A one-shot side-channel for a `ScanSync` job to hand its computed
/// `SyncPlan` back to the main thread: `JobOutcome` carries no payload, so
/// the job closure fills this in via `set` right before returning `Ok(())`,
/// and `handle_job_event` drains it via `take` once it sees the job's
/// `Finished` event — the same shape already used by
/// `registry.open_archive_and_cache` (a registry-side cache keyed by
/// `Location`) for `JobKind::OpenArchive`.
#[derive(Clone, Debug, Default)]
pub struct SyncPlanCell(Arc<Mutex<Option<SyncPlan>>>);

impl SyncPlanCell {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, plan: SyncPlan) {
        *self.0.lock().unwrap() = Some(plan);
    }

    /// Takes the plan out, leaving `None` behind. Returns `None` if the job
    /// never called `set` (e.g. it failed before finishing the scan).
    pub fn take(&self) -> Option<SyncPlan> {
        self.0.lock().unwrap().take()
    }
}

/// What a running job (`Mode::Progress`) should do when it finishes,
/// beyond the generic progress-bar bookkeeping every job gets.
#[derive(Clone, Debug)]
pub enum JobKind {
    /// A copy/move/delete: on completion, just reload the affected panels.
    FileOp,
    /// A connect attempt: on success, the panel identified by
    /// `target_left` should switch to `Location::Remote { profile_id }`,
    /// and `profile` should be saved/updated in the persisted profile
    /// list (host/port/username only — never the password).
    Connect { target_left: bool, profile: SavedProfileSummary },
    /// Downloading and indexing an archive so it can be browsed: on
    /// success, the panel identified by `target_left` should switch to
    /// `location` (already the full `Location::Archive { .. }`).
    OpenArchive { target_left: bool, location: Location },
    /// Scanning two directories for `sync`: on success, the plan drained
    /// from `plan_cell` becomes a `Mode::Confirm` summary (or, if it's a
    /// no-op, just a status message) — nothing is copied or deleted yet.
    ScanSync {
        src_location: Location,
        dst_location: Location,
        plan_cell: SyncPlanCell,
    },
    /// Applying an already-approved `SyncPlan`: on completion, reload the
    /// affected panels, same as `FileOp`.
    ExecuteSync,
}

#[derive(Clone, Debug)]
pub struct ProgressState {
    pub job_id: JobId,
    pub title: String,
    pub progress: JobProgress,
    pub kind: JobKind,
}
