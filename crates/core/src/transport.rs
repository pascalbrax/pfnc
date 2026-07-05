//! How bytes actually move for a single file transfer between two `Vfs`
//! endpoints, abstracted behind `Transport` so the sync/copy algorithms in
//! `job` never need to know or care which one is in use.
//!
//! `VfsStreamTransport` is the always-available fallback, backed by the
//! `Vfs` trait's own streaming `open_read`/`create_write` (however the
//! backend gets there — a local file read, or the SFTP data channel for a
//! remote one). `negotiate_transport` additionally tries a faster path for a
//! `Local`<->`Remote` pair: `Vfs::fast_transport()` on the remote side
//! returns a `RemoteFileAgent` (e.g. an already-deployed QUIC agent, see
//! `pfnc-vfs-sftp`) when one is available, wrapped as `QuicSideTransport`.
//! `pfnc-core` only defines the interfaces here — it never depends on
//! `quinn`/`tokio`/`ssh2` itself; only backend crates that can establish a
//! fast channel do.

use std::io::{Read, Write};
use std::sync::Arc;

use crate::job::{CancellationToken, JobError, JobProgress};
use crate::vfs::{Location, Vfs, VfsError, VfsPath};

/// A backend's own fast channel for streaming file read/write, when it has
/// one (e.g. an already-deployed QUIC agent for `SftpFs`) — see
/// `Vfs::fast_transport`. Implementations live in the backend crate that
/// knows how to establish the channel (e.g. `pfnc-vfs-sftp`); `pfnc-core`
/// only defines the interface, so it never needs to depend on `quinn`/
/// `tokio`/`ssh2`. Implementations must stream in bounded chunks rather
/// than buffering a whole file in memory, same expectation as `Vfs::
/// open_read`/`create_write`.
pub trait RemoteFileAgent: Send + Sync {
    /// Reads `path`'s content, writing it to `writer` as it arrives.
    /// Returns the total number of bytes written.
    fn read_file(&self, path: &VfsPath, writer: &mut dyn Write) -> Result<u64, JobError>;
    /// Writes `path`, pulling content from `reader` until exhausted.
    fn write_file(&self, path: &VfsPath, mode: Option<u32>, reader: &mut dyn Read) -> Result<(), JobError>;
}

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

/// Wraps a `Read`/`Write` so every call also checks `cancel` and reports
/// progress — used by `QuicSideTransport` so a `RemoteFileAgent`'s chunked
/// `read_file`/`write_file` gets the same per-chunk progress/cancellation
/// behavior `VfsStreamTransport`'s own loop already has, without adding any
/// progress-specific plumbing to the `RemoteFileAgent` trait itself: the
/// chunk boundary is simply whatever `write`/`read` call the agent
/// implementation happens to make.
struct ProgressIo<'a, T> {
    inner: T,
    cancel: &'a CancellationToken,
    progress: &'a mut JobProgress,
    report: &'a dyn Fn(JobProgress),
}

fn cancelled_io_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")
}

impl<T: Write> Write for ProgressIo<'_, T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(cancelled_io_error());
        }
        let n = self.inner.write(buf)?;
        if n > 0 {
            self.progress.bytes_done += n as u64;
            (self.report)(self.progress.clone());
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<T: Read> Read for ProgressIo<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(cancelled_io_error());
        }
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.progress.bytes_done += n as u64;
            (self.report)(self.progress.clone());
        }
        Ok(n)
    }
}

/// A `Transport` that routes whichever side is remote through its
/// `RemoteFileAgent` (e.g. a deployed QUIC agent) and whichever side is
/// local through the plain `Vfs` stream calls, wrapped in `ProgressIo` so
/// progress/cancellation behave the same as `VfsStreamTransport`. Only ever
/// constructed by `negotiate_transport` for a `Local`<->`Remote` pair, where
/// `remote_is_src` is fixed for the lifetime of the job (every item in one
/// `copy_job`/`execute_sync_plan` call goes the same direction).
struct QuicSideTransport {
    agent: Arc<dyn RemoteFileAgent>,
    remote_is_src: bool,
}

impl Transport for QuicSideTransport {
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
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }

        let result = if self.remote_is_src {
            let mut writer = dst.create_write(dst_path, mode)?;
            let mut wrapped = ProgressIo { inner: &mut writer, cancel, progress, report };
            self.agent.read_file(src_path, &mut wrapped).map(|_| ())
        } else {
            let mut reader = src.open_read(src_path)?;
            let mut wrapped = ProgressIo { inner: &mut reader, cancel, progress, report };
            self.agent.write_file(dst_path, mode, &mut wrapped)
        };

        match result {
            Ok(()) => {
                progress.files_done += 1;
                report(progress.clone());
                Ok(())
            }
            Err(_) if cancel.is_cancelled() => Err(JobError::Cancelled),
            Err(e) => Err(e),
        }
    }
}

/// Picks the `Transport` to use for a single-file transfer between
/// `src_location` and `dst_location`. When `enable_quic_fast_path` is set
/// and exactly one side is `Location::Remote`, tries that side's
/// `Vfs::fast_transport()` and uses it if available; otherwise (disabled,
/// both-local, both-remote, or an archive endpoint involved, or no fast
/// channel available) falls back to `VfsStreamTransport`.
pub fn negotiate_transport(
    src_location: &Location,
    dst_location: &Location,
    src: &dyn Vfs,
    dst: &dyn Vfs,
    enable_quic_fast_path: bool,
) -> Box<dyn Transport> {
    if enable_quic_fast_path {
        match (src_location, dst_location) {
            (Location::Local, Location::Remote { .. }) => {
                if let Some(agent) = dst.fast_transport() {
                    return Box::new(QuicSideTransport { agent, remote_is_src: false });
                }
            }
            (Location::Remote { .. }, Location::Local) => {
                if let Some(agent) = src.fast_transport() {
                    return Box::new(QuicSideTransport { agent, remote_is_src: true });
                }
            }
            _ => {}
        }
    }
    Box::new(VfsStreamTransport)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::vfs::{EntryMeta, VfsCapabilities, VfsResult};

    /// A minimal `Vfs` fake: only `open_read`/`create_write`/`fast_transport`
    /// (the ones `Transport::transfer` actually calls) do anything real;
    /// everything else is unreachable in these tests. Records whether
    /// `open_read`/`create_write` were called so tests can tell which
    /// `Transport` implementation actually ran.
    struct FakeVfs {
        fast_transport: Option<Arc<dyn RemoteFileAgent>>,
        open_read_called: AtomicBool,
        create_write_called: AtomicBool,
        read_payload: Vec<u8>,
    }

    impl FakeVfs {
        fn plain() -> Self {
            Self {
                fast_transport: None,
                open_read_called: AtomicBool::new(false),
                create_write_called: AtomicBool::new(false),
                read_payload: b"payload".to_vec(),
            }
        }

        fn with_fast_transport(agent: Arc<dyn RemoteFileAgent>) -> Self {
            Self { fast_transport: Some(agent), ..Self::plain() }
        }

        fn with_read_payload(read_payload: Vec<u8>) -> Self {
            Self { read_payload, ..Self::plain() }
        }
    }

    impl Vfs for FakeVfs {
        fn list_dir(&self, _path: &VfsPath) -> VfsResult<Vec<EntryMeta>> {
            unimplemented!("not exercised by these tests")
        }
        fn stat(&self, _path: &VfsPath) -> VfsResult<EntryMeta> {
            unimplemented!("not exercised by these tests")
        }
        fn open_read(&self, _path: &VfsPath) -> VfsResult<Box<dyn Read + Send>> {
            self.open_read_called.store(true, Ordering::SeqCst);
            Ok(Box::new(Cursor::new(self.read_payload.clone())))
        }
        fn create_write(&self, _path: &VfsPath, _mode: Option<u32>) -> VfsResult<Box<dyn Write + Send>> {
            self.create_write_called.store(true, Ordering::SeqCst);
            struct Sink;
            impl Write for Sink {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            Ok(Box::new(Sink))
        }
        fn mkdir(&self, _path: &VfsPath, _mode: Option<u32>) -> VfsResult<()> {
            unimplemented!("not exercised by these tests")
        }
        fn remove_file(&self, _path: &VfsPath) -> VfsResult<()> {
            unimplemented!("not exercised by these tests")
        }
        fn remove_dir(&self, _path: &VfsPath, _recursive: bool) -> VfsResult<()> {
            unimplemented!("not exercised by these tests")
        }
        fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> VfsResult<()> {
            unimplemented!("not exercised by these tests")
        }
        fn set_permissions(&self, _path: &VfsPath, _mode: u32) -> VfsResult<()> {
            unimplemented!("not exercised by these tests")
        }
        fn symlink(&self, _target: &VfsPath, _link: &VfsPath) -> VfsResult<()> {
            unimplemented!("not exercised by these tests")
        }
        fn capabilities(&self) -> VfsCapabilities {
            VfsCapabilities { can_write: true, can_set_permissions: false, can_symlink: false, can_rename: true }
        }
        fn root(&self) -> VfsPath {
            VfsPath::from("/")
        }
        fn fast_transport(&self) -> Option<Arc<dyn RemoteFileAgent>> {
            self.fast_transport.clone()
        }
    }

    #[derive(Default)]
    struct FakeAgent {
        read_called: AtomicBool,
        write_called: AtomicBool,
    }

    fn io_err(e: std::io::Error) -> JobError {
        JobError::Vfs(VfsError::Io(e))
    }

    impl RemoteFileAgent for FakeAgent {
        fn read_file(&self, _path: &VfsPath, writer: &mut dyn Write) -> Result<u64, JobError> {
            self.read_called.store(true, Ordering::SeqCst);
            let data = b"agent-payload";
            writer.write_all(data).map_err(io_err)?;
            Ok(data.len() as u64)
        }
        fn write_file(&self, _path: &VfsPath, _mode: Option<u32>, reader: &mut dyn Read) -> Result<(), JobError> {
            self.write_called.store(true, Ordering::SeqCst);
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).map_err(io_err)?;
            Ok(())
        }
    }

    fn remote(id: &str) -> Location {
        Location::Remote { profile_id: id.to_string() }
    }

    fn run_transfer(transport: &dyn Transport, src: &dyn Vfs, dst: &dyn Vfs) {
        let cancel = CancellationToken::new();
        let mut progress = JobProgress::default();
        transport
            .transfer(src, &VfsPath::from("/src"), dst, &VfsPath::from("/dst"), None, &cancel, &mut progress, &|_| {})
            .unwrap();
    }

    #[test]
    fn flag_off_falls_back_to_stream_even_when_fast_transport_available() {
        let agent = Arc::new(FakeAgent::default());
        let src = FakeVfs::plain();
        let dst = FakeVfs::with_fast_transport(agent.clone());

        let transport = negotiate_transport(&Location::Local, &remote("r1"), &src, &dst, false);
        run_transfer(transport.as_ref(), &src, &dst);

        assert!(src.open_read_called.load(Ordering::SeqCst));
        assert!(dst.create_write_called.load(Ordering::SeqCst));
        assert!(!agent.read_called.load(Ordering::SeqCst));
        assert!(!agent.write_called.load(Ordering::SeqCst));
    }

    #[test]
    fn remote_dst_uses_fast_path_when_enabled_and_available() {
        let agent = Arc::new(FakeAgent::default());
        let src = FakeVfs::plain();
        let dst = FakeVfs::with_fast_transport(agent.clone());

        let transport = negotiate_transport(&Location::Local, &remote("r1"), &src, &dst, true);
        run_transfer(transport.as_ref(), &src, &dst);

        assert!(src.open_read_called.load(Ordering::SeqCst), "local side is still read through the plain Vfs call");
        assert!(agent.write_called.load(Ordering::SeqCst), "remote side is written through the agent");
        assert!(!dst.create_write_called.load(Ordering::SeqCst));
        assert!(!agent.read_called.load(Ordering::SeqCst));
    }

    #[test]
    fn remote_src_uses_fast_path_when_enabled_and_available() {
        let agent = Arc::new(FakeAgent::default());
        let src = FakeVfs::with_fast_transport(agent.clone());
        let dst = FakeVfs::plain();

        let transport = negotiate_transport(&remote("r1"), &Location::Local, &src, &dst, true);
        run_transfer(transport.as_ref(), &src, &dst);

        assert!(agent.read_called.load(Ordering::SeqCst), "remote side is read through the agent");
        assert!(dst.create_write_called.load(Ordering::SeqCst), "local side is still written through the plain Vfs call");
        assert!(!src.open_read_called.load(Ordering::SeqCst));
        assert!(!agent.write_called.load(Ordering::SeqCst));
    }

    #[test]
    fn both_local_never_uses_fast_path() {
        let agent = Arc::new(FakeAgent::default());
        let src = FakeVfs::with_fast_transport(agent.clone());
        let dst = FakeVfs::with_fast_transport(agent.clone());

        let transport = negotiate_transport(&Location::Local, &Location::Local, &src, &dst, true);
        run_transfer(transport.as_ref(), &src, &dst);

        assert!(!agent.read_called.load(Ordering::SeqCst));
        assert!(!agent.write_called.load(Ordering::SeqCst));
        assert!(src.open_read_called.load(Ordering::SeqCst));
        assert!(dst.create_write_called.load(Ordering::SeqCst));
    }

    #[test]
    fn both_remote_never_uses_fast_path() {
        let agent = Arc::new(FakeAgent::default());
        let src = FakeVfs::with_fast_transport(agent.clone());
        let dst = FakeVfs::with_fast_transport(agent.clone());

        let transport = negotiate_transport(&remote("r1"), &remote("r2"), &src, &dst, true);
        run_transfer(transport.as_ref(), &src, &dst);

        assert!(!agent.read_called.load(Ordering::SeqCst));
        assert!(!agent.write_called.load(Ordering::SeqCst));
    }

    #[test]
    fn falls_back_when_fast_transport_returns_none() {
        let src = FakeVfs::plain();
        let dst = FakeVfs::plain();

        let transport = negotiate_transport(&Location::Local, &remote("r1"), &src, &dst, true);
        run_transfer(transport.as_ref(), &src, &dst);

        assert!(src.open_read_called.load(Ordering::SeqCst));
        assert!(dst.create_write_called.load(Ordering::SeqCst));
    }

    /// An agent that reads in small 4-byte chunks (mimicking a real
    /// streaming `RemoteFileAgent`, rather than one `read_to_end` call) so
    /// a test can observe a transfer actually stopping partway through.
    struct ChunkyWriteAgent;

    impl RemoteFileAgent for ChunkyWriteAgent {
        fn read_file(&self, _path: &VfsPath, _writer: &mut dyn Write) -> Result<u64, JobError> {
            unimplemented!("not exercised by this test")
        }
        fn write_file(&self, _path: &VfsPath, _mode: Option<u32>, reader: &mut dyn Read) -> Result<(), JobError> {
            let mut buf = [0u8; 4];
            loop {
                let n = reader.read(&mut buf).map_err(io_err)?;
                if n == 0 {
                    break;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn cancellation_mid_transfer_stops_a_multi_chunk_upload() {
        let agent: Arc<dyn RemoteFileAgent> = Arc::new(ChunkyWriteAgent);
        // 20 bytes / 4-byte reads = 5 chunks, so cancelling after the 2nd
        // chunk's progress report genuinely stops the transfer partway
        // through rather than after the first or last chunk.
        let src = FakeVfs::with_read_payload(vec![0u8; 20]);
        let dst = FakeVfs::plain();

        let transport = QuicSideTransport { agent, remote_is_src: false };
        let cancel = CancellationToken::new();
        let cancel_for_report = cancel.clone();
        let mut progress = JobProgress::default();
        let chunks_seen = std::cell::Cell::new(0u32);
        let report = move |_: JobProgress| {
            chunks_seen.set(chunks_seen.get() + 1);
            if chunks_seen.get() == 2 {
                cancel_for_report.cancel();
            }
        };

        let err = transport
            .transfer(&src, &VfsPath::from("/src"), &dst, &VfsPath::from("/dst"), None, &cancel, &mut progress, &report)
            .unwrap_err();

        assert!(matches!(err, JobError::Cancelled));
        assert!(progress.bytes_done < 20, "cancellation should stop the transfer before all chunks are read");
        assert!(progress.bytes_done > 0, "at least the chunks read before cancellation should still count");
    }
}
