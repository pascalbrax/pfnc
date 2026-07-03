//! Authentication flows: try `ssh-agent`, then key files, then password /
//! keyboard-interactive, mirroring how a typical SSH client behaves.

use std::path::PathBuf;

use ssh2::{KeyboardInteractivePrompt, Prompt, Session};

#[derive(Clone, Debug)]
pub enum AuthMethod {
    Agent,
    KeyFile {
        private_key: PathBuf,
        public_key: Option<PathBuf>,
        passphrase: Option<String>,
    },
    Password(String),
    /// Many servers that look like they support plain password auth
    /// actually route it through PAM as keyboard-interactive; this answers
    /// every prompt with the same password.
    KeyboardInteractive(String),
    /// Try `ssh-agent`, then the default key files in `~/.ssh`, then fall
    /// back to `password` (if given) for password/keyboard-interactive.
    Auto { password: Option<String> },
}

struct SinglePasswordPrompter<'a>(&'a str);

impl KeyboardInteractivePrompt for SinglePasswordPrompter<'_> {
    fn prompt<'b>(&mut self, _username: &str, _instructions: &str, prompts: &[Prompt<'b>]) -> Vec<String> {
        prompts.iter().map(|_| self.0.to_string()).collect()
    }
}

fn default_key_files() -> Vec<PathBuf> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    ["id_ed25519", "id_rsa", "id_ecdsa"]
        .iter()
        .map(|name| PathBuf::from(&home).join(".ssh").join(name))
        .collect()
}

pub fn authenticate(session: &Session, username: &str, method: &AuthMethod) -> Result<(), String> {
    match method {
        AuthMethod::Agent => session
            .userauth_agent(username)
            .map_err(|e| format!("agent auth failed: {e}")),
        AuthMethod::KeyFile {
            private_key,
            public_key,
            passphrase,
        } => session
            .userauth_pubkey_file(username, public_key.as_deref(), private_key, passphrase.as_deref())
            .map_err(|e| format!("key auth failed: {e}")),
        AuthMethod::Password(password) => session
            .userauth_password(username, password)
            .map_err(|e| format!("password auth failed: {e}")),
        AuthMethod::KeyboardInteractive(password) => {
            let mut prompter = SinglePasswordPrompter(password);
            session
                .userauth_keyboard_interactive(username, &mut prompter)
                .map_err(|e| format!("keyboard-interactive auth failed: {e}"))
        }
        AuthMethod::Auto { password } => auto_authenticate(session, username, password.as_deref()),
    }
}

fn auto_authenticate(session: &Session, username: &str, password: Option<&str>) -> Result<(), String> {
    if session.userauth_agent(username).is_ok() {
        return Ok(());
    }
    for key in default_key_files() {
        if key.exists() && session.userauth_pubkey_file(username, None, &key, None).is_ok() {
            return Ok(());
        }
    }
    if let Some(password) = password {
        if session.userauth_password(username, password).is_ok() {
            return Ok(());
        }
        let mut prompter = SinglePasswordPrompter(password);
        if session.userauth_keyboard_interactive(username, &mut prompter).is_ok() {
            return Ok(());
        }
    }
    Err("all authentication methods failed (agent, default keys, password)".to_string())
}
