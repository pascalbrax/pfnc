//! End-to-end proof of the Phase 3 deployment mechanism: a real local
//! sshd, a real SFTP upload of the just-built `pfnc-agent` binary, a real
//! SSH exec of it, and — to prove the whole chain actually works, not just
//! that we parsed some text — a real QUIC handshake against the freshly
//! deployed instance.

mod support;

use pfnc_vfs_sftp::{deploy_and_start, kill_remote_process, AcceptNewPolicy, SftpFs};
use support::TestSshd;

/// `pfnc-agent-linux` is a dev-dependency of this crate (for its
/// `connect`/`hello` test helpers below), but `CARGO_BIN_EXE_<name>` is
/// only set for a crate's *own* binary targets, not a dependency's — so we
/// locate the just-built `pfnc-agent` binary relative to this test
/// binary's own path instead: both land in the same `target/<profile>/`
/// directory (this one under `deps/`, the agent binary directly in it).
fn agent_binary_path() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    let target_dir = test_exe.parent().and_then(|p| p.parent()).expect("target dir two levels up from test exe");
    let path = target_dir.join("pfnc-agent");
    assert!(path.exists(), "expected agent binary at {path:?} (build pfnc-agent-linux first)");
    path
}

async fn hello_over_quic(deployed: &pfnc_vfs_sftp::DeployedAgent) -> anyhow::Result<(u32, bool)> {
    let (endpoint, connection) = pfnc_agent_linux::connect(
        format!("127.0.0.1:{}", deployed.port).parse()?,
        "pfnc-agent",
        &deployed.cert_der,
    )
    .await?;
    let hello = pfnc_agent_linux::hello(&connection, pfnc_agent_linux::PROTOCOL_VERSION).await?;
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
    Ok(hello)
}

#[test]
#[ignore = "spawns a real local sshd and a real subprocess; run with `cargo test -p pfnc-vfs-sftp -- --ignored`"]
fn deploy_uploads_execs_and_a_real_quic_handshake_succeeds() {
    let sshd = TestSshd::start();
    let vfs = SftpFs::connect(&sshd.profile("deploy1"), &AcceptNewPolicy, &sshd.known_hosts_path).unwrap();
    let remote_dir = sshd.scratch_dir();

    let (deployed, _channel) =
        deploy_and_start(&vfs, &agent_binary_path(), remote_dir.to_str().unwrap()).expect("deploy_and_start");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(hello_over_quic(&deployed));

    kill_remote_process(&vfs, deployed.pid).expect("kill_remote_process");

    let (server_version, compatible) = result.expect("real QUIC handshake against the deployed agent");
    assert_eq!(server_version, pfnc_agent_linux::PROTOCOL_VERSION);
    assert!(compatible);
}
