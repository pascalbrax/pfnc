//! Backend-agnostic types shared across the workspace: the `Vfs` trait,
//! `Location`, `EntryMeta`, error types, panel state, the job model, and
//! dialog (`Mode`) state.

pub mod job;
pub mod mode;
pub mod panel;
pub mod transport;
pub mod vfs;

pub use job::{
    CancellationToken, JobError, JobEvent, JobId, JobManager, JobOutcome, JobProgress, SyncCopyItem, SyncPlan,
};
pub use mode::{
    ConfirmDialog, ConfirmPurpose, ConnectField, ConnectForm, EditableText, JobKind, Mode, ProgressState,
    SavedProfileSummary, SyncPlanCell, TextInputPrompt, TextInputPurpose,
};
pub use panel::PanelState;
pub use transport::{negotiate_transport, RemoteFileAgent, Transport, VfsStreamTransport};
pub use vfs::{
    ConnectionInfo, EntryKind, EntryMeta, Location, ProfileId, QuicConnectionInfo, Vfs, VfsCapabilities, VfsError,
    VfsPath, VfsResult,
};
