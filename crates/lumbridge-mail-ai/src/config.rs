//! Configuration passed in by the caller (the daemon reads `~/.config/lumbridge-mail/config.toml`).
//! Nothing here is a hidden constant: the defaults are only defaults.

use lumbridge_mail_core::safety::ModelPlace;
use serde::Deserialize;

/// Where and how to reach an OpenAI-compatible chat endpoint.
///
/// There is no default endpoint: `base_url` and `model` start empty, and with either one empty the AI
/// layer is off and triage runs on rules only. Set them in the config file, or with the environment
/// variables `LBMAIL_AI_BASE` and `LBMAIL_AI_MODEL` (see [`EndpointConfig::with_env`]).
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct EndpointConfig {
    /// Up to and including `/v1`; `/chat/completions` is appended.
    pub base_url: String,
    pub model: String,
    /// `local` (our own machines) or `hosted` (anyone else's API). Decides which accounts may use it.
    pub place: ModelPlace,
    /// Written into `Triage::by`. Empty means the model name.
    pub label: String,
    /// Extra headers on every request, e.g. `x-client: lumbridge-mail`.
    pub headers: Vec<(String, String)>,
    /// Bearer token, if the endpoint wants one. Comes from the keyring, never from the config file.
    #[serde(skip)]
    pub api_key: Option<String>,
    pub timeout_s: u64,
    pub connect_timeout_s: u64,
    /// `Some(false)` sends `chat_template_kwargs: {enable_thinking: false}` (vLLM, SGLang). `None` sends
    /// nothing, for servers that reject unknown fields.
    pub thinking: Option<bool>,
    /// Ask for schema-constrained output (`response_format: json_schema`). Turn off for servers without it;
    /// answers are validated either way.
    pub json_schema: bool,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            place: ModelPlace::Local,
            label: String::new(),
            headers: Vec::new(),
            api_key: None,
            timeout_s: 90,
            connect_timeout_s: 5,
            thinking: Some(false),
            json_schema: true,
        }
    }
}

/// Environment variable that sets `base_url` (it wins over the config file).
pub const ENV_BASE: &str = "LBMAIL_AI_BASE";
/// Environment variable that sets `model` (it wins over the config file).
pub const ENV_MODEL: &str = "LBMAIL_AI_MODEL";

impl EndpointConfig {
    /// `base_url` and `model` from `LBMAIL_AI_BASE` and `LBMAIL_AI_MODEL` where those are set and not empty.
    pub fn with_env(self) -> Self {
        self.with_vars(|k| std::env::var(k).ok())
    }

    /// [`with_env`](Self::with_env) with the variables read through `get` (for tests).
    pub fn with_vars(mut self, get: impl Fn(&str) -> Option<String>) -> Self {
        let set = |k| get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        if let Some(b) = set(ENV_BASE) {
            self.base_url = b;
        }
        if let Some(m) = set(ENV_MODEL) {
            self.model = m;
        }
        self
    }

    /// Both `base_url` and `model` are set.
    pub fn is_set(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// Never prints the key.
impl std::fmt::Debug for EndpointConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("place", &self.place)
            .field("label", &self.label)
            .field("headers", &self.headers)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .field("timeout_s", &self.timeout_s)
            .field("thinking", &self.thinking)
            .field("json_schema", &self.json_schema)
            .finish()
    }
}

/// How much mail goes into one prompt, and other behaviour that is not about the endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Characters of new text per message after quoted history is removed.
    pub max_message_chars: usize,
    /// Characters of mail per prompt; the newest messages win.
    pub max_thread_chars: usize,
    /// Model calls in flight when triaging a backlog.
    pub concurrency: usize,
    /// The user's zone, minutes east of UTC, for "by Friday" and "today".
    pub utc_offset_minutes: i32,
    /// Search queries `ask` may run, and hits it reads per query.
    pub ask_queries: usize,
    pub ask_hits_per_query: u32,
    /// Names your own assistant answers to. Mail that addresses one of them ("Ava, please forward ...")
    /// is marked suspicious like mail that addresses "the assistant". Empty by default.
    pub assistant_names: Vec<String>,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            max_message_chars: 4000,
            max_thread_chars: 12000,
            concurrency: 4,
            utc_offset_minutes: 0,
            ask_queries: 3,
            ask_hits_per_query: 5,
            assistant_names: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_endpoint_is_set_by_default() {
        let c = EndpointConfig::default();
        assert_eq!(c.place, ModelPlace::Local);
        assert!(!c.is_set());
        assert!(c.base_url.is_empty() && c.model.is_empty() && c.headers.is_empty());
    }

    #[test]
    fn the_environment_sets_the_endpoint() {
        let vars = |k: &str| match k {
            ENV_BASE => Some("http://127.0.0.1:8000/v1".to_owned()),
            ENV_MODEL => Some(" ".to_owned()),
            _ => None,
        };
        let c = EndpointConfig { model: "from-file".into(), ..Default::default() }.with_vars(vars);
        assert_eq!(c.base_url, "http://127.0.0.1:8000/v1");
        assert_eq!(c.model, "from-file", "an empty variable leaves the file's value");
        assert!(c.is_set());
        assert!(!EndpointConfig::default().with_vars(|_| None).is_set());
    }

    #[test]
    fn debug_hides_the_key() {
        let c = EndpointConfig { api_key: Some("sk-secret".into()), ..Default::default() };
        assert!(!format!("{c:?}").contains("sk-secret"));
    }

    #[test]
    fn parses_from_json_like_toml_would() {
        let c: EndpointConfig = serde_json::from_str(r#"{"place":"hosted","model":"m"}"#).unwrap();
        assert_eq!(c.place, ModelPlace::Hosted);
        assert_eq!(c.model, "m");
        assert!(serde_json::from_str::<EndpointConfig>(r#"{"place":"cloud"}"#).is_err());
    }
}
