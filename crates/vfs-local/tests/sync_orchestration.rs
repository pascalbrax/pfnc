//! Exercises `pfnc_core::job`'s directory-sync orchestration
//! (`build_sync_plan`/`execute_sync_plan`) against a real `LocalFs`, since
//! `pfnc-core` itself has no concrete `Vfs` implementation to test against.

use std::fs;
use std::os::unix::fs::symlink;

use camino::Utf8PathBuf;
use pfnc_core::job::{build_sync_plan, execute_sync_plan, CancellationToken, JobProgress};
use pfnc_core::{Location, Vfs, VfsPath};
use pfnc_vfs_local::LocalFs;
use tempfile::tempdir;

fn vfs_path(p: &std::path::Path) -> VfsPath {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).expect("tempdir path must be UTF-8")
}

fn noop_report(_: JobProgress) {}

/// Scans and (if the scan found anything to do) executes a sync from
/// `src_root` to `dst_root`, returning the scanned plan for assertions.
fn run_sync(
    vfs: &LocalFs,
    src_root: &VfsPath,
    dst_root: &VfsPath,
    delete_extraneous: bool,
) -> pfnc_core::SyncPlan {
    let cancel = CancellationToken::new();
    let plan = build_sync_plan(vfs, vfs, src_root, dst_root, delete_extraneous, &cancel, &noop_report).unwrap();
    execute_sync_plan(vfs, vfs, &Location::Local, &Location::Local, &plan, false, &cancel, &noop_report).unwrap();
    plan
}

#[test]
fn new_file_is_copied() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src, None).unwrap();
    vfs.mkdir(&dst, None).unwrap();
    fs::write(src.join("new.txt").as_std_path(), b"fresh content").unwrap();

    let plan = run_sync(&vfs, &src, &dst, false);

    assert_eq!(plan.files_to_copy, 1);
    assert_eq!(fs::read(dst.join("new.txt").as_std_path()).unwrap(), b"fresh content");
}

#[test]
fn changed_file_different_size_is_overwritten() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src, None).unwrap();
    vfs.mkdir(&dst, None).unwrap();
    fs::write(src.join("f.txt").as_std_path(), b"much longer new content").unwrap();
    fs::write(dst.join("f.txt").as_std_path(), b"old").unwrap();

    run_sync(&vfs, &src, &dst, false);

    assert_eq!(fs::read(dst.join("f.txt").as_std_path()).unwrap(), b"much longer new content");
}

#[test]
fn identical_file_is_left_untouched() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src, None).unwrap();
    vfs.mkdir(&dst, None).unwrap();
    fs::write(src.join("same.txt").as_std_path(), b"identical content").unwrap();
    fs::write(dst.join("same.txt").as_std_path(), b"identical content").unwrap();

    let dst_file = dst.join("same.txt");
    let mtime_before = fs::metadata(dst_file.as_std_path()).unwrap().modified().unwrap();

    let plan = run_sync(&vfs, &src, &dst, false);

    assert_eq!(plan.files_to_copy, 0, "identical content must not be re-copied");
    let mtime_after = fs::metadata(dst_file.as_std_path()).unwrap().modified().unwrap();
    assert_eq!(mtime_before, mtime_after, "untouched file's mtime must not change");
}

#[test]
fn same_size_different_content_is_caught_by_hashing_even_when_destination_is_newer() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src, None).unwrap();
    vfs.mkdir(&dst, None).unwrap();

    // Source written first, destination written after (so destination's
    // mtime is >= source's) with different content of the *same* size — a
    // naive "copy only if source is newer" check would wrongly skip this.
    // Hashing catches it because `LocalFs::quick_hash` is always available
    // for both sides here.
    fs::write(src.join("f.txt").as_std_path(), b"aaaaaaaaaa").unwrap();
    fs::write(dst.join("f.txt").as_std_path(), b"bbbbbbbbbb").unwrap();

    let plan = run_sync(&vfs, &src, &dst, false);

    assert_eq!(plan.files_to_copy, 1, "hash must catch a same-size content change mtime would miss");
    assert_eq!(fs::read(dst.join("f.txt").as_std_path()).unwrap(), b"aaaaaaaaaa");
}

#[test]
fn nested_directories_are_created() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src.join("a/b"), None).unwrap();
    vfs.mkdir(&dst, None).unwrap();
    fs::write(src.join("a/b/deep.txt").as_std_path(), b"deep").unwrap();

    run_sync(&vfs, &src, &dst, false);

    assert_eq!(fs::read(dst.join("a/b/deep.txt").as_std_path()).unwrap(), b"deep");
}

#[test]
fn dest_only_file_kept_without_delete_extraneous_removed_with_it() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src, None).unwrap();
    vfs.mkdir(&dst, None).unwrap();
    fs::write(dst.join("extra.txt").as_std_path(), b"only on dest").unwrap();

    let plan = run_sync(&vfs, &src, &dst, false);
    assert_eq!(plan.items_to_delete, 0);
    assert!(dst.join("extra.txt").as_std_path().exists(), "must not delete without delete_extraneous");

    let plan = run_sync(&vfs, &src, &dst, true);
    assert_eq!(plan.items_to_delete, 1);
    assert!(!dst.join("extra.txt").as_std_path().exists(), "must delete once delete_extraneous is set");
}

#[test]
fn type_mismatch_file_replaced_by_directory() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src.join("conflict"), None).unwrap();
    fs::write(src.join("conflict/inner.txt").as_std_path(), b"inner").unwrap();
    vfs.mkdir(&dst, None).unwrap();
    fs::write(dst.join("conflict").as_std_path(), b"was a plain file").unwrap();

    run_sync(&vfs, &src, &dst, false);

    assert!(dst.join("conflict").as_std_path().is_dir());
    assert_eq!(fs::read(dst.join("conflict/inner.txt").as_std_path()).unwrap(), b"inner");
}

#[test]
fn type_mismatch_directory_replaced_by_file() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src, None).unwrap();
    fs::write(src.join("conflict").as_std_path(), b"now a file").unwrap();
    vfs.mkdir(&dst.join("conflict/nested"), None).unwrap();
    fs::write(dst.join("conflict/nested/old.txt").as_std_path(), b"old").unwrap();

    run_sync(&vfs, &src, &dst, false);

    assert!(dst.join("conflict").as_std_path().is_file());
    assert_eq!(fs::read(dst.join("conflict").as_std_path()).unwrap(), b"now a file");
}

#[test]
fn mismatched_symlink_is_recreated() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let vfs = LocalFs::new();

    let src = root.join("src");
    let dst = root.join("dst");
    vfs.mkdir(&src, None).unwrap();
    vfs.mkdir(&dst, None).unwrap();
    fs::write(root.join("target_a").as_std_path(), b"a").unwrap();
    fs::write(root.join("target_b").as_std_path(), b"b").unwrap();
    symlink(root.join("target_a").as_std_path(), src.join("link").as_std_path()).unwrap();
    symlink(root.join("target_b").as_std_path(), dst.join("link").as_std_path()).unwrap();

    run_sync(&vfs, &src, &dst, false);

    let resolved = fs::read_link(dst.join("link").as_std_path()).unwrap();
    assert_eq!(resolved, root.join("target_a").as_std_path());
}
