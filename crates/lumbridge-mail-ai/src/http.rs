//! [`Model`] over an OpenAI-compatible `/chat/completions` endpoint (vLLM, llama.cpp, OpenAI, ...).
//!
//! No streaming. Timeouts on connect and on the whole call, one retry for failures that look passing
//! (connect errors, timeouts, 429, 5xx). Errors carry the status only, never the server's body, which
//! may echo the prompt and so the mail.

use crate::config::EndpointConfig;
use crate::model::{ChatRequest, Model};
use lumbridge_mail_core::safety::ModelPlace;
use lumbridge_mail_core::{Error, Result};
use serde_json::{Value, json};
use std::time::Duration;

pub struct OpenAiModel {
    config: EndpointConfig,
    url: String,
    client: reqwest::Client,
}

impl OpenAiModel {
    /// Fails when no endpoint is set (`base_url` or `model` empty): the caller then runs on rules only.
    pub fn new(mut config: EndpointConfig) -> Result<Self> {
        if !config.is_set() {
            return Err(Error::Invalid(format!(
                "no AI endpoint set: give [ai] base_url and model in config.toml, or {} and {}",
                crate::config::ENV_BASE,
                crate::config::ENV_MODEL
            )));
        }
        if config.label.trim().is_empty() {
            config.label = config.model.clone();
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_s.max(1)))
            .timeout(Duration::from_secs(config.timeout_s.max(1)))
            .no_proxy()
            .build()
            .map_err(|e| Error::Other(format!("http client: {e}")))?;
        let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
        Ok(Self { config, url, client })
    }

    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// The request body, separate so tests can check it without a server.
    pub fn body(&self, req: &ChatRequest) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "messages": req.messages,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "stream": false,
        });
        if let (Some(s), true) = (&req.schema, self.config.json_schema) {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": s.name, "strict": true, "schema": s.schema },
            });
        }
        if let Some(t) = self.config.thinking {
            body["chat_template_kwargs"] = json!({ "enable_thinking": t });
        }
        body
    }

    async fn once(&self, body: &Value, task: &str) -> Result<String> {
        let mut r = self.client.post(&self.url).json(body).header("x-lumbridge-mail-task", task);
        for (k, v) in &self.config.headers {
            r = r.header(k.as_str(), v.as_str());
        }
        if let Some(k) = &self.config.api_key {
            r = r.bearer_auth(k);
        }
        let resp = r.send().await.map_err(|e| {
            if e.is_timeout() {
                Error::Offline(format!("model timed out after {}s", self.config.timeout_s))
            } else {
                Error::Offline(format!("model unreachable at {}", self.config.base_url))
            }
        })?;
        let status = resp.status();
        if !status.is_success() {
            let s = status.as_u16();
            return Err(match s {
                401 | 403 => Error::Auth(format!("model endpoint said {s}")),
                408 | 429 | 500..=599 => Error::Offline(format!("model endpoint said {s}")),
                _ => Error::Invalid(format!("model endpoint said {s}")),
            });
        }
        let v: Value = resp.json().await.map_err(|e| {
            if e.is_timeout() { Error::Offline("model timed out".into()) } else { Error::Other("model answer was not JSON".into()) }
        })?;
        match v.pointer("/choices/0/message/content").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => Ok(s.to_string()),
            _ => Err(Error::NoAnswer("model gave an empty answer".into())),
        }
    }
}

#[async_trait::async_trait]
impl Model for OpenAiModel {
    fn place(&self) -> ModelPlace {
        self.config.place
    }
    fn label(&self) -> &str {
        &self.config.label
    }
    async fn chat(&self, req: &ChatRequest) -> Result<String> {
        let body = self.body(req);
        match self.once(&body, req.task).await {
            Err(e) if e.is_retryable() => {
                tokio::time::sleep(Duration::from_millis(400)).await;
                self.once(&body, req.task).await
            }
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatMessage, JsonSchema};

    fn req() -> ChatRequest {
        ChatRequest {
            task: "triage",
            messages: vec![ChatMessage::system("s"), ChatMessage::user("u")],
            schema: Some(JsonSchema { name: "t", schema: json!({"type": "object"}) }),
            max_tokens: 10,
            temperature: 0.0,
        }
    }

    /// An endpoint with a made-up model name, for tests.
    fn ep(base_url: &str) -> EndpointConfig {
        EndpointConfig {
            base_url: base_url.into(),
            model: "test-model".into(),
            headers: vec![("x-client".into(), "lumbridge-mail".into())],
            ..Default::default()
        }
    }

    #[test]
    fn no_endpoint_no_model() {
        let e = OpenAiModel::new(EndpointConfig::default()).err().unwrap();
        assert!(matches!(e, Error::Invalid(ref m) if m.contains("LBMAIL_AI_BASE")), "{e:?}");
        let m = OpenAiModel::new(ep("http://127.0.0.1:8000/v1")).unwrap();
        assert_eq!(m.label(), "test-model", "the label falls back to the model name");
    }

    #[test]
    fn body_has_schema_and_thinking_switch() {
        let m = OpenAiModel::new(ep("http://127.0.0.1:8000/v1")).unwrap();
        let b = m.body(&req());
        assert_eq!(b["model"], "test-model");
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["response_format"]["type"], "json_schema");
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
        let m = OpenAiModel::new(EndpointConfig { thinking: None, json_schema: false, ..ep("http://127.0.0.1:8000/v1") }).unwrap();
        let b = m.body(&req());
        assert!(b.get("response_format").is_none() && b.get("chat_template_kwargs").is_none());
    }

    /// A one-file HTTP server: answers each connection with the next canned (status, body) and hands back
    /// the requests it saw.
    async fn serve(answers: Vec<(u16, &'static str)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in answers {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len = text[..i].lines().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
                        });
                        if buf.len() >= i + 4 + len.unwrap_or(0) {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&buf).to_string());
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                s.write_all(resp.as_bytes()).await.unwrap();
                s.shutdown().await.ok();
            }
            seen
        });
        (base, h)
    }

    #[tokio::test]
    async fn a_5xx_is_retried_once_and_headers_are_sent() {
        let ok = r#"{"choices":[{"message":{"role":"assistant","content":"{\"a\":1}"}}]}"#;
        let (base, h) = serve(vec![(503, "busy"), (200, ok)]).await;
        let m = OpenAiModel::new(ep(&base)).unwrap();
        assert_eq!(m.chat(&req()).await.unwrap(), r#"{"a":1}"#);
        let seen = h.await.unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].to_lowercase().contains("x-client: lumbridge-mail"));
        assert!(seen[1].starts_with("POST /v1/chat/completions"));
    }

    #[tokio::test]
    async fn errors_never_carry_the_servers_body() {
        let (base, _h) = serve(vec![(400, "{\"error\":\"bad prompt: SECRET MAIL TEXT\"}")]).await;
        let m = OpenAiModel::new(ep(&base)).unwrap();
        let e = m.chat(&req()).await.unwrap_err();
        assert_eq!(e, Error::Invalid("model endpoint said 400".into()));
        let (base, _h) = serve(vec![(401, "no")]).await;
        let m = OpenAiModel::new(ep(&base)).unwrap();
        assert!(matches!(m.chat(&req()).await.unwrap_err(), Error::Auth(_)));
    }

    #[tokio::test]
    async fn unreachable_is_offline_after_one_retry() {
        let m = OpenAiModel::new(EndpointConfig { connect_timeout_s: 1, timeout_s: 2, ..ep("http://127.0.0.1:9/v1") })
        .unwrap();
        let e = m.chat(&req()).await.unwrap_err();
        assert!(matches!(e, Error::Offline(_)), "{e:?}");
    }
}
