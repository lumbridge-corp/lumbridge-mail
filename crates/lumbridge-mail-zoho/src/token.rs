//! The OAuth token, kept in a file in the `mcp-remote` format: a JSON object with `access_token`,
//! `refresh_token`, `expires_in` (seconds, counted from when the file was written: its mtime),
//! `token_type`, `scope` and `api_domain`. Other keys are kept as they are.
//!
//! Another program (on another machine) may hold the same refresh token, so this never refreshes on a
//! timer: only when the access token has expired or a call came back 401, and only after reading the file
//! again (someone may have refreshed it already). Refreshes of one file are serialised in this process.
//! The new token is written to a temporary file next to the old one, with the old file's mode (0600 for
//! a new file), and renamed over it.
//!
//! Token values never appear in an error, a log line or `Debug` output.

use lumbridge_mail_core::{Error, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// An access token counts as expired this long before Zoho says it does.
const MARGIN: Duration = Duration::from_secs(60);

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub access: String,
    pub expires_at: Option<SystemTime>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Token {
    pub fn expired(&self) -> bool {
        self.access.is_empty()
            || self
                .expires_at
                .is_some_and(|t| SystemTime::now() + MARGIN >= t)
    }
}

fn num(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A short, safe word from an OAuth error (`invalid_grant`), never the description.
fn error_word(v: &Value) -> String {
    let w = text(v.get("error")).unwrap_or_default();
    w.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ' ')
        .take(40)
        .collect()
}

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> Error {
    Error::Auth(format!(
        "{what} the Zoho token file {}: {}",
        path.display(),
        e.kind()
    ))
}

/// The file's JSON and the token in it (expiry from the file's mtime + `expires_in`).
pub(crate) fn read_file(path: &Path) -> Result<(Map<String, Value>, Token)> {
    let bytes = std::fs::read(path).map_err(|e| io_err("cannot read", path, &e))?;
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let map: Map<String, Value> = serde_json::from_slice(&bytes).map_err(|_| {
        Error::Auth(format!(
            "the Zoho token file {} is not a JSON object",
            path.display()
        ))
    })?;
    let access = text(map.get("access_token")).unwrap_or_default();
    let expires_at = match (num(map.get("expires_in")), mtime) {
        (Some(s), Some(t)) => Some(t + Duration::from_secs(s)),
        _ => None,
    };
    Ok((map, Token { access, expires_at }))
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes `map` over `path` atomically, keeping the old file's permission bits (0600 if there was none).
pub(crate) fn write_file(path: &Path, map: &Map<String, Value>) -> Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "token".into());
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes =
        serde_json::to_vec_pretty(map).map_err(|e| Error::Other(format!("token file: {e}")))?;
    let result = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mode = std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o7777)
                .unwrap_or(0o600);
            opts.mode(0o600);
            mode
        };
        let mut f = opts.open(&tmp)?;
        f.write_all(&bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err("cannot write", path, &e));
    }
    Ok(())
}

/// Percent-encoding for `application/x-www-form-urlencoded`.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The token of one file, shared by every account on the connection.
pub(crate) struct TokenStore {
    path: PathBuf,
    token_url: String,
    client_id: String,
    resource: String,
    current: Mutex<Option<Token>>,
    refreshing: tokio::sync::Mutex<()>,
    refreshes: AtomicU64,
}

impl TokenStore {
    pub fn new(path: PathBuf, token_url: String, client_id: String, resource: String) -> Self {
        Self {
            path,
            token_url,
            client_id,
            resource,
            current: Mutex::new(None),
            refreshing: tokio::sync::Mutex::new(()),
            refreshes: AtomicU64::new(0),
        }
    }

    /// Refreshes posted by this process so far.
    pub fn refreshes(&self) -> u64 {
        self.refreshes.load(Ordering::SeqCst)
    }

    fn adopt(&self, t: &Token) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(t.clone());
    }

    /// A usable access token: the one in memory, else the file's, else a refreshed one.
    pub async fn access(&self, http: &reqwest::Client) -> Result<String> {
        if let Some(t) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            && !t.expired()
        {
            return Ok(t.access);
        }
        self.renew(http, None).await
    }

    /// After a 401 with `stale`: the file's token if someone else already replaced it, else a refresh.
    /// With `stale: None`: the file's token if it has not expired, else a refresh.
    pub async fn renew(&self, http: &reqwest::Client, stale: Option<&str>) -> Result<String> {
        let _one = self.refreshing.lock().await;
        let (mut map, tok) = read_file(&self.path)?;
        let fresh = match stale {
            Some(s) => tok.access != s && !tok.expired(),
            None => !tok.expired(),
        };
        if fresh {
            self.adopt(&tok);
            return Ok(tok.access);
        }
        let refresh = text(map.get("refresh_token")).ok_or_else(|| {
            Error::Auth("the Zoho token file has no refresh token; sign in to Zoho again".into())
        })?;
        let form = form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &self.client_id),
            ("resource", &self.resource),
        ]);
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        let resp = http
            .post(&self.token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(form)
            .send()
            .await
            .map_err(|e| {
                Error::Offline(format!(
                    "Zoho token refresh: {}",
                    if e.is_timeout() {
                        "timed out"
                    } else {
                        "could not connect"
                    }
                ))
            })?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|_| Error::Offline("Zoho token refresh: the answer was cut off".into()))?;
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let access = text(v.get("access_token"));
        let Some(access) = access.filter(|_| (200..300).contains(&status)) else {
            let word = error_word(&v);
            let lower = format!(
                "{word} {}",
                text(v.get("error_description")).unwrap_or_default()
            )
            .to_ascii_lowercase();
            return Err(
                if status == 429
                    || status >= 500
                    || lower.contains("too many")
                    || lower.contains("access denied")
                {
                    Error::Offline(format!(
                        "Zoho token refresh: busy ({status}{}{word})",
                        if word.is_empty() { "" } else { " " }
                    ))
                } else {
                    Error::Auth(format!(
                        "Zoho refused the token refresh ({status}{}{word}); sign in to Zoho again",
                        if word.is_empty() { "" } else { " " }
                    ))
                },
            );
        };
        map.insert("access_token".into(), Value::String(access.clone()));
        if let Some(r) = text(v.get("refresh_token")) {
            map.insert("refresh_token".into(), Value::String(r));
        }
        let expires_in = num(v.get("expires_in"));
        match expires_in {
            Some(s) => {
                map.insert("expires_in".into(), Value::from(s));
            }
            None => {
                map.remove("expires_in");
            }
        }
        for k in ["token_type", "scope", "api_domain"] {
            if let Some(x) = text(v.get(k)) {
                map.insert(k.into(), Value::String(x));
            }
        }
        write_file(&self.path, &map)?;
        let tok = Token {
            access: access.clone(),
            expires_at: expires_in.map(|s| SystemTime::now() + Duration::from_secs(s)),
        };
        self.adopt(&tok);
        Ok(access)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding() {
        assert_eq!(
            form_encode(&[("a", "x y"), ("r", "https://m.example.test/p?q=1&z")]),
            "a=x+y&r=https%3A%2F%2Fm.example.test%2Fp%3Fq%3D1%26z"
        );
    }

    #[test]
    fn read_and_write_keep_keys_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tok.json");
        std::fs::write(&p, r#"{"access_token":"made-up-a","refresh_token":"made-up-r","expires_in":3600,"extra":1}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (mut map, tok) = read_file(&p).unwrap();
        assert_eq!(tok.access, "made-up-a");
        assert!(!tok.expired());
        assert!(!format!("{tok:?}").contains("made-up"));
        map.insert("access_token".into(), "made-up-b".into());
        write_file(&p, &map).unwrap();
        let (map2, tok2) = read_file(&p).unwrap();
        assert_eq!(
            (tok2.access.as_str(), map2["extra"].as_i64()),
            ("made-up-b", Some(1))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file left behind"
        );
        std::fs::write(&p, r#"{"access_token":"x","expires_in":"0"}"#).unwrap();
        assert!(read_file(&p).unwrap().1.expired());
    }
}
