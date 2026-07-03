//! Background job model: thread-per-job execution with progress reporting
//! over a `crossbeam_channel`, plus the backend-agnostic copy/move/delete
//! orchestration logic that drives it. All operations here work purely in
//! terms of `&dyn Vfs`, so the same code path serves local-only jobs today
//! and cross-backend/local<->remote jobs once later milestones need it.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};
use thiserror::Error;

use crate::vfs::{EntryKind, EntryMeta, Vfs, VfsError, VfsPath, VfsResult};

pub type JobId = u64;

const COPY_CHUNK_SIZE: usize = 256 * 1024;

/// A cheap, clonable flag a job checks periodically so its cancellation can
/// be requested from the UI thread without any unsafe thread interruption.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobProgress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: usize,
    pub files_total: usize,
    pub current_file: Option<VfsPath>,
}

#[derive(Clone, Debug)]
pub enum JobOutcome {
    Completed,
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub enum JobEvent {
    Progress { job_id: JobId, progress: JobProgress },
    Finished { job_id: JobId, outcome: JobOutcome },
}

#[derive(Debug, Error)]
pub enum JobError {
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Vfs(#[from] VfsError),
}

impl From<Result<(), JobError>> for JobOutcome {
    fn from(r: Result<(), JobError>) -> Self {
        match r {
            Ok(()) => JobOutcome::Completed,
            Err(JobError::Cancelled) => JobOutcome::Cancelled,
            Err(e) => JobOutcome::Failed(e.to_string()),
        }
    }
}

struct JobHandle {
    cancel: CancellationToken,
    label: String,
    join: Option<thread::JoinHandle<()>>,
}

/// Owns the shared job-event channel and the set of currently running/
/// finished-but-unjoined job threads. Runs no async code; every job is a
/// plain OS thread doing blocking work.
pub struct JobManager {
    next_id: AtomicU64,
    events_tx: Sender<JobEvent>,
    events_rx: Receiver<JobEvent>,
    jobs: HashMap<JobId, JobHandle>,
}

impl Default for JobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl JobManager {
    pub fn new() -> Self {
        let (events_tx, events_rx) = unbounded();
        Self {
            next_id: AtomicU64::new(0),
            events_tx,
            events_rx,
            jobs: HashMap::new(),
        }
    }

    /// Receiver the main loop selects on alongside input/tick events.
    pub fn events(&self) -> &Receiver<JobEvent> {
        &self.events_rx
    }

    pub fn spawn<F>(&mut self, label: impl Into<String>, work: F) -> JobId
    where
        F: FnOnce(&CancellationToken, &dyn Fn(JobProgress)) -> Result<(), JobError> + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        let cancel_for_thread = cancel.clone();
        let tx = self.events_tx.clone();

        let join = thread::Builder::new()
            .name(format!("pfnc-job-{id}"))
            .spawn(move || {
                let report = |progress: JobProgress| {
                    let _ = tx.send(JobEvent::Progress { job_id: id, progress });
                };
                // A job panicking (e.g. an unexpected backend edge case)
                // must not take the whole process down: report it as a
                // failed job instead.
                let result = panic::catch_unwind(AssertUnwindSafe(|| work(&cancel_for_thread, &report)));
                let outcome = match result {
                    Ok(r) => JobOutcome::from(r),
                    Err(_) => JobOutcome::Failed("job panicked".to_string()),
                };
                let _ = tx.send(JobEvent::Finished { job_id: id, outcome });
            })
            .expect("failed to spawn job thread");

        self.jobs.insert(
            id,
            JobHandle {
                cancel,
                label: label.into(),
                join: Some(join),
            },
        );
        id
    }

    pub fn cancel(&self, id: JobId) {
        if let Some(handle) = self.jobs.get(&id) {
            handle.cancel.cancel();
        }
    }

    pub fn label(&self, id: JobId) -> Option<&str> {
        self.jobs.get(&id).map(|h| h.label.as_str())
    }

    /// Joins and drops a finished job's thread handle. Call this once the
    /// UI has processed a job's `Finished` event.
    pub fn reap(&mut self, id: JobId) {
        if let Some(mut handle) = self.jobs.remove(&id) {
            if let Some(join) = handle.join.take() {
                let _ = join.join();
            }
        }
    }
}

/// Recursively describes everything under `src_path` (including itself) as
/// `(source_path, destination_path, metadata)` triples, so callers can size
/// progress totals up front before copying/deleting a single byte.
fn walk_for_copy(
    src: &dyn Vfs,
    src_path: &VfsPath,
    dst_path: &VfsPath,
    out: &mut Vec<(VfsPath, VfsPath, EntryMeta)>,
) -> VfsResult<()> {
    let meta = src.stat(src_path)?;
    let is_dir = meta.kind.is_dir();
    out.push((src_path.clone(), dst_path.clone(), meta));
    if is_dir {
        for entry in src.list_dir(src_path)? {
            let child_dst = dst_path.join(&entry.name);
            walk_for_copy(src, &entry.path, &child_dst, out)?;
        }
    }
    Ok(())
}

fn walk_for_delete(vfs: &dyn Vfs, path: &VfsPath, out: &mut Vec<(VfsPath, EntryMeta)>) -> VfsResult<()> {
    let meta = vfs.stat(path)?;
    if meta.kind.is_dir() {
        for entry in vfs.list_dir(path)? {
            walk_for_delete(vfs, &entry.path, out)?;
        }
    }
    // Post-order: children are pushed before their parent, so deleting in
    // list order never hits a non-empty directory.
    out.push((path.clone(), meta));
    Ok(())
}

fn copy_stream(
    mut reader: Box<dyn std::io::Read + Send>,
    mut writer: Box<dyn std::io::Write + Send>,
    cancel: &CancellationToken,
    progress: &mut JobProgress,
    report: &dyn Fn(JobProgress),
) -> Result<(), JobError> {
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

/// Copies `items` (files or directory trees) from `src` into `dest_dir` on
/// `dst`. `src` and `dst` may be the same backend instance (the common case
/// today) or two different ones (used from milestone M4 onward).
pub fn copy_job(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    items: &[VfsPath],
    dest_dir: &VfsPath,
    cancel: &CancellationToken,
    report: &dyn Fn(JobProgress),
) -> Result<(), JobError> {
    let mut plan = Vec::new();
    for item in items {
        let name = item
            .file_name()
            .ok_or(JobError::Vfs(VfsError::Unsupported("item has no file name")))?;
        walk_for_copy(src, item, &dest_dir.join(name), &mut plan)?;
    }

    let mut progress = JobProgress {
        files_total: plan.iter().filter(|(_, _, m)| !m.kind.is_dir()).count(),
        bytes_total: plan
            .iter()
            .filter(|(_, _, m)| !m.kind.is_dir())
            .map(|(_, _, m)| m.size)
            .sum(),
        ..Default::default()
    };
    report(progress.clone());

    for (src_path, dst_path, meta) in &plan {
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        progress.current_file = Some(dst_path.clone());
        match &meta.kind {
            EntryKind::Dir => {
                dst.mkdir(dst_path, meta.permissions)?;
            }
            EntryKind::Symlink { target: Some(target) } => {
                // Best-effort: recreate the link rather than following it.
                let _ = dst.symlink(target, dst_path);
            }
            EntryKind::Symlink { target: None } | EntryKind::File | EntryKind::Other => {
                let reader = src.open_read(src_path)?;
                let writer = dst.create_write(dst_path, meta.permissions)?;
                copy_stream(reader, writer, cancel, &mut progress, report)?;
                progress.files_done += 1;
                report(progress.clone());
            }
        }
    }
    Ok(())
}

/// Moves `items` to explicit destination paths on `vfs` via `rename`. Only
/// valid when source and destination share the same backend instance —
/// cross-backend moves (M4) compose `copy_job` + `delete_job` instead.
pub fn move_job(
    vfs: &dyn Vfs,
    items: &[(VfsPath, VfsPath)],
    cancel: &CancellationToken,
    report: &dyn Fn(JobProgress),
) -> Result<(), JobError> {
    let mut progress = JobProgress {
        files_total: items.len(),
        ..Default::default()
    };
    report(progress.clone());

    for (from, to) in items {
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        progress.current_file = Some(to.clone());
        vfs.rename(from, to)?;
        progress.files_done += 1;
        report(progress.clone());
    }
    Ok(())
}

/// Moves `items` from `src` into `dest_dir` on `dst` when the two are
/// *different* backend instances — `rename` only makes sense within one
/// backend, so this composes a copy followed by deleting the originals
/// (only once the copy fully succeeded).
pub fn move_cross_backend(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    items: &[VfsPath],
    dest_dir: &VfsPath,
    cancel: &CancellationToken,
    report: &dyn Fn(JobProgress),
) -> Result<(), JobError> {
    copy_job(src, dst, items, dest_dir, cancel, report)?;
    delete_job(src, items, cancel, report)
}

/// Deletes `items` (files or directory trees).
pub fn delete_job(
    vfs: &dyn Vfs,
    items: &[VfsPath],
    cancel: &CancellationToken,
    report: &dyn Fn(JobProgress),
) -> Result<(), JobError> {
    let mut plan = Vec::new();
    for item in items {
        walk_for_delete(vfs, item, &mut plan)?;
    }

    let mut progress = JobProgress {
        files_total: plan.len(),
        ..Default::default()
    };
    report(progress.clone());

    for (path, meta) in &plan {
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        progress.current_file = Some(path.clone());
        if meta.kind.is_dir() {
            vfs.remove_dir(path, false)?;
        } else {
            vfs.remove_file(path)?;
        }
        progress.files_done += 1;
        report(progress.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_token_reflects_cancel() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    // copy_job/move_job/delete_job need a concrete `Vfs` impl to exercise
    // meaningfully; those tests live in crates/vfs-local/tests/, where
    // LocalFs is available (core sits below the backend crates and can't
    // depend on them).
}
