//! The app's own choices, kept in `<config dir>/app.json` (see [`crate::paths`]): theme, density,
//! desktop notifications, and the senders whose remote images load without asking.
//!
//! No secrets live here (CONTRIBUTING.md rule 6), only preferences. Unknown or broken files fall back to the
//! defaults field by field, so an older or newer app never refuses to start over its settings.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FILE: &str = "app.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    #[default]
    Comfortable,
    /// Two-line list rows.
    Compact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    /// Desktop notifications for new mail.
    pub enabled: bool,
    /// Also for anon accounts (off unless the user turns it on).
    pub anon: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self { enabled: true, anon: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub density: Density,
    pub notifications: Notifications,
    /// Sender addresses (lower case) whose remote images load without asking. Never consulted for anon
    /// accounts: there every load is confirmed again.
    pub remote_allowed_senders: Vec<String>,
}

impl Settings {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(FILE)
    }

    /// Read from `dir`, or the defaults when the file is missing or unreadable.
    pub fn load(dir: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(Self::path_in(dir)) else { return Self::default() };
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else { return Self::default() };
        // Field by field, so one bad value does not reset the rest.
        let mut s = Self::default();
        let field = |name: &str| value.get(name).cloned();
        if let Some(t) = field("theme").and_then(|v| serde_json::from_value(v).ok()) {
            s.theme = t;
        }
        if let Some(d) = field("density").and_then(|v| serde_json::from_value(v).ok()) {
            s.density = d;
        }
        if let Some(n) = field("notifications").and_then(|v| serde_json::from_value(v).ok()) {
            s.notifications = n;
        }
        if let Some(list) = field("remote_allowed_senders").and_then(|v| serde_json::from_value::<Vec<String>>(v).ok()) {
            for e in list {
                s.allow_sender(&e);
            }
        }
        s
    }

    /// Write atomically (temp file + rename) into `dir`, creating it (0700) if needed.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        crate::paths::ensure_private_dir(dir)?;
        let path = Self::path_in(dir);
        let tmp = dir.join(format!(".{FILE}.{}", std::process::id()));
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, text + "\n")?;
        std::fs::rename(&tmp, &path)
    }

    pub fn sender_allowed(&self, email: &str) -> bool {
        let e = email.trim().to_ascii_lowercase();
        self.remote_allowed_senders.contains(&e)
    }

    pub fn allow_sender(&mut self, email: &str) {
        let e = email.trim().to_ascii_lowercase();
        if e.contains('@') && !self.sender_allowed(&e) {
            self.remote_allowed_senders.push(e);
            self.remote_allowed_senders.sort();
        }
    }

    pub fn forget_sender(&mut self, email: &str) {
        let e = email.trim().to_ascii_lowercase();
        self.remote_allowed_senders.retain(|s| *s != e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        assert_eq!(Settings::load(&cfg), Settings::default(), "missing file = defaults");
        let mut s = Settings { theme: Theme::Light, density: Density::Compact, ..Default::default() };
        s.notifications.anon = true;
        s.allow_sender("News@WeeklyByte.example ");
        s.save(&cfg).unwrap();
        let back = Settings::load(&cfg);
        assert_eq!(back, s);
        assert!(back.sender_allowed("news@weeklybyte.example"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn broken_or_partial_files_keep_what_they_can() {
        assert_eq!(Settings::parse("not json"), Settings::default());
        let s = Settings::parse(r#"{"theme":"light","density":"sideways","remote_allowed_senders":["a@b.example","nope"]}"#);
        assert_eq!(s.theme, Theme::Light);
        assert_eq!(s.density, Density::Comfortable);
        assert_eq!(s.remote_allowed_senders, vec!["a@b.example".to_string()]);
        assert!(s.notifications.enabled && !s.notifications.anon, "notifications default on, anon off");
    }

    #[test]
    fn senders_are_added_once_and_forgotten() {
        let mut s = Settings::default();
        s.allow_sender("a@b.example");
        s.allow_sender("A@B.example");
        assert_eq!(s.remote_allowed_senders.len(), 1);
        s.forget_sender("a@b.example");
        assert!(!s.sender_allowed("a@b.example"));
    }
}
