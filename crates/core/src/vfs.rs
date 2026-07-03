//! The `Vfs` trait and the types panels use to talk to it, uniformly,
//! regardless of whether a location is local, remote (SFTP), or inside an
//! archive.

use std::io::{Read, Write};
use std::time::SystemTime;

use camino::Utf8PathBuf;
use thiserror::Error;

/// A path within a single backend's namespace. Always UTF-8.
///
/// Restricting to valid UTF-8 paths is a deliberate Phase 1 simplification:
/// real Unix paths can contain arbitrary bytes, but supporting that fully
/// complicates every layer of the app for a rare edge case. Non-UTF8
/// filenames are skipped (with a warning) rather than causing a crash or a
/// failed listing.
pub type VfsPath = Utf8PathBuf;

/// Identifies a saved or ad-hoc SSH connection. Opaque for now; profile
/// storage is introduced in milestone M6.
pub type ProfileId = String;

/// Identifies *where* a path lives. Cheap to clone; used as a panel's
/// location and as a connection-cache key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    Local,
    Remote {
        profile_id: ProfileId,
    },
    Archive {
        base: Box<Location>,
        archive_path: VfsPath,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink { target: Option<VfsPath> },
    Other,
}

impl EntryKind {
    pub fn is_dir(&self) -> bool {
        matches!(self, EntryKind::Dir)
    }
}

#[derive(Clone, Debug)]
pub struct EntryMeta {
    pub name: String,
    pub path: VfsPath,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Unix permission bits (e.g. `0o755`), when the backend has a concept
    /// of them.
    pub permissions: Option<u32>,
    pub owner: Option<String>,
    pub group: Option<String>,
}

#[derive(Debug, Error)]
pub enum VfsError {
    #[error("not found: {0}")]
    NotFound(VfsPath),
    #[error("permission denied: {0}")]
    PermissionDenied(VfsPath),
    #[error("already exists: {0}")]
    AlreadyExists(VfsPath),
    #[error("connection lost: {0}")]
    ConnectionLost(String),
    #[error("unsupported operation: {0}")]
    Unsupported(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type VfsResult<T> = Result<T, VfsError>;

/// Flags so the UI can grey out actions a given backend can't perform
/// (e.g. `ArchiveFs` is read-only).
#[derive(Clone, Copy, Debug)]
pub struct VfsCapabilities {
    pub can_write: bool,
    pub can_set_permissions: bool,
    pub can_symlink: bool,
    pub can_rename: bool,
}

/// A backend implementing filesystem-like operations. Implemented once per
/// backend (`LocalFs`, `SftpFs`, `ArchiveFs`); panels only ever talk to a
/// `dyn Vfs`, never to a concrete backend type.
pub trait Vfs: Send + Sync {
    fn list_dir(&self, path: &VfsPath) -> VfsResult<Vec<EntryMeta>>;
    fn stat(&self, path: &VfsPath) -> VfsResult<EntryMeta>;

    /// Streaming read, so large files and slow links don't require
    /// buffering the whole file in memory.
    fn open_read(&self, path: &VfsPath) -> VfsResult<Box<dyn Read + Send>>;

    /// Streaming create/truncate-write.
    fn create_write(&self, path: &VfsPath, mode: Option<u32>) -> VfsResult<Box<dyn Write + Send>>;

    fn mkdir(&self, path: &VfsPath, mode: Option<u32>) -> VfsResult<()>;
    fn remove_file(&self, path: &VfsPath) -> VfsResult<()>;
    fn remove_dir(&self, path: &VfsPath, recursive: bool) -> VfsResult<()>;
    fn rename(&self, from: &VfsPath, to: &VfsPath) -> VfsResult<()>;
    fn set_permissions(&self, path: &VfsPath, mode: u32) -> VfsResult<()>;
    fn symlink(&self, target: &VfsPath, link: &VfsPath) -> VfsResult<()>;

    fn capabilities(&self) -> VfsCapabilities;

    /// The root path within this backend's namespace ("/" for local/SFTP;
    /// archive-internal root for `ArchiveFs`).
    fn root(&self) -> VfsPath;
}
