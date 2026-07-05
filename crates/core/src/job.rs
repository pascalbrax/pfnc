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
use std::time::{Duration, SystemTime};

use crossbeam_channel::{unbounded, Receiver, Sender};
use thiserror::Error;

use crate::transport::{negotiate_transport, Transport};
use crate::vfs::{EntryKind, EntryMeta, Location, Vfs, VfsError, VfsPath, VfsResult};

pub type JobId = u64;

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

/// Applies one planned copy-side item (mkdir / transfer / best-effort
/// symlink recreate) via `transport`. Shared by `copy_job` and
/// `execute_sync_plan` so the two per-item dispatches can't drift apart.
#[allow(clippy::too_many_arguments)]
fn apply_copy_item(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    transport: &dyn Transport,
    src_path: &VfsPath,
    dst_path: &VfsPath,
    meta: &EntryMeta,
    cancel: &CancellationToken,
    progress: &mut JobProgress,
    report: &dyn Fn(JobProgress),
) -> Result<(), JobError> {
    match &meta.kind {
        EntryKind::Dir => {
            dst.mkdir(dst_path, meta.permissions)?;
        }
        EntryKind::Symlink { target: Some(target) } => {
            // Best-effort: recreate the link rather than following it.
            let _ = dst.symlink(target, dst_path);
        }
        EntryKind::Symlink { target: None } | EntryKind::File | EntryKind::Other => {
            transport.transfer(src, src_path, dst, dst_path, meta.permissions, cancel, progress, report)?;
            progress.files_done += 1;
            report(progress.clone());
        }
    }
    Ok(())
}

/// Deletes one planned entry. Shared by `delete_job` and
/// `execute_sync_plan`'s dest-only cleanup pass.
fn apply_delete_item(vfs: &dyn Vfs, path: &VfsPath, meta: &EntryMeta) -> VfsResult<()> {
    if meta.kind.is_dir() {
        vfs.remove_dir(path, false)
    } else {
        vfs.remove_file(path)
    }
}

/// Copies `items` (files or directory trees) from `src` into `dest_dir` on
/// `dst`. `src` and `dst` may be the same backend instance (the common case
/// today) or two different ones. `src_location`/`dst_location` identify the
/// endpoints for `negotiate_transport`, which picks a faster path than the
/// generic `Vfs`-stream transport when `enable_quic_fast_path` is set and
/// one side offers one (see `pfnc_core::transport`).
#[allow(clippy::too_many_arguments)]
pub fn copy_job(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    src_location: &Location,
    dst_location: &Location,
    items: &[VfsPath],
    dest_dir: &VfsPath,
    enable_quic_fast_path: bool,
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

    let transport = negotiate_transport(src_location, dst_location, src, dst, enable_quic_fast_path);
    for (src_path, dst_path, meta) in &plan {
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        progress.current_file = Some(dst_path.clone());
        apply_copy_item(src, dst, transport.as_ref(), src_path, dst_path, meta, cancel, &mut progress, report)?;
    }
    Ok(())
}

/// Moves `items` to explicit destination paths on `vfs` via `rename`. Only
/// valid when source and destination share the same backend instance —
/// cross-backend moves compose `copy_job` + `delete_job` instead.
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
#[allow(clippy::too_many_arguments)]
pub fn move_cross_backend(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    src_location: &Location,
    dst_location: &Location,
    items: &[VfsPath],
    dest_dir: &VfsPath,
    enable_quic_fast_path: bool,
    cancel: &CancellationToken,
    report: &dyn Fn(JobProgress),
) -> Result<(), JobError> {
    copy_job(src, dst, src_location, dst_location, items, dest_dir, enable_quic_fast_path, cancel, report)?;
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
        apply_delete_item(vfs, path, meta)?;
        progress.files_done += 1;
        report(progress.clone());
    }
    Ok(())
}

/// One item in a `SyncPlan`'s copy list: create or overwrite `dst_path`
/// from `src_path`. `replace_existing`, when `Some`, is the entry
/// currently at `dst_path` that must be removed first — a type mismatch
/// (e.g. the source is now a directory where the destination has a plain
/// file). Order matters here: `execute_sync_plan` always removes it
/// immediately before creating the replacement, never batches it with the
/// general dest-only cleanup pass.
#[derive(Clone, Debug)]
pub struct SyncCopyItem {
    pub src_path: VfsPath,
    pub dst_path: VfsPath,
    pub meta: EntryMeta,
    pub replace_existing: Option<EntryMeta>,
}

/// The result of diffing two directory trees for `sync`: what to copy and
/// (only when the sync was started with delete-extraneous enabled) what to
/// remove from the destination. Built by `build_sync_plan`, applied
/// unchanged by `execute_sync_plan` — scanning never mutates anything, so
/// the user gets a real preview before anything happens.
#[derive(Clone, Debug, Default)]
pub struct SyncPlan {
    pub copy: Vec<SyncCopyItem>,
    /// Dest-only entries, post-order (children before parents).
    pub delete: Vec<(VfsPath, EntryMeta)>,
    pub bytes_total: u64,
    pub files_to_copy: usize,
    pub items_to_delete: usize,
}

impl SyncPlan {
    /// Nothing to do — no confirmation dialog is needed for a no-op sync.
    pub fn is_empty(&self) -> bool {
        self.copy.is_empty() && self.delete.is_empty()
    }
}

/// SFTP protocol v3 mtimes only have whole-second resolution, while local
/// filesystem mtimes are much finer, and `create_write` doesn't preserve a
/// source's mtime on the destination — so a same-instant round trip must
/// not be misread as "changed" by the mtime fallback below.
const MTIME_TOLERANCE: Duration = Duration::from_secs(2);

/// Whether a same-named, same-shaped file pair needs copying. Sizes differ
/// is a cheap definitive "yes". Otherwise, a content hash on both sides
/// (when both backends can produce one — see `Vfs::quick_hash`) is
/// definitive either way; only when hashing isn't available on both sides
/// do we fall back to rsync's classic "quick check" (mtime), which can
/// occasionally miss a same-size, same-mtime content change.
fn file_needs_copy(src: &dyn Vfs, dst: &dyn Vfs, src_entry: &EntryMeta, dst_entry: &EntryMeta) -> VfsResult<bool> {
    if src_entry.size != dst_entry.size {
        return Ok(true);
    }
    match (src.quick_hash(&src_entry.path)?, dst.quick_hash(&dst_entry.path)?) {
        (Some(src_hash), Some(dst_hash)) => Ok(src_hash != dst_hash),
        _ => Ok(mtime_looks_changed(src_entry.modified, dst_entry.modified)),
    }
}

fn mtime_looks_changed(src_modified: Option<SystemTime>, dst_modified: Option<SystemTime>) -> bool {
    match (src_modified, dst_modified) {
        (Some(src), Some(dst)) => match src.duration_since(dst) {
            Ok(diff) => diff > MTIME_TOLERANCE,
            Err(_) => false, // source is not newer than destination
        },
        // Missing mtime info on either side means we can't tell — copy to
        // be safe; a redundant copy is far cheaper than silently missing a
        // real change.
        _ => true,
    }
}

/// Pushes `meta` (already known, e.g. from a prior `list_dir`) as a fresh
/// create at `dst_path`, recursing into every descendant when it's a
/// directory. Used both for entries that don't exist on the destination at
/// all, and — via the top-level `replace_existing` — for the "new subtree
/// created after removing an old, differently-shaped entry" case.
fn push_full_copy(
    src: &dyn Vfs,
    src_path: &VfsPath,
    dst_path: &VfsPath,
    meta: EntryMeta,
    replace_existing: Option<EntryMeta>,
    plan: &mut SyncPlan,
) -> VfsResult<()> {
    let is_dir = meta.kind.is_dir();
    plan.copy.push(SyncCopyItem {
        src_path: src_path.clone(),
        dst_path: dst_path.clone(),
        meta,
        replace_existing,
    });
    if is_dir {
        for entry in src.list_dir(src_path)? {
            let child_dst = dst_path.join(&entry.name);
            let child_src = entry.path.clone();
            push_full_copy(src, &child_src, &child_dst, entry, None, plan)?;
        }
    }
    Ok(())
}

/// Classifies one name present on both sides: same-shape entries are
/// compared/recursed, a shape mismatch (e.g. file replaced by a directory)
/// is treated as remove-then-recreate.
fn diff_entry(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    src_entry: EntryMeta,
    dst_entry: EntryMeta,
    dst_path: VfsPath,
    delete_extraneous: bool,
    plan: &mut SyncPlan,
) -> VfsResult<()> {
    let same_shape = matches!(
        (&src_entry.kind, &dst_entry.kind),
        (EntryKind::Dir, EntryKind::Dir)
            | (EntryKind::File, EntryKind::File)
            | (EntryKind::Symlink { .. }, EntryKind::Symlink { .. })
            | (EntryKind::Other, EntryKind::Other)
    );

    if !same_shape {
        let src_path = src_entry.path.clone();
        return push_full_copy(src, &src_path, &dst_path, src_entry, Some(dst_entry), plan);
    }

    match &src_entry.kind {
        EntryKind::Dir => diff_dir(src, dst, &src_entry.path.clone(), &dst_path, delete_extraneous, plan),
        EntryKind::Symlink { .. } => {
            if src_entry.kind != dst_entry.kind {
                let src_path = src_entry.path.clone();
                plan.copy.push(SyncCopyItem {
                    src_path,
                    dst_path,
                    meta: src_entry,
                    replace_existing: Some(dst_entry),
                });
            }
            Ok(())
        }
        EntryKind::File => {
            if file_needs_copy(src, dst, &src_entry, &dst_entry)? {
                let src_path = src_entry.path.clone();
                plan.copy.push(SyncCopyItem {
                    src_path,
                    dst_path,
                    meta: src_entry,
                    replace_existing: None,
                });
            }
            Ok(())
        }
        EntryKind::Other => Ok(()),
    }
}

/// Diffs `src_dir` against `dst_dir` (both already-listed directories) and
/// recurses into shared subdirectories. Entries only on the destination
/// are recorded for deletion (post-order, via `walk_for_delete`) only when
/// `delete_extraneous` is set — otherwise they're simply left alone.
fn diff_dir(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    src_dir: &VfsPath,
    dst_dir: &VfsPath,
    delete_extraneous: bool,
    plan: &mut SyncPlan,
) -> VfsResult<()> {
    let src_entries = src.list_dir(src_dir)?;
    let dst_entries = dst.list_dir(dst_dir)?;
    let mut dst_by_name: HashMap<String, EntryMeta> = dst_entries.into_iter().map(|e| (e.name.clone(), e)).collect();

    for src_entry in src_entries {
        let dst_path = dst_dir.join(&src_entry.name);
        match dst_by_name.remove(&src_entry.name) {
            None => {
                let src_path = src_entry.path.clone();
                push_full_copy(src, &src_path, &dst_path, src_entry, None, plan)?;
            }
            Some(dst_entry) => {
                diff_entry(src, dst, src_entry, dst_entry, dst_path, delete_extraneous, plan)?;
            }
        }
    }

    if delete_extraneous {
        for leftover in dst_by_name.into_values() {
            walk_for_delete(dst, &leftover.path, &mut plan.delete)?;
        }
    }
    Ok(())
}

/// Computes what a sync of `src_root` (on `src`) into `dst_root` (on
/// `dst`) would do, without changing anything — so the caller can show the
/// user a preview before committing via `execute_sync_plan`. See
/// `file_needs_copy` for the comparison rule and `diff_entry` for how
/// type/shape mismatches are handled.
pub fn build_sync_plan(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    src_root: &VfsPath,
    dst_root: &VfsPath,
    delete_extraneous: bool,
    cancel: &CancellationToken,
    report: &dyn Fn(JobProgress),
) -> Result<SyncPlan, JobError> {
    if cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }

    let mut plan = SyncPlan::default();
    diff_dir(src, dst, src_root, dst_root, delete_extraneous, &mut plan)?;

    plan.files_to_copy = plan.copy.iter().filter(|i| !i.meta.kind.is_dir()).count();
    plan.bytes_total = plan.copy.iter().filter(|i| !i.meta.kind.is_dir()).map(|i| i.meta.size).sum();
    plan.items_to_delete = plan.delete.len();

    report(JobProgress {
        files_total: plan.files_to_copy,
        bytes_total: plan.bytes_total,
        ..Default::default()
    });
    Ok(plan)
}

/// Applies a `SyncPlan` computed by `build_sync_plan`: copies/replaces
/// first (so a type-mismatch replacement's removal always happens right
/// before its recreation, never batched with the general cleanup pass),
/// then removes dest-only entries post-order.
#[allow(clippy::too_many_arguments)]
pub fn execute_sync_plan(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    src_location: &Location,
    dst_location: &Location,
    plan: &SyncPlan,
    enable_quic_fast_path: bool,
    cancel: &CancellationToken,
    report: &dyn Fn(JobProgress),
) -> Result<(), JobError> {
    let transport = negotiate_transport(src_location, dst_location, src, dst, enable_quic_fast_path);
    let mut progress = JobProgress {
        files_total: plan.files_to_copy,
        bytes_total: plan.bytes_total,
        ..Default::default()
    };
    report(progress.clone());

    for item in &plan.copy {
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        if let Some(old) = &item.replace_existing {
            if old.kind.is_dir() {
                dst.remove_dir(&item.dst_path, true)?;
            } else {
                dst.remove_file(&item.dst_path)?;
            }
        }
        progress.current_file = Some(item.dst_path.clone());
        apply_copy_item(
            src,
            dst,
            transport.as_ref(),
            &item.src_path,
            &item.dst_path,
            &item.meta,
            cancel,
            &mut progress,
            report,
        )?;
    }

    for (path, meta) in &plan.delete {
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        progress.current_file = Some(path.clone());
        apply_delete_item(dst, path, meta)?;
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
