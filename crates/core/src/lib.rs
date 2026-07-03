//! Backend-agnostic types shared across the workspace: the `Vfs` trait,
//! `Location`, `EntryMeta`, error types, panel state, the job model, and
//! dialog (`Mode`) state.

pub mod job;
pub mod mode;
pub mod panel;
pub mod vfs;

pub use job::{CancellationToken, JobError, JobEvent, JobId, JobManager, JobOutcome, JobProgress};
pub use mode::{
    ConfirmDialog, ConnectField, ConnectForm, EditableText, JobKind, Mode, ProgressState, SavedProfileSummary,
    TextInputPrompt, TextInputPurpose,
};
pub use panel::PanelState;
pub use vfs::{EntryKind, EntryMeta, Location, ProfileId, Vfs, VfsCapabilities, VfsError, VfsPath, VfsResult};
