//! The HTTP side: one `reqwest` client per account, a small client-side rate limiter, 429 handling
//! (Retry-After) and Resend's errors mapped to core `Error` kinds. The API key is only ever put in the
//! `Authorization` header: it is never in a URL, a log line or an error (messages from Resend are
//! scrubbed of anything that looks like a key before they become an `Error`).

use crate::config::{ApiKey, ResendConfig};
use lumbridge_mail_core::{Error, Result};
use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

/// Spaces requests at least `interval` apart and honours waits a 429 asked for. Shared by every request
/// of one account.
pub(crate) struct Limiter {
    interval: Duration,
    next: tokio::sync::Mutex<Instant>,
}

impl Limiter {
    pub fn new(requests_per_second: f64) -> Self {
        let rps = if requests_per_second.is_finite() && requests_per_second > 0.0 { requests_per_second } else { 1.0 };
        Self { interval: Duration::from_secs_f64(1.0 / rps), next: tokio::sync::Mutex::new(Instant::now()) }
    }

    /// Waits for this request's turn.
    pub async fn wait(&self) {
        let mut next = self.next.lock().await;
        let now = Instant::now();
        if *next > now {
            tokio::time::sleep_until(*next).await;
        }
        *next = Instant::now().max(*next) + self.interval;
    }

    /// No request before `d` from now (a 429's Retry-After).
    pub async fn hold(&self, d: Duration) {
        let mut next = self.next.lock().await;
        let until = Instant::now() + d;
        if until > *next {
            *next = until;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
}

pub(crate) struct Client {
    base: String,
    key: ApiKey,
    http: reqwest::Client,
    limiter: Limiter,
    max_rate_retries: u32,
    max_retry_wait: Duration,
}

/// `https://...`, or `http://` to a loopback address (tests). Anything else would put the key on the wire.
pub(crate) fn check_url(url: &str, what: &str) -> Result<()> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = lower.strip_prefix("http://") {
        let host = rest.split(['/', '?']).next().unwrap_or("");
        let host = match host.strip_prefix('[') {
            Some(h) => h.split(']').next().unwrap_or(""),
            None => host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host),
        };
        if matches!(host, "127.0.0.1" | "localhost" | "::1") {
            return Ok(());
        }
    }
    Err(Error::Refused(format!("{what} must be an https:// URL (plain http only to this machine)")))
}

/// Resend's messages may echo what was sent; never let anything key-like through, and keep it short.
pub(crate) fn scrub(msg: &str, key: &ApiKey) -> String {
    let mut s = if key.is_empty() { msg.to_owned() } else { msg.replace(key.expose(), "<key>") };
    // Anything shaped like a Resend key (`re_` + 8 or more key characters).
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find("re_") {
        let tail = &rest[i + 3..];
        let n = tail.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').count();
        let starts_word = rest[..i].chars().last().is_none_or(|c| !c.is_ascii_alphanumeric());
        out.push_str(&rest[..i]);
        if n >= 8 && starts_word {
            out.push_str("re_<redacted>");
            rest = &tail[n..];
        } else {
            out.push_str("re_");
            rest = tail;
        }
    }
    out.push_str(rest);
    s = out.chars().filter(|c| !c.is_control()).take(300).collect();
    s
}

/// Resend's error body: `{"statusCode": 422, "name": "validation_error", "message": "..."}`.
fn error_parts(body: &[u8]) -> (String, String) {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let name = v.get("name").or_else(|| v.get("type")).and_then(Value::as_str).unwrap_or("").to_owned();
    let message = v.get("message").or_else(|| v.get("error")).and_then(Value::as_str).unwrap_or("").to_owned();
    (name, message)
}

/// Status + Resend's error name to a core error (see the README's table).
pub(crate) fn map_error(status: u16, name: &str, message: &str, key: &ApiKey) -> Error {
    let m = scrub(message, key);
    let said = |what: &str| if m.is_empty() { what.to_owned() } else { format!("{what}: {m}") };
    let key_problem = message.to_ascii_lowercase().contains("api key");
    match (status, name) {
        (401, "restricted_api_key") => Error::Unsupported(
            "this Resend API key can only send; reading sent or received mail needs a full-access key".into(),
        ),
        (401, _) | (400 | 403, "missing_api_key" | "suspended_api_key" | "restricted_api_key" | "invalid_permission") => {
            Error::Auth(said(&format!("Resend did not accept the API key ({})", if name.is_empty() { "unauthorized" } else { name })))
        }
        (400 | 403, "validation_error") if key_problem => Error::Auth(said("Resend did not accept the API key")),
        (403, "email_above_quota") => Error::Unsupported(said("Resend kept no content for this email (over quota when received)")),
        (404, _) => Error::NotFound(said("not found at Resend")),
        (409, "concurrent_idempotent_requests" | "resource_locked") => Error::Offline(said("Resend is busy with the same request")),
        (409, "invalid_idempotent_request") => Error::Refused(said(
            "this draft was already sent once with different content (same idempotency key); not sending again",
        )),
        (429, _) => Error::Offline(said("Resend rate limit or quota")),
        (500..=599, _) => Error::Offline(said(&format!("Resend error {status}"))),
        (403, _) => Error::Refused(said("Resend refused")),
        _ => Error::Invalid(said(&format!("Resend said {status}{}", if name.is_empty() { String::new() } else { format!(" {name}") }))),
    }
}

fn transport(e: reqwest::Error) -> Error {
    let what = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else if e.is_body() || e.is_decode() {
        "the answer was cut off"
    } else {
        "request failed"
    };
    Error::Offline(format!("Resend: {what}"))
}

/// Seconds from `Retry-After` (a number; HTTP dates are treated as 1 s), else `ratelimit-reset`.
fn retry_wait(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs = |name: &str| h.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok());
    let s = secs("retry-after").or_else(|| h.get("retry-after").map(|_| 1.0)).or_else(|| secs("ratelimit-reset"))?;
    (s.is_finite() && s >= 0.0).then(|| Duration::from_secs_f64(s))
}

impl Client {
    pub fn new(cfg: &ResendConfig) -> Result<Self> {
        check_url(&cfg.base_url, "the Resend base URL")?;
        if cfg.key.is_empty() {
            return Err(Error::Auth("no Resend API key".into()));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(cfg.timeout)
            .user_agent("lumbridge-mail")
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| Error::Other(format!("http client: {e}")))?;
        Ok(Self {
            base: cfg.base_url.trim_end_matches('/').to_owned(),
            key: cfg.key.clone(),
            http,
            limiter: Limiter::new(cfg.requests_per_second),
            max_rate_retries: cfg.max_rate_retries,
            max_retry_wait: cfg.max_retry_wait,
        })
    }

    /// One API call: `path` starts with `/` and may carry a query string we built from safe values.
    /// Waits for the limiter, retries 429s (not quota errors) after Retry-After, maps errors.
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>, idempotency_key: Option<&str>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut tries = 0u32;
        loop {
            self.limiter.wait().await;
            let mut r = match method {
                Method::Get => self.http.get(&url),
                Method::Post => self.http.post(&url),
            };
            r = r.bearer_auth(self.key.expose()).header("accept", "application/json");
            if let Some(k) = idempotency_key {
                r = r.header("idempotency-key", k);
            }
            if let Some(b) = body {
                r = r.json(b);
            }
            let resp = r.send().await.map_err(transport)?;
            let status = resp.status().as_u16();
            let headers = resp.headers().clone();
            let bytes = resp.bytes().await.map_err(transport)?;
            if (200..300).contains(&status) {
                if bytes.iter().all(|b| b.is_ascii_whitespace()) {
                    return Ok(Value::Null);
                }
                return serde_json::from_slice(&bytes).map_err(|_| Error::Other(format!("Resend: unexpected answer to {}", path_only(path))));
            }
            let (name, message) = error_parts(&bytes);
            if status == 429 && !name.contains("quota") && tries < self.max_rate_retries {
                tries += 1;
                let wait = retry_wait(&headers).unwrap_or(Duration::from_secs(1)).min(self.max_retry_wait);
                self.limiter.hold(wait).await;
                continue;
            }
            return Err(map_error(status, &name, &message, &self.key));
        }
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(Method::Get, path, None, None).await
    }

    /// A signed download link from Resend (attachments, raw messages): fetched at once, without the API
    /// key, and refused past `cap` bytes.
    pub async fn download(&self, url: &str, cap: u64) -> Result<Vec<u8>> {
        check_url(url, "a download link")?;
        let mut resp = self.http.get(url).send().await.map_err(transport)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(match status {
                403 | 404 | 410 => Error::NotFound("the download link expired or is gone; open the message again".into()),
                500..=599 => Error::Offline(format!("download failed ({status})")),
                _ => Error::Other(format!("download failed ({status})")),
            });
        }
        if resp.content_length().is_some_and(|n| n > cap) {
            return Err(Error::Refused(format!("larger than the {} MB download limit", cap >> 20)));
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(transport)? {
            if out.len() as u64 + chunk.len() as u64 > cap {
                return Err(Error::Refused(format!("larger than the {} MB download limit", cap >> 20)));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

fn path_only(p: &str) -> &str {
    p.split('?').next().unwrap_or(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_scrubbed() {
        let k = ApiKey::new("re_madeupkey_12345678");
        assert_eq!(scrub("bad key re_madeupkey_12345678 here", &k), "bad key <key> here");
        assert_eq!(scrub("other re_AbCdEfGh123 x", &k), "other re_<redacted> x");
        assert_eq!(scrub("store_items are fine", &k), "store_items are fine");
        assert_eq!(scrub("a\nb", &k), "ab");
    }

    #[test]
    fn errors_map_to_kinds() {
        let k = ApiKey::new("re_madeupkey_12345678");
        assert!(matches!(map_error(401, "missing_api_key", "", &k), Error::Auth(_)));
        assert!(matches!(map_error(400, "validation_error", "API key is invalid", &k), Error::Auth(_)));
        assert!(matches!(map_error(403, "validation_error", "The example.com domain is not verified", &k), Error::Refused(_)));
        assert!(matches!(map_error(401, "restricted_api_key", "", &k), Error::Unsupported(_)));
        assert!(matches!(map_error(404, "not_found", "", &k), Error::NotFound(_)));
        assert!(matches!(map_error(409, "invalid_idempotent_request", "", &k), Error::Refused(_)));
        assert!(matches!(map_error(409, "concurrent_idempotent_requests", "", &k), Error::Offline(_)));
        assert!(matches!(map_error(503, "service_unavailable", "", &k), Error::Offline(_)));
        assert!(matches!(map_error(422, "missing_required_field", "", &k), Error::Invalid(_)));
        let e = map_error(401, "validation_error", "API key re_madeupkey_12345678 is invalid", &k);
        assert!(!e.to_string().contains("madeupkey"), "{e}");
    }

    #[test]
    fn only_https_or_loopback() {
        assert!(check_url("https://api.resend.com", "x").is_ok());
        assert!(check_url("http://127.0.0.1:4000/x", "x").is_ok());
        assert!(check_url("http://localhost/x", "x").is_ok());
        assert!(check_url("http://[::1]:80/x", "x").is_ok());
        assert!(check_url("http://api.resend.com", "x").is_err());
        assert!(check_url("http://127.0.0.1.evil.example/", "x").is_err());
        assert!(check_url("ftp://x", "x").is_err());
    }
}
