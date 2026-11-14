//! Ids we hand out. All are pure functions of what the server says, so they are the same on every machine
//! and after every resync.
//!
//! - Message: `<folder>:<uidvalidity>:<uid>`, the folder as the server names it. Unique per account by
//!   IMAP's own rules (a UID is never reused within one UIDVALIDITY of one folder).
//! - Thread: `t:` + a hash of the thread's root Message-ID (first `References` entry, else
//!   `In-Reply-To`, else the message's own Message-ID); replies without either fall back to the
//!   normalised subject.
//! - Message-ID of mail we write: derived from the account and the draft's `local_id`, so a draft saved
//!   twice or a send retried is recognisable on the server.

use lumbridge_mail_core::{MailboxId, MessageId, ThreadId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageKey {
    pub folder: String,
    pub validity: u32,
    pub uid: u32,
}

impl MessageKey {
    pub fn id(&self) -> MessageId {
        message_id(&self.folder, self.validity, self.uid)
    }
    pub fn mailbox(&self) -> MailboxId {
        MailboxId::new(self.folder.clone())
    }
}

pub fn message_id(folder: &str, validity: u32, uid: u32) -> MessageId {
    MessageId::new(format!("{folder}:{validity}:{uid}"))
}

/// Reads an id back. Folder names may contain `:`, so the numbers are taken from the right.
pub fn parse_message_id(id: &MessageId) -> Option<MessageKey> {
    let mut it = id.as_str().rsplitn(3, ':');
    let uid = it.next()?.parse().ok()?;
    let validity = it.next()?.parse().ok()?;
    let folder = it.next()?.to_owned();
    if folder.is_empty() || uid == 0 {
        return None;
    }
    Some(MessageKey { folder, validity, uid })
}

/// FNV-1a, 64 bit. Our own so that ids never change with a std or dependency upgrade.
pub fn fnv64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn clean_msgid(s: &str) -> String {
    lumbridge_mail_core::threading::clean_message_id(s)
}

/// Subject without reply/forward prefixes and list tags (shared with the store: `core::threading`).
pub use lumbridge_mail_core::threading::normalise_subject;
use lumbridge_mail_core::threading::has_reply_prefix;

/// The thread a message belongs to. `fallback` is used when nothing else identifies it (our own id).
pub fn thread_id(
    rfc_message_id: Option<&str>,
    in_reply_to: Option<&str>,
    references: &[String],
    subject: &str,
    fallback: &str,
) -> ThreadId {
    let root = references
        .iter()
        .map(|r| clean_msgid(r))
        .find(|r| !r.is_empty())
        .or_else(|| in_reply_to.map(clean_msgid).filter(|r| !r.is_empty()));
    let key = match root {
        Some(r) => format!("id:{r}"),
        None => {
            let norm = normalise_subject(subject);
            if has_reply_prefix(subject) && !norm.is_empty() {
                format!("subject:{norm}")
            } else {
                match rfc_message_id.map(clean_msgid).filter(|m| !m.is_empty()) {
                    Some(m) => format!("id:{m}"),
                    None => format!("local:{fallback}"),
                }
            }
        }
    };
    ThreadId::new(format!("t:{:016x}", fnv64(key.as_bytes())))
}

/// The Message-ID (without angle brackets) for a draft and the mail sent from it.
pub fn outgoing_message_id(account: &str, local_id: &str, domain: &str) -> String {
    let a = fnv64(format!("{account}\0{local_id}").as_bytes());
    let b = fnv64(format!("{local_id}\0{account}\0lumbridge").as_bytes());
    let domain = if domain.is_empty() || !domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        "lumbridge.invalid"
    } else {
        domain
    };
    format!("lbm.{a:016x}{b:016x}@{domain}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_round_trip_even_with_colons() {
        let id = message_id("Work:2026/Q4", 42, 7);
        assert_eq!(id.as_str(), "Work:2026/Q4:42:7");
        let k = parse_message_id(&id).unwrap();
        assert_eq!((k.folder.as_str(), k.validity, k.uid), ("Work:2026/Q4", 42, 7));
        assert!(parse_message_id(&MessageId::new("nonsense")).is_none());
        assert!(parse_message_id(&MessageId::new("INBOX:1:0")).is_none());
    }

    #[test]
    fn replies_join_the_root_thread() {
        let root = thread_id(Some("<a@x>"), None, &[], "Lunch", "f1");
        let reply = thread_id(Some("<b@x>"), Some("<a@x>"), &["<a@x>".into()], "Re: Lunch", "f2");
        let reply2 = thread_id(Some("<c@x>"), Some("<b@x>"), &["<a@x>".into(), "<b@x>".into()], "Re: Re: Lunch", "f3");
        assert_eq!(root, reply);
        assert_eq!(root, reply2);
        // Only In-Reply-To.
        assert_eq!(thread_id(Some("<d@x>"), Some("<a@x>"), &[], "Re: Lunch", "f4"), root);
        // Unrelated message with the same subject and no reply prefix is its own thread.
        assert_ne!(thread_id(Some("<z@x>"), None, &[], "Lunch", "f5"), root);
        // Reply without headers: subject fallback, shared by others like it.
        let s1 = thread_id(Some("<e@x>"), None, &[], "RE: [team]  Lunch plans", "f6");
        let s2 = thread_id(Some("<f@x>"), None, &[], "Fwd: Re: lunch   plans", "f7");
        assert_eq!(s1, s2);
        assert!(root.as_str().starts_with("t:"));
    }

    #[test]
    fn outgoing_ids_are_stable_and_distinct() {
        let a = outgoing_message_id("acct", "draft-1", "example.com");
        assert_eq!(a, outgoing_message_id("acct", "draft-1", "example.com"));
        assert_ne!(a, outgoing_message_id("acct", "draft-2", "example.com"));
        assert_ne!(a, outgoing_message_id("other", "draft-1", "example.com"));
        assert!(a.ends_with("@example.com"));
        assert!(outgoing_message_id("a", "b", "bad domain>").ends_with("@lumbridge.invalid"));
    }
}
