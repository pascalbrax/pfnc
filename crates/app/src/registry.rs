//! Resolves a panel's `Location` to a live `Arc<dyn Vfs>`. Lives in the
//! `app` crate rather than `pfnc-core` because it needs to construct
//! concrete backends (`LocalFs`, `SftpFs`), and those crates sit *above*
//! `pfnc-core` in the dependency graph — `pfnc-core` itself only knows about
//! the `Vfs` trait, never a specific implementation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pfnc_core::{Location, ProfileId, Vfs, VfsError, VfsResult};
use pfnc_vfs_archive::ArchiveFs;
use pfnc_vfs_local::LocalFs;
use pfnc_vfs_sftp::{AcceptNewPolicy, ConnectError, ConnectionProfile, SftpFs};

pub struct VfsRegistry {
    local: Arc<dyn Vfs>,
    remotes: Mutex<HashMap<ProfileId, Arc<dyn Vfs>>>,
    archives: Mutex<HashMap<Location, Arc<dyn Vfs>>>,
}

impl VfsRegistry {
    pub fn new() -> Self {
        Self {
            local: Arc::new(LocalFs::new()),
            remotes: Mutex::new(HashMap::new()),
            archives: Mutex::new(HashMap::new()),
        }
    }

    /// Resolves an already-connected `Location`. Remote locations must
    /// have gone through `connect_and_cache` first (e.g. via the Connect
    /// dialog) — this never blocks on network I/O, so it's safe to call
    /// from UI-thread code like `reload`.
    pub fn resolve(&self, location: &Location) -> VfsResult<Arc<dyn Vfs>> {
        match location {
            Location::Local => Ok(Arc::clone(&self.local)),
            Location::Remote { profile_id } => self
                .remotes
                .lock()
                .unwrap()
                .get(profile_id)
                .cloned()
                .ok_or_else(|| VfsError::ConnectionLost(format!("no active connection for profile {profile_id}"))),
            Location::Archive { .. } => self
                .archives
                .lock()
                .unwrap()
                .get(location)
                .cloned()
                .ok_or_else(|| VfsError::ConnectionLost("archive not yet opened".to_string())),
        }
    }

    /// Performs the actual (blocking, network) SSH connect and, on
    /// success, caches the resulting backend under `profile.id`. Callers
    /// run this on a background job thread, never the UI thread.
    pub fn connect_and_cache(&self, profile: &ConnectionProfile) -> Result<(), ConnectError> {
        let known_hosts = pfnc_vfs_sftp::default_known_hosts_path();
        let sftp = SftpFs::connect(profile, &AcceptNewPolicy, &known_hosts)?;
        self.remotes.lock().unwrap().insert(profile.id.clone(), Arc::new(sftp));
        Ok(())
    }

    /// Downloads and indexes the archive `location` points at, caching the
    /// resulting `ArchiveFs` under `location` itself (cheap: `Location`
    /// derives `Eq`/`Hash`). Blocking (reads the whole archive, possibly
    /// over the network) — callers run this on a background job thread.
    pub fn open_archive_and_cache(&self, location: &Location) -> VfsResult<()> {
        let Location::Archive { base, archive_path } = location else {
            return Err(VfsError::Unsupported(
                "open_archive_and_cache called with a non-archive location",
            ));
        };
        let format = pfnc_vfs_archive::detect_format(archive_path)
            .ok_or(VfsError::Unsupported("not a recognized archive format"))?;
        let base_vfs = self.resolve(base)?;
        let archive_fs = ArchiveFs::open(base_vfs.as_ref(), archive_path, format)?;
        self.archives.lock().unwrap().insert(location.clone(), Arc::new(archive_fs));
        Ok(())
    }
}

impl Default for VfsRegistry {
    fn default() -> Self {
        Self::new()
    }
}
