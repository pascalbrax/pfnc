//! Exercises `pfnc_core::job`'s backend-agnostic copy/move/delete
//! orchestration against a real `LocalFs`, since `pfnc-core` itself has no
//! concrete `Vfs` implementation to test against.

use std::cell::RefCell;
use std::fs;

use camino::Utf8PathBuf;
use pfnc_core::job::{copy_job, delete_job, move_job, CancellationToken, JobProgress};
use pfnc_core::{Location, Vfs, VfsPath};
use pfnc_vfs_local::LocalFs;
use tempfile::tempdir;

fn vfs_path(p: &std::path::Path) -> VfsPath {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).expect("tempdir path must be UTF-8")
}

fn noop_report(_: JobProgress) {}

#[test]
fn copy_job_copies_file_and_directory_tree() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src_dir = root.join("src");
    vfs.mkdir(&src_dir, None).unwrap();
    fs::write(src_dir.join("a.txt").as_std_path(), b"aaa").unwrap();
    let nested = src_dir.join("nested");
    vfs.mkdir(&nested, None).unwrap();
    fs::write(nested.join("b.txt").as_std_path(), b"bb").unwrap();

    let dest_dir = root.join("dest");
    vfs.mkdir(&dest_dir, None).unwrap();

    let cancel = CancellationToken::new();
    copy_job(
        &vfs,
        &vfs,
        &Location::Local,
        &Location::Local,
        std::slice::from_ref(&src_dir),
        &dest_dir,
        &cancel,
        &noop_report,
    )
    .unwrap();

    let copied_a = dest_dir.join("src/a.txt");
    let copied_b = dest_dir.join("src/nested/b.txt");
    assert_eq!(fs::read(copied_a.as_std_path()).unwrap(), b"aaa");
    assert_eq!(fs::read(copied_b.as_std_path()).unwrap(), b"bb");
    // Original is untouched by a copy.
    assert!(src_dir.join("a.txt").as_std_path().exists());
}

#[test]
fn copy_job_reports_progress_and_totals() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let file = root.join("f.bin");
    fs::write(file.as_std_path(), vec![0u8; 10_000]).unwrap();
    let dest_dir = root.join("dest");
    vfs.mkdir(&dest_dir, None).unwrap();

    let cancel = CancellationToken::new();
    let last = RefCell::new(JobProgress::default());
    let report = |p: JobProgress| *last.borrow_mut() = p;
    copy_job(&vfs, &vfs, &Location::Local, &Location::Local, &[file], &dest_dir, &cancel, &report).unwrap();

    let last = last.into_inner();
    assert_eq!(last.files_total, 1);
    assert_eq!(last.files_done, 1);
    assert_eq!(last.bytes_total, 10_000);
    assert_eq!(last.bytes_done, 10_000);
}

#[test]
fn copy_job_respects_cancellation() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let file = root.join("big.bin");
    fs::write(file.as_std_path(), vec![0u8; 1_000_000]).unwrap();
    let dest_dir = root.join("dest");
    vfs.mkdir(&dest_dir, None).unwrap();

    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = copy_job(&vfs, &vfs, &Location::Local, &Location::Local, &[file], &dest_dir, &cancel, &noop_report)
        .unwrap_err();
    assert!(matches!(err, pfnc_core::job::JobError::Cancelled));
}

#[test]
fn move_job_renames_via_vfs_rename() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("orig.txt");
    fs::write(src.as_std_path(), b"data").unwrap();
    let dst = root.join("moved.txt");

    let cancel = CancellationToken::new();
    move_job(&vfs, &[(src.clone(), dst.clone())], &cancel, &noop_report).unwrap();

    assert!(!src.as_std_path().exists());
    assert_eq!(fs::read(dst.as_std_path()).unwrap(), b"data");
}

#[test]
fn delete_job_removes_files_and_nested_dirs() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let target = root.join("victim");
    vfs.mkdir(&target, None).unwrap();
    fs::write(target.join("a.txt").as_std_path(), b"x").unwrap();
    let nested = target.join("nested");
    vfs.mkdir(&nested, None).unwrap();
    fs::write(nested.join("b.txt").as_std_path(), b"y").unwrap();

    let cancel = CancellationToken::new();
    delete_job(&vfs, std::slice::from_ref(&target), &cancel, &noop_report).unwrap();

    assert!(!target.as_std_path().exists());
}
