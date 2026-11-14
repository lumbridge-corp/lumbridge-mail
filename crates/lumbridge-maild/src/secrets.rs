//! Secrets for providers. Never in the config, the store or a log (CONTRIBUTING.md rule 6).
//!
//! IMAP passwords (or OAuth tokens), in this order: the environment variable named by `password_env`,
//! the stdout of `password_command`, then the OS keyring (service `lumbridge-mail`, user
//! `imap:<account id>`; macOS Keychain or the Secret Service on Linux). `lbmail account set-password`
//! writes the keyring entry. Resend API keys the same way (`api_key_env`, `api_key_command`, keyring
//! user `resend:<account id>`, written by `lbmail account set-key`). gog needs nothing here: it holds
//! its own OAuth tokens.

use lumbridge_mail_core::{Error, Result};

pub const KEYRING_SERVICE: &str = "lumbridge-mail";

fn keyring_user(account: &str) -> String {
    format!("imap:{account}")
}

fn resend_user(account: &str) -> String {
    format!("resend:{account}")
}

/// Where a password can come from, as configured for one account.
#[derive(Debug, Clone, Default)]
pub struct SecretSource {
    pub env: Option<String>,
    pub command: Option<Vec<String>>,
}

/// The secret for `account`. `Error::Auth` (with a hint, never the secret) when there is none.
/// Blocking (the keyring may talk to D-Bus): call from `spawn_blocking`.
pub fn imap_secret(account: &str, source: &SecretSource) -> Result<String> {
    secret_from(account, source, "password_command", &keyring_user(account), &format!("no password saved; run `lbmail account set-password {account}`"))
}

/// A Resend account's API key: `api_key_env`, `api_key_command`, then the keyring (`resend:<id>`).
/// Blocking. `Error::Auth` with a hint (never the key) when there is none.
pub fn resend_secret(account: &str, source: &SecretSource) -> Result<String> {
    secret_from(account, source, "api_key_command", &resend_user(account), &format!("no API key saved; run `lbmail account set-key {account}`"))
}

fn secret_from(account: &str, source: &SecretSource, command_name: &str, user: &str, missing: &str) -> Result<String> {
    if let Some(var) = &source.env {
        return match std::env::var(var) {
            Ok(v) if !v.is_empty() => Ok(v),
            _ => Err(Error::Auth(format!("{account}: environment variable {var} is not set"))),
        };
    }
    if let Some(cmd) = source.command.as_ref().filter(|c| !c.is_empty()) {
        let out = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| Error::Auth(format!("{account}: {command_name} could not run: {}", e.kind())))?;
        if !out.status.success() {
            return Err(Error::Auth(format!("{account}: {command_name} failed ({})", out.status)));
        }
        let s = String::from_utf8(out.stdout).map_err(|_| Error::Auth(format!("{account}: {command_name} printed non-UTF-8")))?;
        let s = s.trim_end_matches(['\r', '\n']).to_owned();
        if s.is_empty() {
            return Err(Error::Auth(format!("{account}: {command_name} printed nothing")));
        }
        return Ok(s);
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, user)
        .map_err(|e| Error::Auth(format!("{account}: keyring unavailable: {}", keyring_kind(&e))))?;
    match entry.get_password() {
        Ok(p) => Ok(p),
        Err(keyring::Error::NoEntry) => Err(Error::Auth(format!("{account}: {missing}"))),
        Err(e) => Err(Error::Auth(format!("{account}: keyring unavailable: {}", keyring_kind(&e)))),
    }
}

/// Where `accounts.add` keeps a new IMAP account's password. [`Keyring`] is the real one; tests hand the
/// daemon a stand-in so no test ever writes to a real keyring.
pub trait SecretStore: Send + Sync {
    /// Blocking.
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    /// Blocking. A missing entry is fine.
    fn delete(&self, account: &str) -> Result<()>;
    /// Blocking. A Resend account's API key (the keyring keeps it apart from IMAP passwords; a test
    /// stand-in may keep both alike).
    fn set_resend(&self, account: &str, key: &str) -> Result<()> {
        self.set(account, key)
    }
    /// Blocking. A missing entry is fine.
    fn delete_resend(&self, account: &str) -> Result<()> {
        self.delete(account)
    }
}

/// The OS keyring (macOS Keychain, the Secret Service on Linux).
pub struct Keyring;

impl SecretStore for Keyring {
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        set_imap_secret(account, secret)
    }
    fn delete(&self, account: &str) -> Result<()> {
        delete_imap_secret(account)
    }
    fn set_resend(&self, account: &str, key: &str) -> Result<()> {
        set_entry(&resend_user(account), key)
    }
    fn delete_resend(&self, account: &str) -> Result<()> {
        delete_entry(&resend_user(account))
    }
}

/// Saves the secret for `account` in the OS keyring. Blocking.
pub fn set_imap_secret(account: &str, secret: &str) -> Result<()> {
    set_entry(&keyring_user(account), secret)
}

/// Saves a Resend API key for `account` in the OS keyring. Blocking.
pub fn set_resend_secret(account: &str, key: &str) -> Result<()> {
    set_entry(&resend_user(account), key)
}

/// Forgets a Resend account's keyring entry. Missing entries are fine.
pub fn delete_resend_secret(account: &str) -> Result<()> {
    delete_entry(&resend_user(account))
}

fn set_entry(user: &str, secret: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, user)
        .map_err(|e| Error::Other(format!("keyring unavailable: {}", keyring_kind(&e))))?;
    entry.set_password(secret).map_err(|e| Error::Other(format!("keyring: {}", keyring_kind(&e))))
}

/// Forgets the keyring entry (account removed). Missing entries are fine.
pub fn delete_imap_secret(account: &str) -> Result<()> {
    delete_entry(&keyring_user(account))
}

fn delete_entry(user: &str) -> Result<()> {
    let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, user) else { return Ok(()) };
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Other(format!("keyring: {}", keyring_kind(&e)))),
    }
}

/// A short description of a keyring error that cannot carry a secret.
fn keyring_kind(e: &keyring::Error) -> &'static str {
    match e {
        keyring::Error::PlatformFailure(_) => "platform failure",
        keyring::Error::NoStorageAccess(_) => "no storage access (no Secret Service on this machine? use password_env)",
        keyring::Error::NoEntry => "no entry",
        keyring::Error::BadEncoding(_) => "bad encoding",
        keyring::Error::TooLong(..) => "too long",
        keyring::Error::Invalid(..) => "invalid",
        keyring::Error::Ambiguous(_) => "ambiguous",
        _ => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_and_command_sources() {
        // SAFETY: the variable name is unique to this test and nothing else reads it.
        unsafe { std::env::set_var("LBMAIL_TEST_SECRET_X1", "hunter2-not-real") };
        let s = SecretSource { env: Some("LBMAIL_TEST_SECRET_X1".into()), command: None };
        assert_eq!(imap_secret("a", &s).unwrap(), "hunter2-not-real");
        let missing = SecretSource { env: Some("LBMAIL_TEST_SECRET_MISSING".into()), command: None };
        assert!(matches!(imap_secret("a", &missing), Err(Error::Auth(_))));
        let cmd = SecretSource { env: None, command: Some(vec!["printf".into(), "made-up\\n".into()]) };
        assert_eq!(imap_secret("a", &cmd).unwrap(), "made-up");
        let bad = SecretSource { env: None, command: Some(vec!["false".into()]) };
        let e = imap_secret("a", &bad).unwrap_err();
        assert!(matches!(e, Error::Auth(_)));
        let e = resend_secret("shop", &bad).unwrap_err();
        assert!(e.to_string().contains("api_key_command"), "{e}");
        assert_eq!(resend_secret("shop", &cmd).unwrap(), "made-up");
    }
}
