//! `SftpFs`: a `pfnc_core::Vfs` implementation backed by `ssh2` (libssh2).
//! libssh2 is a two-decade-old, extremely battle-tested C library, chosen
//! over pure-Rust alternatives specifically for compatibility with the
//! widest range of real-world SSH servers.

mod auth;
mod hostkey;

pub use auth::AuthMethod;
pub use hostkey::{default_known_hosts_path, AcceptNewPolicy, HostKeyPolicy, RejectUnknownPolicy};

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use camino::Utf8PathBuf;
use pfnc_core::{EntryKind, EntryMeta, ProfileId, Vfs, VfsCapabilities, VfsError, VfsPath, VfsResult};
use ssh2::{OpenFlags, OpenType, Session, Sftp};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct ConnectionProfile {
    pub id: ProfileId,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("tcp connect to {0} failed: {1}")]
    Tcp(String, std::io::Error),
    #[error("ssh handshake failed: {0}")]
    Handshake(ssh2::Error),
    #[error("host key verification failed: {0}")]
    HostKey(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("sftp subsystem init failed: {0}")]
    SftpInit(ssh2::Error),
}

pub struct SftpFs {
    sftp: Sftp,
}

impl std::fmt::Debug for SftpFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpFs").finish_non_exhaustive()
    }
}

impl SftpFs {
    /// Connects, verifies the host key via `policy`, authenticates, and
    /// opens the SFTP subsystem — everything needed before the returned
    /// `SftpFs` can serve `Vfs` calls. Blocking; callers run this on a
    /// background job thread rather than the UI thread.
    pub fn connect(
        profile: &ConnectionProfile,
        policy: &dyn HostKeyPolicy,
        known_hosts_path: &Path,
    ) -> Result<Self, ConnectError> {
        let addr = format!("{}:{}", profile.host, profile.port);
        let tcp = TcpStream::connect(&addr).map_err(|e| ConnectError::Tcp(addr, e))?;
        tcp.set_nodelay(true).ok();

        let mut session = Session::new().map_err(ConnectError::Handshake)?;
        session.set_tcp_stream(tcp);
        session.set_timeout(15_000);
        session.handshake().map_err(ConnectError::Handshake)?;

        hostkey::verify_host_key(&session, &profile.host, profile.port, policy, known_hosts_path)
            .map_err(ConnectError::HostKey)?;

        auth::authenticate(&session, &profile.username, &profile.auth).map_err(ConnectError::Auth)?;
        if !session.authenticated() {
            return Err(ConnectError::Auth("server did not confirm authentication".to_string()));
        }

        let sftp = session.sftp().map_err(ConnectError::SftpInit)?;
        Ok(Self { sftp })
    }
}

fn to_vfs_path(p: &std::path::Path) -> Option<VfsPath> {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).ok()
}

fn map_ssh_err(e: ssh2::Error, path: &VfsPath) -> VfsError {
    let io_err: std::io::Error = e.into();
    match io_err.kind() {
        std::io::ErrorKind::NotFound => VfsError::NotFound(path.clone()),
        std::io::ErrorKind::AlreadyExists => VfsError::AlreadyExists(path.clone()),
        _ => VfsError::Io(io_err),
    }
}

fn entry_meta_from_stat(path: VfsPath, name: String, stat: &ssh2::FileStat) -> EntryMeta {
    let kind = if stat.is_dir() {
        EntryKind::Dir
    } else if stat.file_type() == ssh2::FileType::Symlink {
        EntryKind::Symlink { target: None } // filled in by the caller when available
    } else if stat.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };

    EntryMeta {
        name,
        path,
        kind,
        size: stat.size.unwrap_or(0),
        modified: stat.mtime.map(|t| UNIX_EPOCH + Duration::from_secs(t)),
        permissions: stat.perm.map(|p| p & 0o7777),
        // A uid/gid -> name lookup would need an extra round trip (SFTP has
        // no getpwuid); deferred past Phase 1, same as LocalFs.
        owner: None,
        group: None,
    }
}

impl Vfs for SftpFs {
    fn list_dir(&self, path: &VfsPath) -> VfsResult<Vec<EntryMeta>> {
        let entries = self.sftp.readdir(path.as_std_path()).map_err(|e| map_ssh_err(e, path))?;
        let mut out = Vec::with_capacity(entries.len());
        for (full_path, stat) in entries {
            let Some(full) = to_vfs_path(&full_path) else {
                tracing::warn!(%path, "skipping non-UTF8 remote filename");
                continue;
            };
            let name = full.file_name().unwrap_or_default().to_string();
            let mut meta = entry_meta_from_stat(full.clone(), name, &stat);
            if let EntryKind::Symlink { .. } = meta.kind {
                let target = self
                    .sftp
                    .readlink(full.as_std_path())
                    .ok()
                    .and_then(|t| to_vfs_path(&t));
                meta.kind = EntryKind::Symlink { target };
            }
            out.push(meta);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn stat(&self, path: &VfsPath) -> VfsResult<EntryMeta> {
        let stat = self.sftp.lstat(path.as_std_path()).map_err(|e| map_ssh_err(e, path))?;
        let name = path.file_name().unwrap_or("/").to_string();
        let mut meta = entry_meta_from_stat(path.clone(), name, &stat);
        if let EntryKind::Symlink { .. } = meta.kind {
            let target = self.sftp.readlink(path.as_std_path()).ok().and_then(|t| to_vfs_path(&t));
            meta.kind = EntryKind::Symlink { target };
        }
        Ok(meta)
    }

    fn open_read(&self, path: &VfsPath) -> VfsResult<Box<dyn Read + Send>> {
        let file = self.sftp.open(path.as_std_path()).map_err(|e| map_ssh_err(e, path))?;
        Ok(Box::new(file))
    }

    fn create_write(&self, path: &VfsPath, mode: Option<u32>) -> VfsResult<Box<dyn Write + Send>> {
        let file = self
            .sftp
            .open_mode(
                path.as_std_path(),
                OpenFlags::WRITE | OpenFlags::TRUNCATE,
                mode.unwrap_or(0o644) as i32,
                OpenType::File,
            )
            .map_err(|e| map_ssh_err(e, path))?;
        Ok(Box::new(file))
    }

    fn mkdir(&self, path: &VfsPath, mode: Option<u32>) -> VfsResult<()> {
        self.sftp
            .mkdir(path.as_std_path(), mode.unwrap_or(0o755) as i32)
            .map_err(|e| map_ssh_err(e, path))
    }

    fn remove_file(&self, path: &VfsPath) -> VfsResult<()> {
        self.sftp.unlink(path.as_std_path()).map_err(|e| map_ssh_err(e, path))
    }

    fn remove_dir(&self, path: &VfsPath, recursive: bool) -> VfsResult<()> {
        if recursive {
            // The SFTP protocol has no recursive-remove primitive; walk and
            // delete children first, same shape as pfnc-core's job-level
            // walk_for_delete but done in a single call for a Vfs backend
            // that (unlike LocalFs) can't just call an OS-provided
            // remove_dir_all.
            for entry in self.list_dir(path)? {
                if entry.kind.is_dir() {
                    self.remove_dir(&entry.path, true)?;
                } else {
                    self.remove_file(&entry.path)?;
                }
            }
        }
        self.sftp.rmdir(path.as_std_path()).map_err(|e| map_ssh_err(e, path))
    }

    fn rename(&self, from: &VfsPath, to: &VfsPath) -> VfsResult<()> {
        self.sftp
            .rename(from.as_std_path(), to.as_std_path(), None)
            .map_err(|e| map_ssh_err(e, from))
    }

    fn set_permissions(&self, path: &VfsPath, mode: u32) -> VfsResult<()> {
        let mut stat = self.sftp.stat(path.as_std_path()).map_err(|e| map_ssh_err(e, path))?;
        stat.perm = Some(mode);
        self.sftp
            .setstat(path.as_std_path(), stat)
            .map_err(|e| map_ssh_err(e, path))
    }

    fn symlink(&self, target: &VfsPath, link: &VfsPath) -> VfsResult<()> {
        // ssh2's `symlink(path, target)` names are, confusingly, the
        // reverse of what they sound like: per its own docs it "creates a
        // symlink at `target` pointing at `path`" — so our `target`
        // (pointed-to content) is its `path`, and our `link` (where the
        // link file lives) is its `target`.
        self.sftp
            .symlink(target.as_std_path(), link.as_std_path())
            .map_err(|e| map_ssh_err(e, link))
    }

    fn capabilities(&self) -> VfsCapabilities {
        VfsCapabilities {
            can_write: true,
            can_set_permissions: true,
            can_symlink: true,
            can_rename: true,
        }
    }

    fn root(&self) -> VfsPath {
        VfsPath::from("/")
    }
}
