//! The JSON gog prints, as Rust types. Shapes come from gogcli's source (v0.28.0 to 0.48 `internal/cmd`), and
//! where gog passes a Gmail API object straight through (`gmail raw`, `gmail labels list`, the
//! `message` in `gmail drafts create`), from the `google.golang.org/api/gmail/v1` structs it encodes.
//!
//! Everything is lenient: unknown fields are ignored, missing ones default, and the Go client's int64
//! fields (`historyId`, `internalDate` are strings on the wire; `sizeEstimate`, `size` are numbers) are
//! accepted either way, so a gog upgrade that changes an encoding does not break sync.

use serde::{Deserialize, Deserializer};

/// gog prints `null` for an empty list in some commands (seen live: `gmail history` with nothing new).
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A u64 that may arrive as a JSON number or a string of digits.
fn lenient_u64<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        Num(u64),
        Float(f64),
        Str(String),
        Null(()),
    }
    Ok(match N::deserialize(d)? {
        N::Num(n) => n,
        N::Float(f) if f >= 0.0 => f as u64,
        N::Float(_) => 0,
        N::Str(s) => s.trim().parse().unwrap_or(0),
        N::Null(()) => 0,
    })
}

/// `gmail raw <id> --format full|metadata|raw`: a Gmail API `Message`, encoded as is.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub label_ids: Vec<String>,
    pub snippet: String,
    #[serde(deserialize_with = "lenient_u64")]
    pub history_id: u64,
    /// Milliseconds since the epoch.
    #[serde(deserialize_with = "lenient_u64")]
    pub internal_date: u64,
    #[serde(deserialize_with = "lenient_u64")]
    pub size_estimate: u64,
    pub payload: Option<MessagePart>,
    /// base64url RFC 822 bytes, only with `--format raw`.
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MessagePart {
    pub part_id: String,
    pub mime_type: String,
    pub filename: String,
    #[serde(default, deserialize_with = "nullable")]
    pub headers: Vec<Header>,
    pub body: Option<PartBody>,
    #[serde(default, deserialize_with = "nullable")]
    pub parts: Vec<MessagePart>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PartBody {
    pub attachment_id: Option<String>,
    #[serde(deserialize_with = "lenient_u64")]
    pub size: u64,
    /// base64url (padded or not) content, after the transfer encoding is undone.
    pub data: Option<String>,
}

impl MessagePart {
    /// First header with this name, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
    }
}

/// `gmail labels list`: `{"labels": [Label]}` (no counts; those need `gmail labels get`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LabelList {
    #[serde(default, deserialize_with = "nullable")]
    pub labels: Vec<Label>,
}

/// `gmail labels get <id>`: `{"label": Label}` with counts.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LabelGet {
    pub label: Label,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Label {
    pub id: String,
    pub name: String,
    /// `system` or `user`.
    #[serde(rename = "type")]
    pub kind: String,
    pub label_list_visibility: String,
    pub message_list_visibility: String,
    #[serde(deserialize_with = "lenient_u64")]
    pub messages_total: u64,
    #[serde(deserialize_with = "lenient_u64")]
    pub messages_unread: u64,
}

/// `gmail messages search <q>`: `{"messages": [MessageItem], "nextPageToken": ""}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MessageSearch {
    #[serde(default, deserialize_with = "nullable")]
    pub messages: Vec<MessageItem>,
    pub next_page_token: String,
}

/// One row of `gmail messages search`. `labels` are label *names* (gog maps ids through the label list;
/// system labels keep their id as name). What else is there depends on gog's version and our flags:
///
/// | field | from |
/// |---|---|
/// | `date` (`2006-01-02 15:04` in `--timezone`, from the Date header) | every version |
/// | `from`, `subject` | every version |
/// | `body` (best text part, else the HTML), `attachments` | `--include-body` (every version) |
/// | `internalDateIso` (RFC 3339, Gmail's internalDate) | 0.34.2 |
/// | `to`, `cc`, `bcc` | `--include-recipients`, 0.44.0 |
/// | `headers` (`{"Message-Id": ...}`, canonical names, first value) | `--headers a,b`, 0.47.0 |
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MessageItem {
    pub id: String,
    pub thread_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub labels: Vec<String>,
    pub date: String,
    pub internal_date_iso: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: String,
    #[serde(default, deserialize_with = "nullable")]
    pub attachments: Vec<AttachmentItem>,
}

impl MessageItem {
    /// A header from `headers`, matched without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
    }
}

/// An attachment in a search row (`--include-body` / `--include-attachments`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AttachmentItem {
    pub filename: String,
    #[serde(deserialize_with = "lenient_u64")]
    pub size: u64,
    pub mime_type: String,
}

/// `gmail thread get <threadId>`: `{"thread": <Gmail API Thread, format full>, "downloaded": [...]}`
/// (the same shape from v0.28 to 0.48).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThreadGet {
    pub thread: Thread,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Thread {
    pub id: String,
    #[serde(deserialize_with = "lenient_u64")]
    pub history_id: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub messages: Vec<Message>,
}

/// `gmail history --since <id>`: `{"historyId": "...", "messages": ["id", ...], "nextPageToken": ""}`.
/// gog asks Gmail only for `messageAdded` records, so this lists new messages, not label changes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct History {
    #[serde(deserialize_with = "lenient_u64")]
    pub history_id: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub messages: Vec<String>,
    pub next_page_token: String,
}

/// `gmail drafts list`: `{"drafts": [{"id", "messageId", "threadId"}], "nextPageToken": ""}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DraftList {
    #[serde(default, deserialize_with = "nullable")]
    pub drafts: Vec<DraftItem>,
    pub next_page_token: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DraftItem {
    pub id: String,
    pub message_id: String,
    pub thread_id: String,
}

/// `gmail drafts create|update`: `{"draftId": "...", "message": {...}, "threadId": "..."}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DraftResult {
    pub draft_id: String,
    pub thread_id: String,
}

/// `gog gmail send`: `{"from": ..., "messageId": ..., "threadId": ...}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SendResult {
    pub message_id: String,
    pub thread_id: String,
}

/// `gog version`: `{"version": "v0.29.0", "commit": "...", "date": "..."}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Version {
    pub version: String,
}

impl Version {
    /// `(major, minor, patch)` from `v0.29.0`, `0.41.2-dev` and the like.
    pub fn parse(&self) -> Option<(u32, u32, u32)> {
        let v = self.version.trim().trim_start_matches(['v', 'V']);
        let core = v.split(['-', '+', ' ']).next()?;
        let mut it = core.split('.').map(|p| p.parse::<u32>());
        Some((it.next()?.ok()?, it.next().unwrap_or(Ok(0)).ok()?, it.next().unwrap_or(Ok(0)).ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int64_fields_parse_as_strings_or_numbers() {
        let m: Message = serde_json::from_str(
            r#"{"id":"a","historyId":"42","internalDate":"1700000000000","sizeEstimate":1234,"unknown":true}"#,
        )
        .unwrap();
        assert_eq!((m.history_id, m.internal_date, m.size_estimate), (42, 1_700_000_000_000, 1234));
        let m: Message = serde_json::from_str(r#"{"historyId":42,"internalDate":null,"sizeEstimate":"7"}"#).unwrap();
        assert_eq!((m.history_id, m.internal_date, m.size_estimate), (42, 0, 7));
    }

    #[test]
    fn versions_parse() {
        let v = |s: &str| Version { version: s.into() }.parse();
        assert_eq!(v("v0.29.0"), Some((0, 29, 0)));
        assert_eq!(v("0.41.2-dev"), Some((0, 41, 2)));
        assert_eq!(v("1.2"), Some((1, 2, 0)));
        assert_eq!(v("dev"), None);
    }
}

#[cfg(test)]
mod null_list_tests {
    use super::*;

    #[test]
    fn null_lists_read_as_empty() {
        let h: History = serde_json::from_str(r#"{"historyId":"42","messages":null}"#).unwrap();
        assert!(h.messages.is_empty());
        let s: MessageSearch = serde_json::from_str(r#"{"messages":null}"#).unwrap();
        assert!(s.messages.is_empty());
        let l: LabelList = serde_json::from_str(r#"{"labels":null}"#).unwrap();
        assert!(l.labels.is_empty());
    }
}
