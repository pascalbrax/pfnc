//! Spins up a throwaway, unprivileged local `sshd` for SFTP integration
//! tests — the primary documented Phase 1 approach is a Docker-based
//! OpenSSH container, but this environment has no Docker available, so we
//! use the plan's documented fallback: a real local `sshd` on a scratch
//! high port, authenticating the current OS user via a freshly generated
//! throwaway keypair. This still exercises the real OpenSSH server
//! implementation (not a Rust-side test double), which is what actually
//! matters for the "ultra compatible with any host" claim.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use pfnc_vfs_sftp::{AuthMethod, ConnectionProfile};

pub struct TestSshd {
    pub port: u16,
    pub username: String,
    pub client_key_path: PathBuf,
    pub known_hosts_path: PathBuf,
    dir: tempfile::TempDir,
    child: Child,
}

impl TestSshd {
    /// Starts a fresh `sshd` instance with its own host key, scratch
    /// known_hosts file, and a client keypair pre-authorized for the
    /// current user. Panics (with a clear message) if `sshd`/`ssh-keygen`
    /// aren't on `PATH` — callers should only reach this from tests marked
    /// `#[ignore]` so a plain `cargo test` never depends on it.
    pub fn start() -> Self {
        let dir = tempfile::tempdir().expect("failed to create tempdir for test sshd");
        let host_key = dir.path().join("host_key");
        let client_key = dir.path().join("client_key");
        let client_pub = dir.path().join("client_key.pub");
        let authorized_keys = dir.path().join("authorized_keys");
        let sshd_config = dir.path().join("sshd_config");
        let pidfile = dir.path().join("sshd.pid");
        let known_hosts_path = dir.path().join("known_hosts");

        run_ok(&["ssh-keygen", "-t", "ed25519", "-f", path_str(&host_key), "-N", "", "-q"]);
        run_ok(&["ssh-keygen", "-t", "ed25519", "-f", path_str(&client_key), "-N", "", "-q"]);
        std::fs::copy(&client_pub, &authorized_keys).expect("failed to install authorized_keys");

        let port = free_port();
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "nobody".to_string());

        let config = format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {host_key}\n\
             AuthorizedKeysFile {authorized_keys}\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             PubkeyAuthentication yes\n\
             UsePAM no\n\
             StrictModes no\n\
             PidFile {pidfile}\n\
             Subsystem sftp internal-sftp\n\
             LogLevel ERROR\n",
            host_key = host_key.display(),
            authorized_keys = authorized_keys.display(),
            pidfile = pidfile.display(),
        );
        std::fs::write(&sshd_config, config).expect("failed to write sshd_config");

        let sshd_bin = ["/usr/sbin/sshd", "/usr/bin/sshd"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .expect("no sshd binary found at /usr/sbin/sshd or /usr/bin/sshd");

        let child = Command::new(sshd_bin)
            .args(["-f", path_str(&sshd_config), "-D", "-e"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn sshd");

        wait_for_port(port);

        Self {
            port,
            username,
            client_key_path: client_key,
            known_hosts_path,
            dir,
            child,
        }
    }

    pub fn profile(&self, id: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: id.to_string(),
            host: "127.0.0.1".to_string(),
            port: self.port,
            username: self.username.clone(),
            auth: AuthMethod::KeyFile {
                private_key: self.client_key_path.clone(),
                public_key: None,
                passphrase: None,
            },
        }
    }

    /// A path inside the test's own scratch directory, for tests that need
    /// somewhere writable on the "remote" filesystem.
    pub fn scratch_dir(&self) -> PathBuf {
        let path = self.dir.path().join("remote_root");
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for TestSshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn path_str(p: &std::path::Path) -> &str {
    p.to_str().expect("test paths must be UTF-8")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("failed to bind ephemeral port")
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_port(port: u16) {
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("test sshd did not start listening on 127.0.0.1:{port} in time");
}

fn run_ok(args: &[&str]) {
    let status = Command::new(args[0])
        .args(&args[1..])
        .status()
        .unwrap_or_else(|e| panic!("failed to run {args:?}: {e}"));
    assert!(status.success(), "command failed: {args:?}");
}
