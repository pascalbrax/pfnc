//! End-to-end proof that the QUIC fast path wired into `negotiate_transport`
//! actually gets used by a real `copy_job` between a real `LocalFs` and a
//! real `SftpFs` — not just that the pieces work standalone (see
//! `agent_deploy.rs` and `pfnc-agent-linux`'s own tests for those). A real
//! local sshd stands in for the remote host; since it runs on the same
//! machine, the "remote" file this test writes is just read back with
//! `std::fs::read` afterward to confirm the bytes actually arrived intact,
//! independent of which transport moved them.

mod support;

use camino::Utf8PathBuf;
use pfnc_core::job::{copy_job, CancellationToken, JobProgress};
use pfnc_core::{Location, Vfs};
use pfnc_vfs_local::LocalFs;
use pfnc_vfs_sftp::{AcceptNewPolicy, SftpFs};
use support::TestSshd;

fn noop_report(_: JobProgress) {}

#[test]
#[ignore = "spawns a real local sshd and a real subprocess; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn copy_job_local_to_remote_actually_uses_the_deployed_quic_agent() {
    let sshd = TestSshd::start();
    let vfs = SftpFs::connect(&sshd.profile("quic1"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();
    let remote_dir = sshd.scratch_dir();

    // Forces (and caches) the same deploy+connect attempt `copy_job` below
    // will reuse via `negotiate_transport` — asserting `Some` here is what
    // proves the OS probe, upload, exec, and QUIC handshake against this
    // real local sshd all actually succeeded, not just that they *could*.
    let agent = vfs.fast_transport();
    assert!(agent.is_some(), "expected a real QUIC agent to deploy and connect against a local Linux sshd");
    drop(agent);

    // `connection_info()` (what the F1 Help box reads) should now reflect
    // the exact same real deployment: the test sshd runs on this machine,
    // so the remote OS probe genuinely reports this host's OS.
    let info = vfs.connection_info().expect("SftpFs must report connection info once connected");
    assert_eq!(info.protocol, "SFTP");
    assert_eq!(info.remote_os.as_deref(), Some("Linux"), "the local test sshd runs on this (Linux CI/dev) host");
    let quic = info.quic.expect("connection_info should report the already-deployed QUIC agent");
    assert!(quic.local_port.is_some() && quic.local_port != Some(0), "should report a real local UDP port");
    assert!(quic.remote_port > 0, "should report the real remote agent port");
    assert!(quic.agent_pid > 0, "should report the real deployed pfnc-agent pid");

    let local_dir = tempfile::tempdir().unwrap();
    let local_vfs = LocalFs::new();
    let local_root = Utf8PathBuf::from_path_buf(local_dir.path().to_path_buf()).unwrap();
    let src_file = local_root.join("payload.bin");
    let payload: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src_file.as_std_path(), &payload).unwrap();

    let dest_dir = Utf8PathBuf::from_path_buf(remote_dir.clone()).unwrap();
    let cancel = CancellationToken::new();
    copy_job(
        &local_vfs,
        &vfs,
        &Location::Local,
        &Location::Remote { profile_id: "quic1".to_string() },
        &[src_file],
        &dest_dir,
        true,
        &cancel,
        &noop_report,
    )
    .unwrap();

    let landed = remote_dir.join("payload.bin");
    assert_eq!(std::fs::read(&landed).unwrap(), payload, "file content must survive the QUIC fast path intact");

    // `vfs`'s `Drop` impl kills the deployed remote agent process when it
    // goes out of scope here (see `SftpFs::drop` in `crates/vfs-sftp/src/
    // lib.rs`) — verified externally via a `ps aux` check after the test
    // suite runs, same bar as the `agent_deploy` integration test.
}

#[test]
#[ignore = "spawns a real local sshd and a real subprocess; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn copy_job_copies_a_nested_directory_tree_over_the_quic_fast_path() {
    let sshd = TestSshd::start();
    let vfs = SftpFs::connect(&sshd.profile("quictree"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();
    let remote_dir = sshd.scratch_dir();

    let agent = vfs.fast_transport();
    assert!(agent.is_some(), "expected a real QUIC agent to deploy and connect against a local Linux sshd");
    drop(agent);

    let local_dir = tempfile::tempdir().unwrap();
    let local_vfs = LocalFs::new();
    let local_root = Utf8PathBuf::from_path_buf(local_dir.path().to_path_buf()).unwrap();

    // src/
    //   top.txt
    //   sub1/
    //     a.txt
    //     sub2/
    //       b.txt
    //       sub3/
    //         c.txt
    let src_dir = local_root.join("src");
    std::fs::create_dir_all(src_dir.join("sub1/sub2/sub3").as_std_path()).unwrap();
    std::fs::write(src_dir.join("top.txt").as_std_path(), b"top").unwrap();
    std::fs::write(src_dir.join("sub1/a.txt").as_std_path(), b"a").unwrap();
    std::fs::write(src_dir.join("sub1/sub2/b.txt").as_std_path(), b"b").unwrap();
    std::fs::write(src_dir.join("sub1/sub2/sub3/c.txt").as_std_path(), b"c").unwrap();

    let dest_dir = Utf8PathBuf::from_path_buf(remote_dir.clone()).unwrap();
    let cancel = CancellationToken::new();
    copy_job(
        &local_vfs,
        &vfs,
        &Location::Local,
        &Location::Remote { profile_id: "quictree".to_string() },
        &[src_dir],
        &dest_dir,
        true,
        &cancel,
        &noop_report,
    )
    .unwrap();

    let landed = remote_dir.join("src");
    assert_eq!(std::fs::read(landed.join("top.txt")).unwrap(), b"top");
    assert_eq!(std::fs::read(landed.join("sub1/a.txt")).unwrap(), b"a");
    assert_eq!(std::fs::read(landed.join("sub1/sub2/b.txt")).unwrap(), b"b");
    assert_eq!(std::fs::read(landed.join("sub1/sub2/sub3/c.txt")).unwrap(), b"c");
}

#[test]
#[ignore = "spawns a real local sshd and a real subprocess; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn copy_job_copies_a_large_directory_tree_over_the_quic_fast_path() {
    let sshd = TestSshd::start();
    let vfs = SftpFs::connect(&sshd.profile("quicbig"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();
    let remote_dir = sshd.scratch_dir();

    let agent = vfs.fast_transport();
    assert!(agent.is_some(), "expected a real QUIC agent to deploy and connect against a local Linux sshd");
    drop(agent);

    let local_dir = tempfile::tempdir().unwrap();
    let local_vfs = LocalFs::new();
    let local_root = Utf8PathBuf::from_path_buf(local_dir.path().to_path_buf()).unwrap();

    let src_dir = local_root.join("bigsrc");
    let mut expected: Vec<Utf8PathBuf> = Vec::new();
    for dir_idx in 0..10 {
        let sub = src_dir.join(format!("dir{dir_idx}"));
        std::fs::create_dir_all(sub.as_std_path()).unwrap();
        for file_idx in 0..10 {
            let file = sub.join(format!("file{file_idx}.txt"));
            std::fs::write(file.as_std_path(), format!("dir{dir_idx}-file{file_idx}")).unwrap();
            expected.push(file);
        }
    }

    let dest_dir = Utf8PathBuf::from_path_buf(remote_dir.clone()).unwrap();
    let cancel = CancellationToken::new();
    copy_job(
        &local_vfs,
        &vfs,
        &Location::Local,
        &Location::Remote { profile_id: "quicbig".to_string() },
        std::slice::from_ref(&src_dir),
        &dest_dir,
        true,
        &cancel,
        &noop_report,
    )
    .unwrap();

    let mut missing = Vec::new();
    for file in &expected {
        let rel = file.strip_prefix(&local_root).unwrap();
        let landed = remote_dir.join(rel.as_str());
        if !landed.exists() {
            missing.push(landed);
        }
    }
    assert!(missing.is_empty(), "missing {} of {} files after QUIC copy: {:?}", missing.len(), expected.len(), missing);
}

#[test]
#[ignore = "spawns a real local sshd; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn connection_info_reports_remote_os_even_without_probing_quic() {
    let sshd = TestSshd::start();
    let vfs = SftpFs::connect(&sshd.profile("osonly"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();

    // No `fast_transport()`/`copy_job` call here — only the OS probe, which
    // `VfsRegistry::connect_and_cache` runs unconditionally (unlike the
    // QUIC agent probe, which is gated by `enable_quic_fast_path`) so the
    // F1 Help box can show the remote OS regardless of that setting.
    vfs.warm_remote_os_probe();

    let info = vfs.connection_info().expect("SftpFs must report connection info once connected");
    assert_eq!(info.remote_os.as_deref(), Some("Linux"));
    assert!(info.quic.is_none(), "QUIC was never probed, so there must be nothing to report for it");
}
