//! `SftpFs`: a `pfnc_core::Vfs` implementation backed by `ssh2` (libssh2).
//! libssh2 is a two-decade-old, extremely battle-tested C library, chosen
//! over pure-Rust alternatives specifically for compatibility with the
//! widest range of real-world SSH servers.

mod auth;
mod deploy;
mod hostkey;
mod quic_agent;

pub use auth::AuthMethod;
pub use deploy::{deploy_and_start, kill_remote_process, DeployError, DeployedAgent};
pub use hostkey::{default_known_hosts_path, AcceptNewPolicy, HostKeyPolicy, RejectUnknownPolicy};

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use camino::Utf8PathBuf;
use pfnc_core::{
    ConnectionInfo, EntryKind, EntryMeta, ProfileId, QuicConnectionInfo, RemoteFileAgent, Vfs, VfsCapabilities,
    VfsError, VfsPath, VfsResult,
};
use quic_agent::QuicAgentHandle;
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
    session: Session,
    sftp: Sftp,
    /// The remote host to dial for the QUIC fast path — not otherwise
    /// needed since `session`/`sftp` are already connected, but `quic_agent`
    /// has to know where to send its own, separate UDP connection.
    host: String,
    /// Cached result of the first `quick_hash` attempt on this connection:
    /// `Some(false)` means a prior attempt found no working `xxhsum` on the
    /// remote host (command not found, or the exec channel itself doesn't
    /// work on this server), so later calls skip straight to `Ok(None)`
    /// instead of paying for a failed round trip per file. `None` means not
    /// probed yet; `Some(true)` means it works (kept for clarity, though
    /// nothing currently short-circuits on it — each file still needs its
    /// own hash computed).
    xxhsum_available: OnceLock<bool>,
    /// Cached result of the first `fast_transport` attempt — `None` means
    /// either not probed yet, or a prior attempt failed (unsupported remote
    /// OS, deployment failure, handshake failure); either way, later calls
    /// don't retry a doomed connection once per file. See `quic_agent`.
    quic_agent_cache: OnceLock<Option<Arc<QuicAgentHandle>>>,
    /// Cached `uname -s` probe of the remote host, trimmed — `None` means
    /// either not probed yet or the probe failed. Populated by
    /// `warm_remote_os_probe` (and reused by the QUIC deploy decision, see
    /// `quic_agent.rs`) so it's never fetched more than once per
    /// connection, and exposed for display via `connection_info`.
    remote_os_cache: OnceLock<Option<String>>,
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
        Ok(Self {
            session,
            sftp,
            host: profile.host.clone(),
            xxhsum_available: OnceLock::new(),
            quic_agent_cache: OnceLock::new(),
            remote_os_cache: OnceLock::new(),
        })
    }

    /// Runs `xxhsum -H1` on `path` via an SSH *exec* channel (not the sftp
    /// subsystem), so only a short hex string crosses the wire rather than
    /// the file's content. Returns `Ok(None)` — not an error — when the
    /// command isn't found or exits non-zero (e.g. an ancient/minimal
    /// remote without `xxhsum` installed); a real channel/connection
    /// failure still propagates as a `VfsError`.
    fn exec_quick_hash(&self, path: &VfsPath) -> VfsResult<Option<u64>> {
        let mut channel = self.session.channel_session().map_err(|e| map_ssh_err(e, path))?;
        let command = format!("xxhsum -H1 -- {}", shell_quote(path.as_str()));
        channel.exec(&command).map_err(|e| map_ssh_err(e, path))?;

        let mut stdout = String::new();
        channel.read_to_string(&mut stdout).map_err(VfsError::Io)?;
        channel.wait_close().map_err(|e| map_ssh_err(e, path))?;

        let status = channel.exit_status().map_err(|e| map_ssh_err(e, path))?;
        if status != 0 {
            return Ok(None);
        }

        let hex = stdout.split_whitespace().next().unwrap_or("");
        Ok(u64::from_str_radix(hex, 16).ok())
    }
}

/// Single-quotes `s` for safe interpolation into a remote shell command,
/// escaping embedded single quotes with the standard `'\''` technique.
/// `path` values are attacker-influenced (they come from directory
/// listings), so this must never be skipped.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
        // Directories first (Midnight-Commander-style), alphabetical within
        // each group.
        out.sort_by(|a, b| b.kind.is_dir().cmp(&a.kind.is_dir()).then_with(|| a.name.cmp(&b.name)));
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

    fn quick_hash(&self, path: &VfsPath) -> VfsResult<Option<u64>> {
        if self.xxhsum_available.get() == Some(&false) {
            return Ok(None);
        }
        match self.exec_quick_hash(path)? {
            Some(hash) => {
                let _ = self.xxhsum_available.set(true);
                Ok(Some(hash))
            }
            None => {
                let _ = self.xxhsum_available.set(false);
                Ok(None)
            }
        }
    }

    fn fast_transport(&self) -> Option<Arc<dyn RemoteFileAgent>> {
        self.quic_agent().map(|handle| handle as Arc<dyn RemoteFileAgent>)
    }

    fn connection_info(&self) -> Option<ConnectionInfo> {
        // Only ever peeks already-warmed caches (`.get()`, never
        // `.get_or_init()`) — this must be safe to call from UI-thread
        // rendering code, never trigger a fresh probe or QUIC deployment.
        let remote_os = self.remote_os_cache.get().cloned().flatten();
        let quic = self.quic_agent_cache.get().cloned().flatten().map(|agent| QuicConnectionInfo {
            local_port: agent.local_port(),
            remote_port: agent.remote_port(),
            agent_pid: agent.pid(),
        });
        Some(ConnectionInfo { protocol: "SFTP", remote_os, quic })
    }
}

impl Drop for SftpFs {
    /// If a QUIC agent was successfully deployed on this connection, kill
    /// the remote `pfnc-agent` process before this `SftpFs` (and its exec
    /// channel keeping the process alive) goes away — otherwise it would be
    /// left running on the remote host indefinitely.
    fn drop(&mut self) {
        if let Some(agent) = self.quic_agent_cache.get().and_then(|cached| cached.clone()) {
            let _ = kill_remote_process(self, agent.pid());
        }
    }
}
