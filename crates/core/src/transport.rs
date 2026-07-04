//! How bytes actually move for a single file transfer between two `Vfs`
//! endpoints, abstracted behind `Transport` so the sync/copy algorithms in
//! `job` never need to know or care which one is in use.
//!
//! Today there is exactly one implementation, `VfsStreamTransport`, backed
//! by the `Vfs` trait's own streaming `open_read`/`create_write` (which is
//! however the backend gets there — a local file read, or the SFTP data
//! channel for a remote one). `negotiate_transport` is the seam for a
//! future, faster path: when a remote host is involved, it could probe UDP
//! reachability and negotiate a `quinn`-based QUIC transport with an agent
//! deployed over the SSH control channel, falling back to
//! `VfsStreamTransport` (SFTP) when that isn't possible. No such
//! negotiation exists yet — `negotiate_transport` always returns the
//! stream transport — but `copy_job` and sync already go through this seam
//! so adding that later doesn't require touching either algorithm.

use std::io::{Read, Write};

use crate::job::{CancellationToken, JobError, JobProgress};
use crate::vfs::{Location, Vfs, VfsError, VfsPath};

/// Moves the content of one file from `src_path` on `src` to `dst_path` on
/// `dst`. Implementations are responsible for the whole transfer,
/// including honoring `cancel` and reporting incremental progress via
/// `report`.
pub trait Transport: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn transfer(
        &self,
        src: &dyn Vfs,
        src_path: &VfsPath,
        dst: &dyn Vfs,
        dst_path: &VfsPath,
        mode: Option<u32>,
        cancel: &CancellationToken,
        progress: &mut JobProgress,
        report: &dyn Fn(JobProgress),
    ) -> Result<(), JobError>;
}

const COPY_CHUNK_SIZE: usize = 256 * 1024;

/// The only `Transport` today: a plain chunked copy through each backend's
/// `Vfs::open_read`/`create_write` streams. Correct for any backend
/// combination (local-local, local-remote, remote-remote, archive-*)
/// because it only relies on the `Vfs` trait, never backend internals.
pub struct VfsStreamTransport;

impl Transport for VfsStreamTransport {
    fn transfer(
        &self,
        src: &dyn Vfs,
        src_path: &VfsPath,
        dst: &dyn Vfs,
        dst_path: &VfsPath,
        mode: Option<u32>,
        cancel: &CancellationToken,
        progress: &mut JobProgress,
        report: &dyn Fn(JobProgress),
    ) -> Result<(), JobError> {
        let mut reader = src.open_read(src_path)?;
        let mut writer = dst.create_write(dst_path, mode)?;

        let mut buf = vec![0u8; COPY_CHUNK_SIZE];
        loop {
            if cancel.is_cancelled() {
                return Err(JobError::Cancelled);
            }
            let n = reader.read(&mut buf).map_err(VfsError::from)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).map_err(VfsError::from)?;
            progress.bytes_done += n as u64;
            report(progress.clone());
        }
        writer.flush().map_err(VfsError::from)?;
        Ok(())
    }
}

/// Picks the `Transport` to use for a single-file transfer between
/// `src_location` and `dst_location`. Always `VfsStreamTransport` for now
/// (see module docs) — the parameters are unused today but define the
/// shape a real negotiation will need: it has to know both endpoints to
/// decide whether a faster path is even possible.
pub fn negotiate_transport(_src_location: &Location, _dst_location: &Location) -> Box<dyn Transport> {
    Box::new(VfsStreamTransport)
}
