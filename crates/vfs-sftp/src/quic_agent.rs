//! Lazily deploys and connects to a QUIC agent (see `pfnc-agent-linux`) over
//! the same SSH session `SftpFs` already has open, so a Local<->Remote
//! transfer can use `pfnc-core`'s faster `RemoteFileAgent` path instead of
//! the plain SFTP data channel. Any failure at any step — the remote-OS
//! probe, a missing local agent binary, deployment, or the QUIC handshake —
//! falls back to `None`, cached (via `SftpFs::quic_agent`'s `OnceLock`) so a
//! doomed connection isn't retried once per file.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use pfnc_core::job::JobError;
use pfnc_core::{RemoteFileAgent, VfsError, VfsPath};

use crate::deploy::{deploy_and_start, kill_remote_process, DeployedAgent};
use crate::SftpFs;

/// Where on the remote host the agent binary gets uploaded to run from.
/// Simple and always writable/executable on virtually every Linux host; a
/// noexec-mounted `/tmp` is a known edge case that just falls back to SFTP.
const REMOTE_DEPLOY_DIR: &str = "/tmp";

/// A live, already-authenticated QUIC connection to a deployed `pfnc-agent`,
/// plus everything needed to keep it running and to tear it down. Owns a
/// dedicated single-thread Tokio runtime so `read_file`/`write_file` can
/// present a synchronous `RemoteFileAgent` interface to `pfnc-core`, which
/// deliberately has no async runtime of its own.
pub(crate) struct QuicAgentHandle {
    runtime: tokio::runtime::Runtime,
    // Kept alive so the connection isn't torn down early; also read by
    // `local_port` for display purposes.
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    // Keeps the remote `pfnc-agent` process running (see `deploy_and_start`'s
    // docs) until `SftpFs::drop` kills it via a fresh channel.
    _agent_channel: ssh2::Channel,
    pid: u32,
    remote_port: u16,
}

impl QuicAgentHandle {
    /// The remote PID of the deployed `pfnc-agent` process — used by
    /// `SftpFs::drop` to kill it via a fresh exec channel, and shown in the
    /// F1 Help box as "who is listening" on the remote host.
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// The port the remote `pfnc-agent` is listening on.
    pub(crate) fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// This process's own local UDP port for the connection, if the
    /// endpoint's bound address can still be determined.
    pub(crate) fn local_port(&self) -> Option<u16> {
        self.endpoint.local_addr().ok().map(|addr| addr.port())
    }
}

impl RemoteFileAgent for QuicAgentHandle {
    fn read_file(&self, path: &VfsPath, writer: &mut dyn Write) -> Result<u64, JobError> {
        self.runtime
            .block_on(pfnc_agent_linux::read_file(&self.connection, path.as_str(), |chunk| writer.write_all(chunk)))
            .map_err(|e| JobError::Vfs(VfsError::Io(std::io::Error::other(e.to_string()))))
    }

    fn write_file(&self, path: &VfsPath, mode: Option<u32>, reader: &mut dyn Read) -> Result<(), JobError> {
        self.runtime
            .block_on(pfnc_agent_linux::write_file(&self.connection, path.as_str(), mode, |buf| reader.read(buf)))
            .map(|_| ())
            .map_err(|e| JobError::Vfs(VfsError::Io(std::io::Error::other(e.to_string()))))
    }
}

/// Whether a `uname -s` output indicates a host `pfnc-agent-linux` can run
/// on. Only Linux is supported today (`pfnc-agent-macos`/`pfnc-agent-bsd`
/// are still placeholders) — kept as a pure function so this doesn't need a
/// live SSH session to test.
pub(crate) fn os_supports_quic_agent(uname_output: &str) -> bool {
    uname_output.trim() == "Linux"
}

/// Finds the `pfnc-agent` binary this process was built alongside. In the
/// real `pfnc` binary that's a direct sibling (`target/<profile>/pfnc` next
/// to `target/<profile>/pfnc-agent`); from a `cargo test` binary it's one
/// directory further up (`target/<profile>/deps/<test>` next to
/// `target/<profile>/pfnc-agent`, since test binaries live in `deps/`) —
/// checking both covers the same trick the `agent_deploy` integration test
/// uses, since `CARGO_BIN_EXE_*` only works for a crate's own binaries, not
/// a dependency's. Returns `None` (not an error) when it's missing either
/// way, so callers fall back to the plain SFTP transport.
fn agent_binary_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..2 {
        let candidate = dir.join("pfnc-agent");
        if candidate.exists() {
            return Some(candidate);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

impl SftpFs {
    /// Runs `uname -s` on a fresh exec channel. `None` on any exec/channel
    /// failure — callers treat that the same as "not Linux" (fall back).
    fn exec_probe_remote_os(&self) -> Option<String> {
        let mut channel = self.session.channel_session().ok()?;
        channel.exec("uname -s").ok()?;
        let mut stdout = String::new();
        std::io::Read::read_to_string(&mut channel, &mut stdout).ok()?;
        channel.wait_close().ok()?;
        Some(stdout.trim().to_string())
    }

    /// Warms `remote_os_cache` on first call (cheap: one exec-channel round
    /// trip, no upload/exec of the agent binary) — unlike the QUIC agent
    /// probe below, callers run this unconditionally after every connect
    /// (see `VfsRegistry::connect_and_cache`), regardless of
    /// `enable_quic_fast_path`, since it's just useful diagnostic info
    /// (shown in the F1 Help box) independent of whether the fast path
    /// itself is enabled.
    pub fn warm_remote_os_probe(&self) {
        self.remote_os_cache.get_or_init(|| self.exec_probe_remote_os());
    }

    /// Lazily deploys+connects a QUIC agent on first call, caching the
    /// result (including a cached failure) for the lifetime of this
    /// connection — see the module docs for the fallback chain.
    pub(crate) fn quic_agent(&self) -> Option<Arc<QuicAgentHandle>> {
        self.quic_agent_cache.get_or_init(|| self.deploy_and_connect_quic_agent()).clone()
    }

    fn deploy_and_connect_quic_agent(&self) -> Option<Arc<QuicAgentHandle>> {
        self.warm_remote_os_probe();
        let Some(uname) = self.remote_os_cache.get().cloned().flatten() else {
            tracing::warn!(host = %self.host, "QUIC fast path unavailable: remote OS probe (`uname -s` over SSH) failed");
            return None;
        };
        if !os_supports_quic_agent(&uname) {
            tracing::info!(host = %self.host, remote_os = %uname, "QUIC fast path not attempted: remote OS is not Linux");
            return None;
        }

        let Some(binary_path) = agent_binary_path() else {
            tracing::warn!(
                host = %self.host,
                "QUIC fast path unavailable: no pfnc-agent binary found next to the running pfnc executable \
                 (it must be built and placed alongside pfnc, e.g. `cargo build --workspace`)"
            );
            return None;
        };

        let (deployed, agent_channel) = match deploy_and_start(self, &binary_path, REMOTE_DEPLOY_DIR) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(host = %self.host, error = %e, "QUIC fast path unavailable: failed to deploy pfnc-agent over SSH");
                return None;
            }
        };

        match connect_quic_agent(&self.host, &deployed, agent_channel) {
            Some(handle) => Some(Arc::new(handle)),
            None => {
                tracing::warn!(
                    host = %self.host,
                    port = deployed.port,
                    "QUIC fast path unavailable: pfnc-agent was deployed but the QUIC connection to it failed \
                     (often a firewall/NAT blocking outbound/inbound UDP on this port)"
                );
                let _ = kill_remote_process(self, deployed.pid);
                None
            }
        }
    }
}

/// How long to wait for the whole handshake+hello exchange before giving up.
/// `quinn`'s own internal idle/handshake timeout is much longer than this
/// (tens of seconds) — appropriate for a long-lived connection, but this is
/// a background best-effort probe for a fast path that's expected to work
/// near-instantly on a real LAN; a host that silently drops/filters UDP
/// (common on hardened VPS firewalls) should fail fast, not tie up a
/// background job thread for as long as quinn itself would wait.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn connect_quic_agent(host: &str, deployed: &DeployedAgent, agent_channel: ssh2::Channel) -> Option<QuicAgentHandle> {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build the QUIC client's Tokio runtime");
            return None;
        }
    };
    // `host` is whatever the user typed into the Connect form (often a
    // DNS/mDNS name or an `/etc/hosts` entry, not a literal IP) — the same
    // thing `SftpFs::connect`'s own `TcpStream::connect` already resolves
    // via the system resolver. `SocketAddr::parse` only ever accepts a
    // literal numeric IP and would silently fail for every non-IP host, so
    // this must go through `ToSocketAddrs` (a real, possibly blocking,
    // resolution) instead — fine here since this whole function already
    // runs on a background job thread, never the UI thread.
    use std::net::ToSocketAddrs;
    let addr = match (host, deployed.port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(addr) => addr,
            None => {
                tracing::warn!(host, port = deployed.port, "resolving remote host/port produced no addresses");
                // No runtime work was ever started; a plain drop is fine.
                return None;
            }
        },
        Err(e) => {
            tracing::warn!(host, port = deployed.port, error = %e, "failed to resolve remote host/port to a socket address");
            return None;
        }
    };
    let cert_der = deployed.cert_der.clone();

    // `tokio::time::timeout` must be constructed from *within* a running
    // runtime (it looks up the current reactor/timer immediately, not
    // lazily when polled) — so the whole thing, including the `timeout(..)`
    // call itself, has to live inside the `block_on`'d async block, not be
    // built as a plain argument to `block_on` from this sync function.
    let outcome = runtime.block_on(async {
        tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let (endpoint, connection) = match pfnc_agent_linux::connect(addr, "pfnc-agent", &cert_der).await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "QUIC handshake with pfnc-agent failed");
                    return None;
                }
            };
            let (server_version, compatible) =
                match pfnc_agent_linux::hello(&connection, pfnc_agent_linux::PROTOCOL_VERSION).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!(%addr, error = %e, "pfnc-agent hello exchange failed after a successful QUIC handshake");
                        return None;
                    }
                };
            if !compatible {
                tracing::warn!(
                    server_version,
                    client_version = pfnc_agent_linux::PROTOCOL_VERSION,
                    "pfnc-agent reported an incompatible protocol version"
                );
                return None;
            }
            Some((endpoint, connection))
        })
        .await
    });

    let (endpoint, connection) = match outcome {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            // A specific reason was already logged above.
            runtime.shutdown_background();
            return None;
        }
        Err(_elapsed) => {
            tracing::warn!(
                %addr,
                timeout_secs = HANDSHAKE_TIMEOUT.as_secs(),
                "QUIC handshake with pfnc-agent timed out; giving up early rather than waiting on \
                 quinn's own, much longer, internal timeout"
            );
            runtime.shutdown_background();
            return None;
        }
    };

    Some(QuicAgentHandle {
        runtime,
        endpoint,
        connection,
        _agent_channel: agent_channel,
        pid: deployed.pid,
        remote_port: deployed.port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_supports_quic_agent_recognizes_linux() {
        assert!(os_supports_quic_agent("Linux\n"));
        assert!(os_supports_quic_agent("Linux"));
    }

    #[test]
    fn os_supports_quic_agent_rejects_other_unix_variants() {
        assert!(!os_supports_quic_agent("Darwin\n"));
        assert!(!os_supports_quic_agent("FreeBSD\n"));
        assert!(!os_supports_quic_agent("OpenBSD\n"));
        assert!(!os_supports_quic_agent("SunOS\n"));
    }

    #[test]
    fn os_supports_quic_agent_rejects_garbage_or_empty_output() {
        assert!(!os_supports_quic_agent(""));
        assert!(!os_supports_quic_agent("\n"));
        assert!(!os_supports_quic_agent("not a real uname output"));
    }
}
