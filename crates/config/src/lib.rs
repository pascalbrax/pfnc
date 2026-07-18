//! TOML-backed application config: general settings, keybinding overrides,
//! saved SSH connection profiles (host/port/username only — never a
//! password), and the last session's local panel locations.
//!
//! Deliberately decoupled from `pfnc-core`/`pfnc-vfs-*`: this crate only
//! knows about plain strings and primitives, so it stays at the bottom of
//! the dependency graph. The `app` crate (which knows about `Location`,
//! `crossterm::KeyCode`, etc.) is responsible for converting to/from these
//! plain types.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the config directory")]
    NoConfigDir,
    #[error("failed to read {0}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("failed to parse {0}: {1}")]
    Parse(PathBuf, toml::de::Error),
    #[error("failed to write {0}: {1}")]
    Write(PathBuf, std::io::Error),
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct GeneralConfig {
    pub confirm_delete: bool,
    pub show_hidden: bool,
    /// Whether `sync` also removes destination files/directories that
    /// don't exist on the source. Defaults to `false` — a sync that only
    /// ever adds/updates is a much safer default than one that can also
    /// delete, and the scan-then-confirm flow always shows the user
    /// exactly what a `true` setting would remove before it happens.
    pub sync_delete_extraneous: bool,
    /// Whether a Local<->Remote(SFTP) copy/sync may try the QUIC-agent
    /// transport (auto-deployed to the remote host over SSH, only attempted
    /// when the remote host is detected as Linux) instead of plain SFTP.
    /// Defaults to `false`: real-world LAN measurements (see `roadmap.md`'s
    /// "Known limitations") found the QUIC path consistently *slower* than
    /// plain SFTP — e.g. 59 MiB/s vs. plain SFTP's ~100 MiB/s for the same
    /// 1 GiB file on the same LAN host, both in release builds — so this is
    /// opt-in until that regression is root-caused, not a safe default
    /// despite the always-on SFTP fallback on failure. Turning it on also
    /// means pfnc uploads and executes a small binary on remote hosts it
    /// connects to.
    pub enable_quic_fast_path: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            confirm_delete: true,
            show_hidden: false,
            sync_delete_extraneous: false,
            enable_quic_fast_path: false,
        }
    }
}

/// A saved connection: everything needed to refill the Connect dialog's
/// host/port/username fields. No password/passphrase field exists here on
/// purpose — those are never written to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionProfileConfig {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelLocationConfig {
    Local,
    Remote { profile_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PanelSessionConfig {
    pub location: PanelLocationConfig,
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SessionConfig {
    pub left: Option<PanelSessionConfig>,
    pub right: Option<PanelSessionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    /// Action name (e.g. `"copy"`) -> key name (e.g. `"F5"`). Only actions
    /// present here override the built-in default; everything else keeps
    /// its default binding. See `pfnc`'s `keymap` module for the action
    /// list and key-name syntax.
    pub keybindings: HashMap<String, String>,
    pub profiles: Vec<ConnectionProfileConfig>,
    pub session: SessionConfig,
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    let dirs = directories::ProjectDirs::from("dev", "pfnc", "pfnc").ok_or(ConfigError::NoConfigDir)?;
    Ok(dirs.config_dir().join("config.toml"))
}

impl Config {
    /// Loads config from the default XDG location, defaulting (not
    /// erroring) if the file doesn't exist yet — the common first-run
    /// case. Malformed existing config *is* an error rather than silently
    /// discarding the user's settings.
    pub fn load() -> Result<Self, ConfigError> {
        let path = config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.to_path_buf(), e))?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&config_path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::Write(path.to_path_buf(), e))?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).map_err(|e| ConfigError::Write(path.to_path_buf(), e))
    }

    /// Inserts or updates a saved profile by id.
    pub fn upsert_profile(&mut self, profile: ConnectionProfileConfig) {
        if let Some(existing) = self.profiles.iter_mut().find(|p| p.id == profile.id) {
            *existing = profile;
        } else {
            self.profiles.push(profile);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_loads_as_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = Config::default();
        config.general.show_hidden = true;
        config.keybindings.insert("quit".to_string(), "F10".to_string());
        config.upsert_profile(ConnectionProfileConfig {
            id: "user@host:22".to_string(),
            host: "host".to_string(),
            port: 22,
            username: "user".to_string(),
        });
        config.session.left = Some(PanelSessionConfig {
            location: PanelLocationConfig::Local,
            cwd: "/home/user".to_string(),
        });

        config.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn partial_config_fills_in_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[general]\nshow_hidden = true\n").unwrap();

        let config = Config::load_from(&path).unwrap();
        assert!(config.general.show_hidden);
        assert!(config.general.confirm_delete); // untouched field keeps its default
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn upsert_profile_replaces_existing_id() {
        let mut config = Config::default();
        config.upsert_profile(ConnectionProfileConfig {
            id: "a".into(),
            host: "old".into(),
            port: 22,
            username: "u".into(),
        });
        config.upsert_profile(ConnectionProfileConfig {
            id: "a".into(),
            host: "new".into(),
            port: 2222,
            username: "u".into(),
        });
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles[0].host, "new");
    }

    #[test]
    fn malformed_existing_file_is_an_error_not_silently_defaulted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not valid toml {{{").unwrap();
        assert!(Config::load_from(&path).is_err());
    }
}
