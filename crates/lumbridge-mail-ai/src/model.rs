//! The one thing the AI layer needs from a model: a chat call, optionally held to a JSON schema.

use lumbridge_mail_core::Result;
use lumbridge_mail_core::safety::ModelPlace;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn system(s: impl Into<String>) -> Self {
        Self { role: Role::System, content: s.into() }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self { role: Role::User, content: s.into() }
    }
    pub fn assistant(s: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: s.into() }
    }
}

/// A JSON schema the answer must follow (OpenAI `response_format: json_schema`, vLLM guided decoding).
#[derive(Debug, Clone, PartialEq)]
pub struct JsonSchema {
    pub name: &'static str,
    pub schema: serde_json::Value,
}

/// What the AI layer asks for. `task` names the call (`triage`, `draft`, ...) for tracing headers and
/// for fakes to answer by; it carries no mail.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub task: &'static str,
    pub messages: Vec<ChatMessage>,
    pub schema: Option<JsonSchema>,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// Where it runs. Checked against `safety::ai_allowed` before any mail reaches it.
    fn place(&self) -> ModelPlace;
    /// Written into `Triage::by`, e.g. `local/my-model`.
    fn label(&self) -> &str;
    /// The assistant's text. `Error::Offline` for timeouts and unreachable servers.
    async fn chat(&self, req: &ChatRequest) -> Result<String>;
}

#[async_trait::async_trait]
impl<M: Model + ?Sized> Model for std::sync::Arc<M> {
    fn place(&self) -> ModelPlace {
        (**self).place()
    }
    fn label(&self) -> &str {
        (**self).label()
    }
    async fn chat(&self, req: &ChatRequest) -> Result<String> {
        (**self).chat(req).await
    }
}
