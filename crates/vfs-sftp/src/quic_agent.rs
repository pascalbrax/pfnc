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

/// How many `pfnc_agent_linux::CHUNK_SIZE` chunks may sit buffered between
/// the scoped local-I/O thread and the QUIC connection at once — see the
/// module-level pipelining docs above `read_file`/`write_file` below. Purely
/// an internal implementation constant (bounds a small, fixed amount of
/// read-ahead memory — `PIPELINE_DEPTH * CHUNK_SIZE`, 1 MiB at the current
/// chunk size); not worth exposing as user-facing config.
const PIPELINE_DEPTH: usize = 4;

/// Log `quinn`'s connection stats every this-many chunks during a transfer
/// (in addition to once at the end), not just at the end — a single
/// end-of-transfer snapshot can't distinguish "the congestion window grew
/// throughout and this is just its final value" from "it never grew at
/// all," which matters a great deal for diagnosing a throughput cap. At the
/// current `CHUNK_SIZE` (256 KiB) this logs roughly every 10 MiB.
const STATS_LOG_INTERVAL_CHUNKS: u64 = 40;

impl RemoteFileAgent for QuicAgentHandle {
    /// Downloads `path` from the agent, writing it to `writer`. `writer` is
    /// `&mut dyn Write` with no `Send` bound (matching `pfnc-core`'s
    /// `Vfs`/`RemoteFileAgent` traits), so it can never cross a thread
    /// boundary — instead, the QUIC receive loop itself runs on a second,
    /// scoped OS thread (everything it touches — `self`, `path` — is already
    /// `Send`/`Sync`, required by `RemoteFileAgent`'s own supertrait bound),
    /// forwarding chunks back to *this* thread over a small bounded channel
    /// for the actual (possibly slow/blocking) local disk write. See
    /// `write_file` below for why this pipelining exists.
    fn read_file(&self, path: &VfsPath, writer: &mut dyn Write) -> Result<u64, JobError> {
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(PIPELINE_DEPTH);

            let net_thread = scope.spawn(move || {
                let mut chunks_seen = 0u64;
                let mut bytes_seen = 0u64;
                let on_chunk = |chunk: &[u8]| -> std::io::Result<()> {
                    chunks_seen += 1;
                    bytes_seen += chunk.len() as u64;
                    if chunks_seen % STATS_LOG_INTERVAL_CHUNKS == 0 {
                        log_connection_stats(&self.connection, Some(bytes_seen));
                    }
                    tx.send(Ok(chunk.to_vec()))
                        .map_err(|_| std::io::Error::other("local writer stopped accepting data"))
                };
                let result = self.runtime.block_on(pfnc_agent_linux::read_file(&self.connection, path.as_str(), on_chunk));
                log_connection_stats(&self.connection, None);
                result
            });

            let mut total = 0u64;
            let mut write_err = None;
            loop {
                match rx.recv() {
                    Ok(Ok(chunk)) => match writer.write_all(&chunk) {
                        Ok(()) => total += chunk.len() as u64,
                        Err(e) => {
                            write_err = Some(e);
                            break;
                        }
                    },
                    // The network side only ever sends `Ok(_)` on this leg
                    // (its own errors surface via `net_thread`'s own return
                    // value instead) — kept for symmetry/robustness only.
                    Ok(Err(e)) => {
                        write_err = Some(e);
                        break;
                    }
                    Err(_) => break, // channel closed: network side is done.
                }
            }
            // Explicit, not left to fall out at the end of this block: if we
            // broke out of the loop early on a local write error, the
            // network thread's `on_chunk` may still be parked on a full
            // channel with nothing left to drain it — dropping `rx` here,
            // *before* `join()`, is what unblocks it (its `tx.send` starts
            // returning `Err` immediately once the receiver is gone) so the
            // join below can't deadlock.
            drop(rx);

            let net_result = net_thread.join().expect("QUIC download's network thread panicked");
            match (net_result, write_err) {
                (Ok(total_read), None) => {
                    debug_assert_eq!(total_read, total, "bytes read over QUIC must match bytes written locally");
                    Ok(total)
                }
                (Err(e), _) => Err(JobError::Vfs(VfsError::Io(std::io::Error::other(e.to_string())))),
                (Ok(_), Some(e)) => Err(JobError::Vfs(VfsError::Io(e))),
            }
        })
    }

    /// Uploads `path` to the agent, reading it from `reader`. Same shape as
    /// `read_file` above, mirrored: `reader` (`&mut dyn Read`, no `Send`
    /// bound) stays on this thread, and the QUIC send loop runs on a second,
    /// scoped thread, fed local chunks over a small bounded channel.
    ///
    /// This pipelining exists because a real LAN measurement found the QUIC
    /// fast path *slower* than plain SFTP despite identical chunk sizes and
    /// a release build (see `roadmap.md`'s "Known limitations"): `quinn`'s
    /// own stats showed zero UDP GSO/GRO batching (`udp_tx_ios` ==
    /// `udp_tx_datagrams`) even though the kernel supports both — the
    /// strict, unpipelined read-then-await-send loop this replaced
    /// synchronously blocked the QUIC-driving thread on every single local
    /// disk read, starving `quinn`'s internal endpoint-driving task of the
    /// prompt, repeated polling it needs to batch outgoing datagrams. With a
    /// dedicated thread reading a few chunks ahead into a bounded queue, the
    /// QUIC side almost never actually waits on local I/O once the pipeline
    /// is primed. `std::thread::scope` (not `tokio::task::spawn_blocking`)
    /// is what makes this possible without changing `pfnc-core` at all:
    /// `reader` is borrowed with this call's own stack lifetime, not
    /// `'static`, which `spawn_blocking` requires but a scoped thread does
    /// not.
    fn write_file(&self, path: &VfsPath, mode: Option<u32>, reader: &mut dyn Read) -> Result<(), JobError> {
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(PIPELINE_DEPTH);

            let net_thread = scope.spawn(move || {
                let mut chunks_seen = 0u64;
                let mut bytes_seen = 0u64;
                let next_chunk = |buf: &mut [u8]| -> std::io::Result<usize> {
                    match rx.recv() {
                        Ok(Ok(chunk)) => {
                            chunks_seen += 1;
                            bytes_seen += chunk.len() as u64;
                            if chunks_seen % STATS_LOG_INTERVAL_CHUNKS == 0 {
                                log_connection_stats(&self.connection, Some(bytes_seen));
                            }
                            buf[..chunk.len()].copy_from_slice(&chunk);
                            Ok(chunk.len())
                        }
                        Ok(Err(e)) => Err(e),
                        Err(_) => Ok(0), // channel closed: local reader is done.
                    }
                };
                let result =
                    self.runtime.block_on(pfnc_agent_linux::write_file(&self.connection, path.as_str(), mode, next_chunk));
                log_connection_stats(&self.connection, None);
                result
            });

            loop {
                let mut buf = vec![0u8; pfnc_agent_linux::CHUNK_SIZE];
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send(Ok(buf)).is_err() {
                            break; // network thread gave up (e.g. a send error); stop reading.
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
            // Explicit, same reasoning as `read_file` above: if the network
            // thread's `next_chunk` is parked waiting on a full channel
            // because we broke out of this loop early, dropping `tx` here —
            // before `join()` — unblocks it rather than risking a deadlock.
            drop(tx);

            net_thread
                .join()
                .expect("QUIC upload's network thread panicked")
                .map(|_| ())
                .map_err(|e| JobError::Vfs(VfsError::Io(std::io::Error::other(e.to_string()))))
        })
    }
}

/// Logs `quinn`'s own transport-level stats for this connection right after
/// a transfer — real congestion-control/packet-loss/MTU numbers straight
/// from the QUIC state machine, rather than guessing about them. In
/// particular, `udp_tx.ios < udp_tx.datagrams` would confirm GSO batching is
/// actually happening (fewer syscalls than datagrams sent); `path.cwnd`
/// staying small or `path.lost_packets`/`congestion_events` climbing would
/// point at congestion control rather than raw crypto/syscall throughput.
/// `debug`-level: this is transport-internals diagnostic data (see
/// `roadmap.md`'s "Known limitations" for what it was used to root-cause),
/// not something a normal run needs to surface by default — set
/// `RUST_LOG=debug` to see it.
fn log_connection_stats(connection: &quinn::Connection, bytes_so_far: Option<u64>) {
    let stats = connection.stats();
    tracing::debug!(
        bytes_so_far,
        rtt_ms = stats.path.rtt.as_millis() as u64,
        cwnd = stats.path.cwnd,
        congestion_events = stats.path.congestion_events,
        lost_packets = stats.path.lost_packets,
        sent_packets = stats.path.sent_packets,
        current_mtu = stats.path.current_mtu,
        udp_tx_datagrams = stats.udp_tx.datagrams,
        udp_tx_ios = stats.udp_tx.ios,
        udp_rx_datagrams = stats.udp_rx.datagrams,
        udp_rx_ios = stats.udp_rx.ios,
        "quic connection stats"
    );
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
