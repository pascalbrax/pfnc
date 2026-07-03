//! Host-key verification against `~/.ssh/known_hosts`. Never silently
//! accepts a *changed* key (that's the actual MITM-detection guarantee);
//! unknown-host handling is pluggable via `HostKeyPolicy`.

use ssh2::{CheckResult, KnownHostFileKind, Session};

/// Decides what to do about a host key that isn't already in
/// `known_hosts`. A mismatched (changed) key is never delegated here — it
/// is always rejected, regardless of policy.
pub trait HostKeyPolicy: Send + Sync {
    fn accept_unknown_host(&self, host: &str, port: u16, fingerprint: &str) -> bool;
}

/// Auto-accepts and saves the key for hosts seen for the first time —
/// equivalent to OpenSSH's `StrictHostKeyChecking=accept-new`, a common,
/// defensible default for automation-style clients. It still rejects any
/// key that *mismatches* a previously trusted entry.
pub struct AcceptNewPolicy;

impl HostKeyPolicy for AcceptNewPolicy {
    fn accept_unknown_host(&self, _host: &str, _port: u16, _fingerprint: &str) -> bool {
        true
    }
}

/// Never trusts an unknown host; only connections to already-known hosts
/// succeed. Useful for tests and for a future "strict" user setting.
pub struct RejectUnknownPolicy;

impl HostKeyPolicy for RejectUnknownPolicy {
    fn accept_unknown_host(&self, _host: &str, _port: u16, _fingerprint: &str) -> bool {
        false
    }
}

/// The default `~/.ssh/known_hosts` path. Not used directly by
/// `verify_host_key` (which takes an explicit path so it stays testable
/// without mutating the process-global `HOME` env var) — callers that want
/// the real default call this.
pub fn default_known_hosts_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".ssh").join("known_hosts")
}

/// The known_hosts identifier for `host:port`, matching OpenSSH's own
/// `[host]:port` bracketed convention for non-default ports.
fn host_key_id(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

pub fn verify_host_key(
    session: &Session,
    host: &str,
    port: u16,
    policy: &dyn HostKeyPolicy,
    path: &std::path::Path,
) -> Result<(), String> {
    let mut known_hosts = session
        .known_hosts()
        .map_err(|e| format!("could not initialize known_hosts: {e}"))?;

    if path.exists() {
        known_hosts
            .read_file(path, KnownHostFileKind::OpenSSH)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    }

    let (key, key_type) = session
        .host_key()
        .ok_or_else(|| "server did not present a host key".to_string())?;

    let id = host_key_id(host, port);
    match known_hosts.check(&id, key) {
        CheckResult::Match => Ok(()),
        CheckResult::Mismatch => Err(format!(
            "host key for {id} does not match the one in {} — possible man-in-the-middle attack, refusing to connect",
            path.display()
        )),
        CheckResult::Failure => Err("host key check failed".to_string()),
        CheckResult::NotFound => {
            let fingerprint = session
                .host_key_hash(ssh2::HashType::Sha256)
                .map(|h| format!("SHA256:{}", hex_encode(h)))
                .unwrap_or_else(|| "<unavailable>".to_string());

            if !policy.accept_unknown_host(host, port, &fingerprint) {
                return Err(format!("host key for {id} rejected by policy"));
            }

            known_hosts
                .add(&id, key, "", key_type.into())
                .map_err(|e| format!("could not record host key: {e}"))?;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            known_hosts
                .write_file(path, KnownHostFileKind::OpenSSH)
                .map_err(|e| format!("could not save {}: {e}", path.display()))?;
            Ok(())
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
