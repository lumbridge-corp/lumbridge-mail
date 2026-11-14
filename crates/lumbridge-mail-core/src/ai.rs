//! What the AI layer writes back onto mail. The models and prompts live in `lumbridge-mail-ai`;
//! these are only the results, so the store can keep them and the app can show them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// A person wrote to you and it matters.
    Important,
    /// A person wrote to you.
    Personal,
    /// Receipts, shipping, statements, calendar, account notices.
    Transactional,
    Newsletter,
    Promotion,
    /// Automated notifications (CI, GitHub, monitoring).
    Notification,
    /// Security codes, password resets, bank, government. Handled by rules, never by a model.
    Sensitive,
    Spam,
    Other,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::Important,
        Category::Personal,
        Category::Transactional,
        Category::Newsletter,
        Category::Promotion,
        Category::Notification,
        Category::Sensitive,
        Category::Spam,
        Category::Other,
    ];

    /// The wire name (`newsletter`, ...), the same string serde uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Important => "important",
            Category::Personal => "personal",
            Category::Transactional => "transactional",
            Category::Newsletter => "newsletter",
            Category::Promotion => "promotion",
            Category::Notification => "notification",
            Category::Sensitive => "sensitive",
            Category::Spam => "spam",
            Category::Other => "other",
        }
    }

    /// The reverse of [`Category::as_str`] (case-insensitive).
    pub fn parse(s: &str) -> Option<Category> {
        let s = s.trim().to_ascii_lowercase();
        Category::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Triage {
    pub category: Category,
    /// 0 (ignore) to 100 (now).
    pub priority: u8,
    /// One line, in our words. For `Sensitive` mail this is a fixed phrase like "A security code from X".
    pub summary: String,
    pub needs_reply: bool,
    /// A date the mail asks something by, if any (Unix seconds).
    pub due: Option<i64>,
    /// Which model or rule produced this, e.g. `rules`, `local/my-model`.
    pub by: String,
    pub at: i64,
    /// The mail seems to try to instruct an assistant (prompt injection, hidden text). The app shows a
    /// warning; the priority is capped. The summary does not repeat this.
    #[serde(default)]
    pub suspicious: bool,
}

/// Who an action item is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// Asked of the user.
    Me,
    /// Someone else promised it.
    Them,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    pub owner: Owner,
    /// Unix seconds (end of the day when only a date was given).
    pub due: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateMention {
    pub what: String,
    /// Unix seconds.
    pub at: i64,
    /// Only a date was given (then `at` is 23:59 that day in the user's zone).
    pub all_day: bool,
}

/// Action items and dates found in one thread (`ai.extract`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extracted {
    pub items: Vec<ActionItem>,
    pub dates: Vec<DateMention>,
    /// Which model produced this.
    pub by: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_have_names() {
        for c in Category::ALL {
            assert_eq!(serde_json::to_string(&c).unwrap(), format!("\"{}\"", c.as_str()));
            assert_eq!(Category::parse(c.as_str()), Some(c));
        }
        assert_eq!(Category::parse("Newsletter"), Some(Category::Newsletter));
        assert_eq!(Category::parse("x"), None);
    }

    #[test]
    fn old_triage_rows_read_as_not_suspicious() {
        let t: Triage = serde_json::from_str(
            r#"{"category":"other","priority":1,"summary":"s","needs_reply":false,"due":null,"by":"rules","at":0}"#,
        )
        .unwrap();
        assert!(!t.suspicious);
    }
}
