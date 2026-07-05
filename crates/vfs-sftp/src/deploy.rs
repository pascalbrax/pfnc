//! SSH-based deployment of the Phase 3 `pfnc-agent` binary: upload it over
//! the already-open SFTP subsystem, exec it over a fresh SSH channel (the
//! same exec-channel mechanism `quick_hash` uses for `xxhsum`), and parse
//! what it reports about itself on startup (PID, bound port, cert).
//!
//! Deliberately *not* wired into `pfnc-core`'s `Transport`/`negotiate_transport`
//! yet — this proves the deployment mechanism alone. No new production
//! dependency on `quinn`/`rustls`/`tokio` is needed here; this crate only
//! uploads bytes and runs a shell command.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use ssh2::{Channel, OpenFlags, OpenType};
use thiserror::Error;

use crate::{shell_quote, SftpFs};

/// What a freshly deployed agent reported about itself on startup.
#[derive(Clone, Debug)]
pub struct DeployedAgent {
    pub pid: u32,
    pub port: u16,
    pub cert_der: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("failed to read local agent binary at {0}: {1}")]
    ReadLocalBinary(PathBuf, std::io::Error),
    #[error("failed to upload agent binary: {0}")]
    Upload(String),
    #[error("failed to exec remote agent: {0}")]
    Exec(ssh2::Error),
    #[error("failed to read agent startup output: {0}")]
    ReadOutput(std::io::Error),
    #[error("agent startup output was missing or malformed: {0}")]
    MalformedOutput(String),
    #[error("failed to kill remote process {0}: {1}")]
    Kill(u32, String),
}

/// Uploads `local_binary_path` to `{remote_dir}/pfnc-agent` (mode `0o755`)
/// and execs it with `--port 0`, returning what it reported about itself
/// plus the still-open exec `Channel` — the agent keeps running as long as
/// that channel stays open, so callers must hold onto it (and eventually
/// call `kill_remote_process` to clean up; letting the channel close on
/// its own is not a reliable way to stop the remote process).
pub fn deploy_and_start(
    vfs: &SftpFs,
    local_binary_path: &Path,
    remote_dir: &str,
) -> Result<(DeployedAgent, Channel), DeployError> {
    let bytes = std::fs::read(local_binary_path)
        .map_err(|e| DeployError::ReadLocalBinary(local_binary_path.to_path_buf(), e))?;

    let remote_path = format!("{remote_dir}/pfnc-agent");
    {
        use std::io::Write;
        let mut remote_file = vfs
            .sftp
            .open_mode(
                std::path::Path::new(&remote_path),
                OpenFlags::WRITE | OpenFlags::TRUNCATE,
                0o755,
                OpenType::File,
            )
            .map_err(|e| DeployError::Upload(e.to_string()))?;
        remote_file.write_all(&bytes).map_err(|e| DeployError::Upload(e.to_string()))?;
    }

    let mut channel = vfs.session.channel_session().map_err(DeployError::Exec)?;
    let command = format!("{} --port 0", shell_quote(&remote_path));
    channel.exec(&command).map_err(DeployError::Exec)?;

    let mut reader = std::io::BufReader::new(channel);
    let deployed = parse_startup_lines(&mut reader)?;
    // Reclaim the `Channel` (discarding any buffered leftover bytes — there
    // shouldn't be any, since the agent writes nothing else to stdout after
    // its three startup lines) so the caller can keep it open and running.
    let channel = reader.into_inner();

    Ok((deployed, channel))
}

/// Reads `pfnc-agent`'s three `PFNC-AGENT-*` startup lines from `reader`,
/// identifying each by its prefix rather than assuming they're exactly the
/// first three lines read. Real-world `sshd`/shell configs sometimes write
/// something of their own to stdout before the command's own output even
/// for a plain exec channel (a MOTD, a security banner, a shell wrapper) —
/// any such line is skipped (logged at `debug`, not treated as an error)
/// rather than failing the whole deployment. Bounded by
/// `MAX_STARTUP_LINES` so a remote that never actually produces the
/// expected output (wrong binary, immediate crash) still fails promptly
/// instead of hanging.
fn parse_startup_lines(reader: &mut impl BufRead) -> Result<DeployedAgent, DeployError> {
    const MAX_STARTUP_LINES: usize = 50;
    let mut pid = None;
    let mut port = None;
    let mut cert_der = None;
    for _ in 0..MAX_STARTUP_LINES {
        if pid.is_some() && port.is_some() && cert_der.is_some() {
            break;
        }
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).map_err(DeployError::ReadOutput)?;
        if bytes_read == 0 {
            break; // EOF: the process exited before producing everything expected.
        }
        let line = line.trim();
        if let Some(value) = line.strip_prefix("PFNC-AGENT-PID ") {
            pid = value
                .parse()
                .map_err(|_| DeployError::MalformedOutput(format!("bad pid in {line:?}")))
                .map(Some)?;
        } else if let Some(value) = line.strip_prefix("PFNC-AGENT-PORT ") {
            port = value
                .parse()
                .map_err(|_| DeployError::MalformedOutput(format!("bad port in {line:?}")))
                .map(Some)?;
        } else if let Some(value) = line.strip_prefix("PFNC-AGENT-CERT-HEX ") {
            cert_der = Some(from_hex(value).map_err(|e| DeployError::MalformedOutput(format!("bad cert hex: {e}")))?);
        } else if !line.is_empty() {
            tracing::debug!(line, "ignoring unexpected output before pfnc-agent's own startup lines");
        }
    }

    let (Some(pid), Some(port), Some(cert_der)) = (pid, port, cert_der) else {
        return Err(DeployError::MalformedOutput(
            "missing one or more expected startup lines (agent likely failed to start)".to_string(),
        ));
    };

    Ok(DeployedAgent { pid, port, cert_der })
}

/// Kills the remote process `pid` via a separate, fresh exec channel —
/// deliberately not relying on the original exec channel's close to tear
/// down the process, since that isn't reliably specified behavior across
/// `sshd` configurations.
pub fn kill_remote_process(vfs: &SftpFs, pid: u32) -> Result<(), DeployError> {
    let mut channel = vfs.session.channel_session().map_err(|e| DeployError::Kill(pid, e.to_string()))?;
    channel.exec(&format!("kill {pid}")).map_err(|e| DeployError::Kill(pid, e.to_string()))?;
    // `wait_close` requires the channel to have reached EOF first, which
    // only happens once its output has actually been read (matching
    // `exec_quick_hash`'s use of the same exec-channel mechanism above).
    let mut discard = String::new();
    std::io::Read::read_to_string(&mut channel, &mut discard).map_err(|e| DeployError::Kill(pid, e.to_string()))?;
    channel.wait_close().map_err(|e| DeployError::Kill(pid, e.to_string()))?;
    Ok(())
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("odd-length hex string ({} chars)", s.len()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_hex_decodes_known_bytes() {
        assert_eq!(from_hex("00abff").unwrap(), vec![0x00, 0xab, 0xff]);
    }

    #[test]
    fn from_hex_rejects_odd_length() {
        assert!(from_hex("abc").is_err());
    }

    #[test]
    fn from_hex_rejects_non_hex_chars() {
        assert!(from_hex("zz").is_err());
    }

    fn deployed(pid: u32, port: u16, hex: &str) -> String {
        format!("PFNC-AGENT-PID {pid}\nPFNC-AGENT-PORT {port}\nPFNC-AGENT-CERT-HEX {hex}\n")
    }

    #[test]
    fn parse_startup_lines_reads_exactly_the_three_expected_lines() {
        let input = deployed(123, 4433, "00abff");
        let mut reader = std::io::BufReader::new(input.as_bytes());
        let agent = parse_startup_lines(&mut reader).unwrap();
        assert_eq!(agent.pid, 123);
        assert_eq!(agent.port, 4433);
        assert_eq!(agent.cert_der, vec![0x00, 0xab, 0xff]);
    }

    #[test]
    fn parse_startup_lines_tolerates_a_shell_motd_banner_first() {
        // Real-world sshd/shell configs can write a banner before the
        // command's own output even on a plain exec channel — this must
        // not be treated as a fatal "unexpected startup line" error.
        let input = format!(
            "Welcome to Ubuntu 22.04.3 LTS\n\n\
             {}",
            deployed(456, 9999, "1234")
        );
        let mut reader = std::io::BufReader::new(input.as_bytes());
        let agent = parse_startup_lines(&mut reader).unwrap();
        assert_eq!(agent.pid, 456);
        assert_eq!(agent.port, 9999);
        assert_eq!(agent.cert_der, vec![0x12, 0x34]);
    }

    #[test]
    fn parse_startup_lines_tolerates_the_three_lines_out_of_order() {
        let input = "PFNC-AGENT-PORT 9999\nPFNC-AGENT-CERT-HEX ab\nPFNC-AGENT-PID 456\n";
        let mut reader = std::io::BufReader::new(input.as_bytes());
        let agent = parse_startup_lines(&mut reader).unwrap();
        assert_eq!(agent.pid, 456);
        assert_eq!(agent.port, 9999);
        assert_eq!(agent.cert_der, vec![0xab]);
    }

    #[test]
    fn parse_startup_lines_errors_cleanly_on_eof_before_all_three_arrive() {
        let mut reader = std::io::BufReader::new("PFNC-AGENT-PID 123\n".as_bytes());
        assert!(parse_startup_lines(&mut reader).is_err());
    }

    #[test]
    fn parse_startup_lines_errors_rather_than_hanging_on_endless_junk() {
        // 200 blank-ish junk lines, well past MAX_STARTUP_LINES, and never
        // any of the real startup lines — must still terminate.
        let input = "some banner line\n".repeat(200);
        let mut reader = std::io::BufReader::new(input.as_bytes());
        assert!(parse_startup_lines(&mut reader).is_err());
    }
}
