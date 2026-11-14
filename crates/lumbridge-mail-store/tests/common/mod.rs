//! Made-up mail for the tests. Every address is on a reserved example domain.
#![allow(dead_code)]

use lumbridge_mail_core::*;
use lumbridge_mail_store::Store;

pub fn account(id: &str, persona: Persona) -> Account {
    Account {
        id: AccountId::new(id),
        persona,
        label: id.to_owned(),
        email: format!("{id}@example.com"),
        name: "Test User".into(),
        provider: ProviderKind::Demo,
        color: None,
        ai: AiPolicy::LocalOnly,
        readonly: false,
    }
}

pub fn mailbox(account: &str, id: &str, role: MailboxRole) -> Mailbox {
    Mailbox { id: MailboxId::new(id), account: AccountId::new(account), name: id.to_owned(), role, unread: 0, total: 0 }
}

pub fn standard_mailboxes(account: &str) -> Vec<Mailbox> {
    vec![
        mailbox(account, "INBOX", MailboxRole::Inbox),
        mailbox(account, "Sent", MailboxRole::Sent),
        mailbox(account, "Archive", MailboxRole::Archive),
        mailbox(account, "Trash", MailboxRole::Trash),
        mailbox(account, "Projects", MailboxRole::Custom),
    ]
}

pub struct M<'a> {
    pub account: &'a str,
    pub id: &'a str,
    pub thread: &'a str,
    pub from: (&'a str, &'a str),
    pub subject: &'a str,
    pub snippet: &'a str,
    pub date: i64,
    pub mailboxes: &'a [&'a str],
    pub seen: bool,
}

impl Default for M<'_> {
    fn default() -> Self {
        M {
            account: "personal",
            id: "m1",
            thread: "t1",
            from: ("Ada Lovelace", "ada@example.com"),
            subject: "Hello",
            snippet: "Just saying hello",
            date: 1_700_000_000,
            mailboxes: &["INBOX"],
            seen: false,
        }
    }
}

impl M<'_> {
    pub fn meta(&self) -> MessageMeta {
        MessageMeta {
            id: MessageId::new(self.id),
            account: AccountId::new(self.account),
            thread: ThreadId::new(self.thread),
            mailboxes: self.mailboxes.iter().map(|m| MailboxId::new(*m)).collect(),
            from: vec![Address::new(Some(self.from.0), self.from.1)],
            to: vec![Address::new(Some("Test User"), &format!("{}@example.com", self.account))],
            cc: vec![],
            bcc: vec![],
            subject: self.subject.into(),
            snippet: self.snippet.into(),
            date: self.date,
            flags: Flags { seen: self.seen, ..Flags::default() },
            has_attachments: false,
            size: 1000,
            rfc_message_id: Some(format!("{}@mail.example.com", self.id)),
            in_reply_to: None,
            references: vec![],
            partial: false,
        }
    }
}

/// A store with a personal, a work and an anon account, each with the standard mailboxes.
pub fn store() -> Store {
    let mut s = Store::open_in_memory().unwrap();
    for (id, p) in [("personal", Persona::Personal), ("work", Persona::Work), ("anon", Persona::Anon)] {
        s.upsert_account(&account(id, p)).unwrap();
        s.apply_delta(&AccountId::new(id), &Delta { mailboxes: Some(standard_mailboxes(id)), ..Delta::default() }).unwrap();
    }
    s
}

pub fn put(s: &mut Store, msgs: &[MessageMeta]) {
    let mut by_account: std::collections::BTreeMap<AccountId, Vec<MessageMeta>> = Default::default();
    for m in msgs {
        by_account.entry(m.account.clone()).or_default().push(m.clone());
    }
    for (a, upserted) in by_account {
        s.apply_delta(&a, &Delta { upserted, cursor: Some(SyncCursor("c".into())), ..Delta::default() }).unwrap();
    }
}

pub fn ids(page: &lumbridge_mail_store::ThreadPage) -> Vec<String> {
    page.threads.iter().map(|t| format!("{}/{}", t.account, t.id)).collect()
}

pub fn q() -> Query {
    Query::default()
}

pub fn text(t: &str) -> Query {
    Query { text: Some(t.into()), ..Query::default() }
}

pub fn a(id: &str) -> AccountId {
    AccountId::new(id)
}
