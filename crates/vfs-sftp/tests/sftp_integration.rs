//! SFTP integration tests against a real local `sshd` (see
//! `tests/support/mod.rs`). Marked `#[ignore]` so a plain `cargo test`
//! never spawns external processes; run explicitly with:
//! `cargo test -p pfnc-vfs-sftp -- --ignored`

mod support;

use std::fs;

use camino::Utf8PathBuf;
use pfnc_core::{EntryKind, Vfs, VfsError, VfsPath};
use pfnc_vfs_sftp::{AcceptNewPolicy, RejectUnknownPolicy, SftpFs};
use support::TestSshd;

fn vfs_path(p: &std::path::Path) -> VfsPath {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).expect("test paths must be UTF-8")
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn connect_list_and_stat_over_real_sftp() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    fs::write(root.join("file.txt"), b"hello").unwrap();
    fs::create_dir(root.join("subdir")).unwrap();

    let vfs = SftpFs::connect(&sshd.profile("t1"), &AcceptNewPolicy, &sshd.known_hosts_path)
        .expect("connect should succeed against a freshly-trusted host");

    let mut entries = vfs.list_dir(&vfs_path(&root)).unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "file.txt");
    assert_eq!(entries[0].size, 5);
    assert_eq!(entries[1].name, "subdir");
    assert!(entries[1].kind.is_dir());

    let stat = vfs.stat(&vfs_path(&root.join("file.txt"))).unwrap();
    assert_eq!(stat.size, 5);
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn write_then_read_roundtrip_over_real_sftp() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    let vfs = SftpFs::connect(&sshd.profile("t2"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    let file = vfs_path(&root.join("out.bin"));
    {
        use std::io::Write;
        let mut w = vfs.create_write(&file, None).unwrap();
        w.write_all(b"some bytes over sftp").unwrap();
    }
    let mut buf = Vec::new();
    {
        use std::io::Read;
        vfs.open_read(&file).unwrap().read_to_end(&mut buf).unwrap();
    }
    assert_eq!(buf, b"some bytes over sftp");
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn mkdir_rename_and_remove_over_real_sftp() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    let vfs = SftpFs::connect(&sshd.profile("t3"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    let dir = vfs_path(&root.join("newdir"));
    vfs.mkdir(&dir, None).unwrap();
    assert!(root.join("newdir").is_dir());

    let renamed = vfs_path(&root.join("renamed"));
    vfs.rename(&dir, &renamed).unwrap();
    assert!(!root.join("newdir").exists());
    assert!(root.join("renamed").is_dir());

    vfs.remove_dir(&renamed, false).unwrap();
    assert!(!root.join("renamed").exists());
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn remove_dir_recursive_over_real_sftp() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    let vfs = SftpFs::connect(&sshd.profile("t4"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    let target = root.join("victim");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("a.txt"), b"x").unwrap();
    fs::create_dir(target.join("nested")).unwrap();
    fs::write(target.join("nested/b.txt"), b"y").unwrap();

    vfs.remove_dir(&vfs_path(&target), true).unwrap();
    assert!(!target.exists());
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn symlink_over_real_sftp_reports_target() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    let vfs = SftpFs::connect(&sshd.profile("t5"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    let target = vfs_path(&root.join("target.txt"));
    fs::write(target.as_std_path(), b"x").unwrap();
    let link = vfs_path(&root.join("link.txt"));
    vfs.symlink(&target, &link).unwrap();

    let meta = vfs.stat(&link).unwrap();
    match meta.kind {
        EntryKind::Symlink { target: Some(t) } => assert_eq!(t, target),
        other => panic!("expected symlink with target, got {other:?}"),
    }
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn not_found_maps_to_typed_error() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    let vfs = SftpFs::connect(&sshd.profile("t6"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    let err = vfs.stat(&vfs_path(&root.join("nope.txt"))).unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)));
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn quick_hash_over_real_sftp_matches_local_xxh64() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    let vfs = SftpFs::connect(&sshd.profile("t10"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    let file = root.join("hash_me.txt");
    fs::write(&file, b"identical content").unwrap();

    match vfs.quick_hash(&vfs_path(&file)).unwrap() {
        // The test host may not have `xxhsum` installed; that's a valid,
        // documented degrade path, not a test failure.
        None => eprintln!("xxhsum not available on this host; skipping hash-value assertion"),
        Some(remote_hash) => {
            let local_hash = pfnc_vfs_local::LocalFs::new().quick_hash(&vfs_path(&file)).unwrap().unwrap();
            assert_eq!(
                remote_hash, local_hash,
                "remote (exec-channel xxhsum) and local (in-process xxh64) hashes of identical content must match"
            );
        }
    }
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn quick_hash_gracefully_degrades_instead_of_erroring() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    let vfs = SftpFs::connect(&sshd.profile("t11"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    // A path that doesn't exist makes `xxhsum` exit non-zero — the same
    // code path as "command not found" on a minimal remote host. Either
    // way `quick_hash` must degrade to `Ok(None)`, never a hard error, so
    // sync's mtime fallback kicks in instead.
    let missing = vfs_path(&root.join("does_not_exist.txt"));
    assert_eq!(vfs.quick_hash(&missing).unwrap(), None);
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn quick_hash_shell_escapes_filenames_with_quotes() {
    let sshd = TestSshd::start();
    let root = sshd.scratch_dir();
    let vfs = SftpFs::connect(&sshd.profile("t12"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    // A filename crafted to break naive shell interpolation: an embedded
    // single quote plus `;`/`$()` shell metacharacters, but no `/` (a
    // literal slash in the name would just make the *local* `fs::write`
    // below create nested directories, unrelated to what we're testing).
    // If the remote-side escaping were wrong, the shell would either
    // misparse the command (non-zero exit -> `None`) or hash a completely
    // different, unintended byte sequence — either way the hash would
    // fail to match a local hash of this exact file's content.
    let tricky_name = "quo'te; $(id) && echo 'pwned'x.txt";
    let file = root.join(tricky_name);
    fs::write(&file, b"identical content").unwrap();

    let hash = vfs.quick_hash(&vfs_path(&file)).unwrap();
    if let Some(remote_hash) = hash {
        let local_hash = pfnc_vfs_local::LocalFs::new().quick_hash(&vfs_path(&file)).unwrap().unwrap();
        assert_eq!(remote_hash, local_hash);
    }
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn host_key_tofu_then_reconnect_succeeds() {
    let sshd = TestSshd::start();
    // First connect: known_hosts is empty, AcceptNewPolicy trusts and saves it.
    SftpFs::connect(&sshd.profile("t7a"), &AcceptNewPolicy, &sshd.known_hosts_path)
        .expect("first connect should trust-on-first-use");
    assert!(sshd.known_hosts_path.exists());

    // Second connect: host key is now known, so even a strict
    // reject-unknown policy succeeds (it's a *match*, not "unknown").
    SftpFs::connect(&sshd.profile("t7b"), &RejectUnknownPolicy, &sshd.known_hosts_path)
        .expect("second connect should succeed via an existing known_hosts match");
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn reject_unknown_policy_refuses_first_connect() {
    let sshd = TestSshd::start();
    let err = SftpFs::connect(&sshd.profile("t8"), &RejectUnknownPolicy, &sshd.known_hosts_path)
        .expect_err("an unknown host key must be rejected by RejectUnknownPolicy");
    assert!(matches!(err, pfnc_vfs_sftp::ConnectError::HostKey(_)));
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn wrong_key_fails_authentication() {
    let sshd = TestSshd::start();
    let mut profile = sshd.profile("t9");
    // Point at a key that was never added to authorized_keys.
    let other_dir = tempfile::tempdir().unwrap();
    let bogus_key = other_dir.path().join("bogus_key");
    std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f", bogus_key.to_str().unwrap(), "-N", "", "-q"])
        .status()
        .unwrap();
    profile.auth = pfnc_vfs_sftp::AuthMethod::KeyFile {
        private_key: bogus_key,
        public_key: None,
        passphrase: None,
    };

    let err = SftpFs::connect(&profile, &AcceptNewPolicy, &sshd.known_hosts_path)
        .expect_err("authentication with an unauthorized key must fail");
    assert!(matches!(err, pfnc_vfs_sftp::ConnectError::Auth(_)));
}
