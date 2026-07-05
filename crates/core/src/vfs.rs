//! The `Vfs` trait and the types panels use to talk to it, uniformly,
//! regardless of whether a location is local, remote (SFTP), or inside an
//! archive.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::SystemTime;

use camino::Utf8PathBuf;
use thiserror::Error;

use crate::transport::RemoteFileAgent;

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

/// Diagnostic info about a connection, for display purposes only (e.g. the
/// F1 Help box) — never consulted by any transfer-path decision, so it's
/// safe to construct from whatever's already cached without triggering new
/// network I/O. Plain enough types (`String`/`u16`/`u32`) that `pfnc-core`
/// still doesn't need to depend on `quinn`/`ssh2` to define this.
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    /// The protocol actually moving directory listings and most file
    /// operations for this connection, e.g. `"SFTP"`.
    pub protocol: &'static str,
    /// The remote host's OS (e.g. `Some("Linux")`), from a `uname -s`
    /// probe — `None` if that probe hasn't completed yet or failed.
    pub remote_os: Option<String>,
    /// Present only when a QUIC fast-path agent is actually connected.
    pub quic: Option<QuicConnectionInfo>,
}

/// Details about an active QUIC fast-path connection, for display only.
#[derive(Clone, Debug)]
pub struct QuicConnectionInfo {
    /// This process's own local UDP port for the connection, when known.
    pub local_port: Option<u16>,
    /// The port the remote `pfnc-agent` is listening on.
    pub remote_port: u16,
    /// PID of the `pfnc-agent` process listening on `remote_port` — "who
    /// is listening" on the remote host.
    pub agent_pid: u32,
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

    /// A cheap content hash (XXH64) for `path`, used by directory sync to
    /// tell identical files apart from changed ones without relying solely
    /// on mtime. Returns `Ok(None)` when a fast hash isn't available here
    /// (the default for any backend that doesn't override this) — callers
    /// must fall back to a size/mtime comparison in that case, never treat
    /// `None` as an error.
    ///
    /// Implementations must never make this *more* expensive than just
    /// copying the file would be: computing a hash by streaming the whole
    /// file across a slow link defeats the purpose of a "quick" check.
    /// `LocalFs` hashes in-process (free); `SftpFs` only returns `Some` when
    /// it can compute the hash *on the remote host* via an exec channel.
    fn quick_hash(&self, _path: &VfsPath) -> VfsResult<Option<u64>> {
        Ok(None)
    }

    /// This backend's own fast whole-file read/write channel, when it has
    /// one (e.g. an already-deployed QUIC agent for `SftpFs`) — used by
    /// `negotiate_transport` to pick a faster `Transport` than the generic
    /// `Vfs`-stream copy. `None` (the default) means no such channel exists;
    /// callers must fall back to the generic stream transport in that case,
    /// never treat `None` as an error.
    fn fast_transport(&self) -> Option<Arc<dyn RemoteFileAgent>> {
        None
    }

    /// Diagnostic info about this connection for display purposes (e.g.
    /// the F1 Help box). `None` (the default) means nothing interesting to
    /// show — used by `LocalFs`/`ArchiveFs`. Implementations must only
    /// report already-cached facts, never probe fresh over the network:
    /// this can be called from UI-thread rendering code.
    fn connection_info(&self) -> Option<ConnectionInfo> {
        None
    }
}
