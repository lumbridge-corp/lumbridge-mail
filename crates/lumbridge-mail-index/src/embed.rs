//! Embeddings over an OpenAI-compatible `/v1/embeddings` endpoint (vLLM, NIM, llama.cpp, OpenAI).
//!
//! The configured headers go on every request (production names an untraced client, so mail is not
//! archived by the tracing proxy). Errors carry the status only, never the server's body, which may
//! echo the input and so the mail. No retries here: the worker retries a job with backoff, and a
//! search falls back to full text.

use lumbridge_mail_core::safety::ModelPlace;
use lumbridge_mail_core::{Error, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// Where and how to reach the embedding endpoint (`[pg.embed]` in the daemon's config).
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EmbedEndpoint {
    /// Up to and including `/v1`; `/embeddings` is appended. Empty: not configured.
    pub base_url: String,
    /// Sent as `model` and written into `mail.embeddings.model`.
    pub model: String,
    /// `local` (our own machines) or `hosted`. Accounts with AI `local_only` only go to `local`.
    pub place: ModelPlace,
    /// Extra headers on every request, e.g. `[["x-no-trace", "1"]]`.
    pub headers: Vec<(String, String)>,
    /// Environment variable holding a bearer token, if the endpoint wants one (never the token).
    pub api_key_env: Option<String>,
    pub timeout_s: u64,
    pub connect_timeout_s: u64,
    /// Send `input_type: "passage"` / `"query"` (NVIDIA NIM and other asymmetric models want it).
    pub input_type: bool,
    /// Prefixes some models want on their input ("query: ", "passage: ").
    pub query_prefix: String,
    pub passage_prefix: String,
    /// Ask for this many dimensions (Matryoshka models); must match the column.
    pub dimensions: Option<u32>,
}

impl Default for EmbedEndpoint {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            place: ModelPlace::Local,
            headers: Vec::new(),
            api_key_env: None,
            timeout_s: 60,
            connect_timeout_s: 5,
            input_type: false,
            query_prefix: String::new(),
            passage_prefix: String::new(),
            dimensions: None,
        }
    }
}

impl std::fmt::Debug for EmbedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedEndpoint")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("place", &self.place)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("api_key_env", &self.api_key_env)
            .finish()
    }
}

impl EmbedEndpoint {
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// What is being embedded: stored mail, or a search query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Passage,
    Query,
}

#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// Where it runs: checked against `safety::ai_allowed` before any mail reaches it.
    fn place(&self) -> ModelPlace;
    /// The model name written with every vector.
    fn model(&self) -> &str;
    /// One vector per input, in order. `Error::Offline` for timeouts, unreachable, 429 and 5xx.
    async fn embed(&self, inputs: &[String], kind: InputKind) -> Result<Vec<Vec<f32>>>;
}

pub struct OpenAiEmbedder {
    config: EmbedEndpoint,
    url: String,
    client: reqwest::Client,
    api_key: Option<String>,
}

impl OpenAiEmbedder {
    /// `api_key` is read by the caller (from `api_key_env`).
    pub fn new(config: EmbedEndpoint, api_key: Option<String>) -> Result<Self> {
        if !config.is_configured() {
            return Err(Error::Invalid(
                "the embedding endpoint needs base_url and model".into(),
            ));
        }
        let mut headers = HeaderMap::new();
        for (k, v) in &config.headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| Error::Invalid(format!("embedding header name {k:?}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|_| Error::Invalid(format!("embedding header {k:?}: bad value")))?;
            headers.append(name, value);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_s.max(1)))
            .timeout(Duration::from_secs(config.timeout_s.max(1)))
            .default_headers(headers)
            .no_proxy()
            .build()
            .map_err(|e| Error::Other(format!("http client: {e}")))?;
        let url = format!("{}/embeddings", config.base_url.trim_end_matches('/'));
        Ok(Self {
            config,
            url,
            client,
            api_key: api_key.filter(|k| !k.is_empty()),
        })
    }

    pub fn config(&self) -> &EmbedEndpoint {
        &self.config
    }

    /// The request body, separate so tests can check it without a server.
    pub fn body(&self, inputs: &[String], kind: InputKind) -> Value {
        let prefix = match kind {
            InputKind::Passage => &self.config.passage_prefix,
            InputKind::Query => &self.config.query_prefix,
        };
        let input: Vec<String> = inputs.iter().map(|t| format!("{prefix}{t}")).collect();
        let mut body = json!({
            "model": self.config.model,
            "input": input,
            "encoding_format": "float",
        });
        if self.config.input_type {
            body["input_type"] = json!(match kind {
                InputKind::Passage => "passage",
                InputKind::Query => "query",
            });
        }
        if let Some(d) = self.config.dimensions {
            body["dimensions"] = json!(d);
        }
        body
    }
}

/// Reads an OpenAI embeddings answer: one finite vector per input, all the same length, ordered by
/// `index`.
pub fn parse_answer(v: &Value, n: usize) -> Result<Vec<Vec<f32>>> {
    let bad = |what: &str| Error::Other(format!("embedding answer: {what}"));
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("no data"))?;
    if data.len() != n {
        return Err(bad(&format!("{} vectors for {n} inputs", data.len())));
    }
    let mut out: Vec<Option<Vec<f32>>> = vec![None; n];
    for (pos, d) in data.iter().enumerate() {
        let i = d
            .get("index")
            .and_then(Value::as_u64)
            .map(|i| i as usize)
            .unwrap_or(pos);
        if i >= n || out[i].is_some() {
            return Err(bad("indexes do not match the inputs"));
        }
        let e = d
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("no embedding"))?;
        let vec = e
            .iter()
            .map(|x| x.as_f64().map(|f| f as f32).filter(|f| f.is_finite()))
            .collect::<Option<Vec<f32>>>()
            .ok_or_else(|| bad("a value is not a finite number"))?;
        if vec.is_empty() {
            return Err(bad("an empty vector"));
        }
        out[i] = Some(vec);
    }
    let out: Vec<Vec<f32>> = out.into_iter().map(|v| v.unwrap_or_default()).collect();
    if out.iter().any(|v| v.len() != out[0].len()) {
        return Err(bad("vectors of different lengths"));
    }
    Ok(out)
}

#[async_trait::async_trait]
impl Embedder for OpenAiEmbedder {
    fn place(&self) -> ModelPlace {
        self.config.place
    }
    fn model(&self) -> &str {
        &self.config.model
    }
    async fn embed(&self, inputs: &[String], kind: InputKind) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let mut r = self
            .client
            .post(&self.url)
            .json(&self.body(inputs, kind))
            .header(
                "x-lumbridge-mail-task",
                match kind {
                    InputKind::Passage => "embed",
                    InputKind::Query => "embed-query",
                },
            );
        if let Some(k) = &self.api_key {
            r = r.bearer_auth(k);
        }
        let resp = r.send().await.map_err(|e| {
            if e.is_timeout() {
                Error::Offline(format!(
                    "embedding endpoint timed out after {}s",
                    self.config.timeout_s
                ))
            } else {
                Error::Offline(format!(
                    "embedding endpoint unreachable at {}",
                    self.config.base_url
                ))
            }
        })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(match status {
                401 | 403 => Error::Auth(format!("embedding endpoint said {status}")),
                408 | 429 | 500..=599 => {
                    Error::Offline(format!("embedding endpoint said {status}"))
                }
                _ => Error::Invalid(format!("embedding endpoint said {status}")),
            });
        }
        let v: Value = resp.json().await.map_err(|e| {
            if e.is_timeout() {
                Error::Offline("embedding endpoint timed out".into())
            } else {
                Error::Other("embedding answer was not JSON".into())
            }
        })?;
        parse_answer(&v, inputs.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeEmbedServer;

    fn cfg(base: &str) -> EmbedEndpoint {
        EmbedEndpoint {
            base_url: base.into(),
            model: "test-embed".into(),
            headers: vec![("x-no-trace".into(), "1".into())],
            ..EmbedEndpoint::default()
        }
    }

    #[test]
    fn body_carries_model_prefixes_and_input_type() {
        let e = OpenAiEmbedder::new(
            EmbedEndpoint {
                input_type: true,
                query_prefix: "query: ".into(),
                dimensions: Some(2048),
                ..cfg("http://127.0.0.1:9/v1")
            },
            None,
        )
        .unwrap();
        let b = e.body(&["hello".into()], InputKind::Query);
        assert_eq!(b["model"], "test-embed");
        assert_eq!(b["input"][0], "query: hello");
        assert_eq!(b["input_type"], "query");
        assert_eq!(b["dimensions"], 2048);
        let b = e.body(&["hello".into()], InputKind::Passage);
        assert_eq!(b["input"][0], "hello");
        assert_eq!(b["input_type"], "passage");
        assert!(OpenAiEmbedder::new(EmbedEndpoint::default(), None).is_err());
        assert!(
            OpenAiEmbedder::new(
                EmbedEndpoint {
                    headers: vec![("bad header".into(), "x".into())],
                    ..cfg("http://h/v1")
                },
                None
            )
            .is_err()
        );
        assert!(
            !format!("{:?}", cfg("x")).contains("\"1\""),
            "header values stay out of Debug"
        );
    }

    #[test]
    fn answers_are_checked() {
        let ok = json!({"data": [{"index": 1, "embedding": [0.0, 1.0]}, {"index": 0, "embedding": [1.0, 0.0]}]});
        assert_eq!(
            parse_answer(&ok, 2).unwrap(),
            vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            "ordered by index"
        );
        assert!(parse_answer(&ok, 3).is_err());
        assert!(
            parse_answer(
                &json!({"data": [{"embedding": [1.0]}, {"embedding": [1.0, 2.0]}]}),
                2
            )
            .is_err()
        );
        assert!(parse_answer(&json!({"data": [{"embedding": ["x"]}]}), 1).is_err());
        assert!(parse_answer(&json!({"data": [{"embedding": []}]}), 1).is_err());
        assert!(parse_answer(&json!({"error": "nope"}), 1).is_err());
    }

    #[tokio::test]
    async fn embeds_through_a_server_with_the_configured_headers() {
        let srv = FakeEmbedServer::start(8).await;
        let e = OpenAiEmbedder::new(cfg(&srv.base_url()), Some("k-123".into())).unwrap();
        let v = e
            .embed(
                &["flight to Lisbon".into(), "invoice paid".into()],
                InputKind::Passage,
            )
            .await
            .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].len(), 8);
        assert_ne!(v[0], v[1]);
        let seen = srv.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].header("x-no-trace").as_deref(), Some("1"));
        assert_eq!(
            seen[0].header("authorization").as_deref(),
            Some("Bearer k-123")
        );
        assert_eq!(seen[0].inputs, ["flight to Lisbon", "invoice paid"]);
        assert_eq!(seen[0].model, "test-embed");
    }

    #[tokio::test]
    async fn errors_never_carry_the_servers_body() {
        let srv = FakeEmbedServer::start(8).await;
        srv.fail_next(400, "{\"error\": \"bad input: SECRET MAIL TEXT\"}");
        let e = OpenAiEmbedder::new(cfg(&srv.base_url()), None).unwrap();
        let err = e
            .embed(&["x".into()], InputKind::Passage)
            .await
            .unwrap_err();
        assert_eq!(err, Error::Invalid("embedding endpoint said 400".into()));
        srv.fail_next(503, "busy");
        let err = e
            .embed(&["x".into()], InputKind::Passage)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Offline(_)) && !err.to_string().contains("busy"));
        let dead = OpenAiEmbedder::new(
            EmbedEndpoint {
                connect_timeout_s: 1,
                timeout_s: 2,
                ..cfg("http://127.0.0.1:9/v1")
            },
            None,
        )
        .unwrap();
        assert!(matches!(
            dead.embed(&["x".into()], InputKind::Query)
                .await
                .unwrap_err(),
            Error::Offline(_)
        ));
    }
}
