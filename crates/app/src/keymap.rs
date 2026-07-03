//! Maps configurable "actions" (the F-key-style commands) to
//! `crossterm::KeyCode`s, with built-in defaults overridable via
//! `config.toml`'s `[keybindings]` table (e.g. `copy = "F5"`).
//!
//! A small fixed set of aliases (`q`/`Esc` for quit, `j`/`k` for cursor
//! movement, arrow keys, `Enter`, `Backspace`, `Space`, `Tab`) are handled
//! directly in `actions::handle_browsing_key` rather than through this
//! map — remapping "how do I move the cursor" away from the arrow keys
//! isn't a real user need the way remapping F5/F6/etc. is, and keeping
//! them fixed avoids a config mistake locking someone out of navigation.

use std::collections::HashMap;

use crossterm::event::KeyCode;

pub const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("quit", "F10"),
    ("edit", "F4"),
    ("copy", "F5"),
    ("move", "F6"),
    ("mkdir", "F7"),
    ("delete", "F8"),
    ("connect", "F9"),
];

/// Parses the small key-name syntax used in `config.toml`: `F1`..`F12`,
/// or any other single character taken literally (`q`, `a`, ...). This
/// deliberately doesn't support modifier combinations (`Ctrl+x` etc.) —
/// nothing in the app uses them today, so there's nothing meaningful to
/// remap onto them yet.
pub fn parse_key_name(s: &str) -> Option<KeyCode> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix(['F', 'f']) {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return rest.parse::<u8>().ok().map(KeyCode::F);
        }
    }
    match s {
        "Esc" | "Escape" => Some(KeyCode::Esc),
        "Enter" | "Return" => Some(KeyCode::Enter),
        "Tab" => Some(KeyCode::Tab),
        "Backspace" => Some(KeyCode::Backspace),
        "Space" => Some(KeyCode::Char(' ')),
        "Left" => Some(KeyCode::Left),
        "Right" => Some(KeyCode::Right),
        "Up" => Some(KeyCode::Up),
        "Down" => Some(KeyCode::Down),
        "Home" => Some(KeyCode::Home),
        "End" => Some(KeyCode::End),
        _ if s.chars().count() == 1 => s.chars().next().map(KeyCode::Char),
        _ => None,
    }
}

pub struct Keymap {
    /// action name -> resolved key.
    bindings: HashMap<&'static str, KeyCode>,
}

impl Keymap {
    /// Builds the effective keymap: `DEFAULT_BINDINGS`, with any entry in
    /// `overrides` that names a known action and parses successfully
    /// replacing the default. Unknown action names or unparseable key
    /// strings are logged and ignored rather than failing startup.
    pub fn from_overrides(overrides: &HashMap<String, String>) -> Self {
        let mut bindings = HashMap::new();
        for (action, default_key) in DEFAULT_BINDINGS {
            let resolved = overrides
                .get(*action)
                .and_then(|key_str| {
                    let parsed = parse_key_name(key_str);
                    if parsed.is_none() {
                        tracing::warn!(action, key = key_str, "unrecognized key name in config, ignoring");
                    }
                    parsed
                })
                .unwrap_or_else(|| parse_key_name(default_key).expect("built-in default key names are always valid"));
            bindings.insert(*action, resolved);
        }
        for action in overrides.keys() {
            if !DEFAULT_BINDINGS.iter().any(|(name, _)| name == action) {
                tracing::warn!(action, "unknown action name in config [keybindings], ignoring");
            }
        }
        Self { bindings }
    }

    pub fn action_for(&self, key: KeyCode) -> Option<&'static str> {
        self.bindings.iter().find(|(_, &v)| v == key).map(|(&name, _)| name)
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::from_overrides(&HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve_to_expected_keys() {
        let keymap = Keymap::default();
        assert_eq!(keymap.action_for(KeyCode::F(5)), Some("copy"));
        assert_eq!(keymap.action_for(KeyCode::F(10)), Some("quit"));
        assert_eq!(keymap.action_for(KeyCode::F(1)), None);
    }

    #[test]
    fn override_replaces_default_binding() {
        let mut overrides = HashMap::new();
        overrides.insert("copy".to_string(), "F2".to_string());
        let keymap = Keymap::from_overrides(&overrides);
        assert_eq!(keymap.action_for(KeyCode::F(2)), Some("copy"));
        assert_eq!(keymap.action_for(KeyCode::F(5)), None);
    }

    #[test]
    fn invalid_override_falls_back_to_default() {
        let mut overrides = HashMap::new();
        overrides.insert("copy".to_string(), "NotAKey!!".to_string());
        let keymap = Keymap::from_overrides(&overrides);
        assert_eq!(keymap.action_for(KeyCode::F(5)), Some("copy"));
    }

    #[test]
    fn parses_function_and_named_keys() {
        assert_eq!(parse_key_name("F5"), Some(KeyCode::F(5)));
        assert_eq!(parse_key_name("f12"), Some(KeyCode::F(12)));
        assert_eq!(parse_key_name("Esc"), Some(KeyCode::Esc));
        assert_eq!(parse_key_name("q"), Some(KeyCode::Char('q')));
        assert_eq!(parse_key_name(""), None);
    }
}
