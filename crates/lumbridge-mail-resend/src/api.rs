//! Resend's JSON, as we read it (from the OpenAPI spec, resend/resend-openapi, and the API reference;
//! see the README for links). Every field is optional and unknown fields are ignored, so a new field or
//! a missing one never breaks a sync.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// `null`, a string or a list of strings (Resend uses both for `to`, `cc`, `reply_to`).
fn strings<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => vec![s],
        Value::Array(a) => a.into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    })
}

fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) if !s.is_empty() => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn number<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

/// `{"object": "list", "has_more": bool, "data": [...]}`.
#[derive(Debug, Clone, Deserialize)]
pub struct List<T> {
    #[serde(default = "Vec::new")]
    pub data: Vec<T>,
    #[serde(default)]
    pub has_more: bool,
}

/// An email this team sent (`GET /emails`, `GET /emails/{id}`). The list leaves out `html`/`text`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SentEmail {
    #[serde(default)]
    pub id: String,
    #[serde(default, deserialize_with = "opt_string")]
    pub message_id: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub from: Option<String>,
    #[serde(default, deserialize_with = "strings")]
    pub to: Vec<String>,
    #[serde(default, deserialize_with = "strings")]
    pub cc: Vec<String>,
    #[serde(default, deserialize_with = "strings")]
    pub bcc: Vec<String>,
    #[serde(default, deserialize_with = "strings")]
    pub reply_to: Vec<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub subject: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub scheduled_at: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub last_event: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub html: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub text: Option<String>,
}

/// One attachment of a received or sent email. `download_url` (valid about an hour) only from the
/// attachment endpoints and the attachment lists.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AttachmentInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default, deserialize_with = "opt_string")]
    pub filename: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub content_type: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub content_id: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub content_disposition: Option<String>,
    #[serde(default, deserialize_with = "number")]
    pub size: u64,
    #[serde(default, deserialize_with = "opt_string")]
    pub download_url: Option<String>,
}

/// `raw` on a received email: a signed link to the original message.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawLink {
    #[serde(default, deserialize_with = "opt_string")]
    pub download_url: Option<String>,
}

/// Mail received on a domain (`GET /emails/receiving`, `GET /emails/receiving/{id}`). The list leaves
/// out `html`, `text`, `headers` and `raw`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReceivedEmail {
    #[serde(default)]
    pub id: String,
    #[serde(default, deserialize_with = "opt_string")]
    pub message_id: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub from: Option<String>,
    #[serde(default, deserialize_with = "strings")]
    pub to: Vec<String>,
    #[serde(default, deserialize_with = "strings")]
    pub cc: Vec<String>,
    #[serde(default, deserialize_with = "strings")]
    pub bcc: Vec<String>,
    #[serde(default, deserialize_with = "strings")]
    pub reply_to: Vec<String>,
    #[serde(default, deserialize_with = "strings")]
    pub received_for: Vec<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub subject: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub html: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    pub text: Option<String>,
    /// Header name to a string or a list of strings (names as Resend gives them, usually lower case).
    #[serde(default)]
    pub headers: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub attachments: Vec<AttachmentInfo>,
    #[serde(default)]
    pub raw: Option<RawLink>,
    /// `{spf, dkim, dmarc}`: `pass` | `fail` | `gray` | `processing_failed` | `unknown`.
    #[serde(default)]
    pub authentication: Option<serde_json::Map<String, Value>>,
}

/// `POST /emails` answer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Created {
    #[serde(default)]
    pub id: String,
}

/// A domain (`GET /domains`): what `lbmail resend domains` shows and what sending checks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `pending` | `verified` | `failed` | `not_started` | `partially_verified` | `partially_failed`.
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<DomainCapabilities>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainCapabilities {
    /// `enabled` | `disabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sending: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiving: Option<String>,
}

impl DomainInfo {
    pub fn can_send(&self) -> bool {
        matches!(self.status.as_str(), "verified" | "partially_verified")
            && self.capabilities.as_ref().and_then(|c| c.sending.as_deref()) != Some("disabled")
    }
    pub fn receives(&self) -> bool {
        self.capabilities.as_ref().and_then(|c| c.receiving.as_deref()) == Some("enabled")
    }
}

/// An API key as `GET /api-keys` lists it: a name and dates. Resend never returns a key's value after
/// it was created, and neither do we.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_shapes() {
        let e: SentEmail = serde_json::from_str(r#"{"id":"a","to":"x@y.example","cc":null,"last_event":"delivered","bcc":["b@y.example"],"new_field":1}"#).unwrap();
        assert_eq!(e.to, vec!["x@y.example"]);
        assert!(e.cc.is_empty());
        assert_eq!(e.bcc.len(), 1);
        let r: ReceivedEmail = serde_json::from_str(r#"{"id":"r","subject":null,"attachments":[{"id":"t","size":"12"}],"raw":null}"#).unwrap();
        assert_eq!(r.subject, None);
        assert_eq!(r.attachments[0].size, 12);
        let l: List<ApiKeyInfo> = serde_json::from_str(r#"{"object":"list","data":[{"id":"k","name":"site"}]}"#).unwrap();
        assert!(!l.has_more && l.data[0].name == "site");
    }
}
