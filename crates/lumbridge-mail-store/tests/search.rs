mod common;

use common::*;
use lumbridge_mail_core::*;
use lumbridge_mail_store::{Caller, Store, parse_search};

fn day(s: &str) -> i64 {
    parse_search(&format!("after:{s}")).after.unwrap()
}

fn seeded() -> Store {
    let mut s = store();
    let mut invoice = M {
        id: "inv",
        thread: "t-invoice",
        from: ("Billing Team", "billing@shop.example.com"),
        subject: "Your invoice for March",
        snippet: "Invoice 1042 is attached, total due on receipt",
        date: day("2024-03-05") + 3600,
        seen: true,
        ..M::default()
    }
    .meta();
    invoice.has_attachments = true;
    let mut lunch = M {
        id: "lunch",
        thread: "t-lunch",
        from: ("Grace Hopper", "grace@example.org"),
        subject: "Lunch on Friday?",
        snippet: "The new restaurant near the harbour looks good",
        date: day("2024-04-10"),
        ..M::default()
    }
    .meta();
    lunch.cc = vec![Address::new(Some("Bob Builder"), "bob@example.net")];
    let report = M {
        id: "report",
        thread: "t-report",
        from: ("Ada Lovelace", "ada@example.com"),
        subject: "Weekly report: engines",
        snippet: "Analytical engine progress this week",
        date: day("2024-05-01"),
        mailboxes: &["Projects"],
        seen: true,
        ..M::default()
    }
    .meta();
    let sent = M {
        id: "sent",
        thread: "t-lunch",
        from: ("Test User", "personal@example.com"),
        subject: "Re: Lunch on Friday?",
        snippet: "Sounds great, see you at noon",
        date: day("2024-04-11"),
        mailboxes: &["Sent"],
        seen: true,
        ..M::default()
    }
    .meta();
    let secret = M {
        account: "anon",
        id: "anon1",
        thread: "t-anon",
        from: ("Someone", "someone@example.net"),
        subject: "Invoice from the other life",
        snippet: "invoice restaurant report",
        date: day("2024-06-01"),
        ..M::default()
    }
    .meta();
    put(&mut s, &[invoice, lunch, report, sent, secret]);
    s
}

fn find(s: &Store, t: &str) -> Vec<String> {
    s.list_threads(&text(t)).unwrap().threads.into_iter().map(|t| t.id.0).collect()
}

#[test]
fn words_match_any_field_as_prefixes() {
    let s = seeded();
    assert_eq!(find(&s, "invoice"), ["t-invoice"]);
    assert_eq!(find(&s, "INVOI"), ["t-invoice"]);
    assert_eq!(find(&s, "harbour restaurant"), ["t-lunch"]);
    assert_eq!(find(&s, "grace"), ["t-lunch"]);
    assert_eq!(find(&s, "\"total due\""), ["t-invoice"]);
    assert!(find(&s, "\"due total\"").is_empty());
    assert!(find(&s, "zeppelin").is_empty());
}

#[test]
fn typos_are_forgiven() {
    let s = seeded();
    assert_eq!(find(&s, "invocie"), ["t-invoice"]);
    assert_eq!(find(&s, "resturant"), ["t-lunch"]);
    assert_eq!(find(&s, "analyticl"), ["t-report"]);
    // Short words must be exact (or a prefix): no guessing on three letters.
    assert!(find(&s, "bpb").is_empty());
}

#[test]
fn address_and_subject_operators() {
    let s = seeded();
    assert_eq!(find(&s, "from:grace"), ["t-lunch"]);
    assert_eq!(find(&s, "from:billing@shop.example.com"), ["t-invoice"]);
    assert_eq!(find(&s, "from:\"Ada Lovelace\""), ["t-report"]);
    assert_eq!(find(&s, "to:bob"), ["t-lunch"]);
    assert_eq!(find(&s, "cc:bob@example.net"), ["t-lunch"]);
    assert_eq!(find(&s, "subject:\"weekly report\""), ["t-report"]);
    assert!(find(&s, "subject:harbour").is_empty());
    assert!(find(&s, "from:bob").is_empty());
}

#[test]
fn flag_date_and_mailbox_operators() {
    let s = seeded();
    assert_eq!(find(&s, "has:attachment"), ["t-invoice"]);
    assert_eq!(find(&s, "is:unread"), ["t-lunch"]);
    assert_eq!(find(&s, "is:read"), ["t-report", "t-lunch", "t-invoice"]);
    assert_eq!(find(&s, "before:2024-04-01"), ["t-invoice"]);
    assert_eq!(find(&s, "after:2024-04-11"), ["t-report", "t-lunch"]);
    assert_eq!(find(&s, "after:2024-04-01 before:2024-04-30"), ["t-lunch"]);
    assert_eq!(find(&s, "in:projects"), ["t-report"]);
    assert_eq!(find(&s, "in:sent"), ["t-lunch"]);
    assert_eq!(find(&s, "in:inbox is:read"), ["t-invoice"]);
    assert_eq!(find(&s, "in:sent noon"), ["t-lunch"]);
    assert!(find(&s, "in:sent harbour").is_empty(), "both parts must match one message");
    assert!(find(&s, "is:starred").is_empty());
}

#[test]
fn starred_follows_local_changes() {
    let mut s = seeded();
    s.enqueue(&a("personal"), Action::SetFlagged { ids: vec![MessageId::new("report")], flagged: true }, Caller::Person).unwrap();
    assert_eq!(find(&s, "is:starred"), ["t-report"]);
    assert_eq!(find(&s, "in:starred"), ["t-report"]);
}

#[test]
fn search_respects_the_query_filters_and_the_anon_wall() {
    let s = seeded();
    // The anon account has the same words, but is not searched unless named.
    assert_eq!(find(&s, "invoice"), ["t-invoice"]);
    let named = Query { accounts: Some(vec![a("anon")]), text: Some("invoice".into()), ..q() };
    assert_eq!(ids(&s.list_threads(&named).unwrap()), ["anon/t-anon"]);
    let inbox = Query { role: Some(MailboxRole::Inbox), text: Some("lunch".into()), ..q() };
    assert_eq!(ids(&s.list_threads(&inbox).unwrap()), ["personal/t-lunch"]);
    let projects = Query { mailbox: Some(MailboxId::new("Projects")), text: Some("lunch".into()), ..q() };
    assert!(s.list_threads(&projects).unwrap().threads.is_empty());
    let unread = Query { unread_only: true, text: Some("from:ada".into()), ..q() };
    assert!(s.list_threads(&unread).unwrap().threads.is_empty());
}

#[test]
fn hostile_search_text_is_just_text() {
    let s = seeded();
    for t in ["\"", "*", "AND OR NOT", "NEAR(a b)", "from:\"", "subject:\"\"", "a\"b", "''; DROP TABLE messages; --", "{sender}: x", "^invoice", "-", "in:'"] {
        let r = s.list_threads(&text(t));
        assert!(r.is_ok(), "{t:?}: {r:?}");
    }
    assert_eq!(find(&s, "invoice"), ["t-invoice"]);
}

#[test]
fn negation_or_and_relative_dates() {
    let s = seeded();
    // What agent tools saw return nothing: unread mail not from someone.
    assert_eq!(find(&s, "-from:billing is:unread"), ["t-lunch"]);
    assert_eq!(find(&s, "is:unread -from:grace"), Vec::<String>::new());
    assert_eq!(find(&s, "-from:ada is:read"), ["t-lunch", "t-invoice"], "the read reply in t-lunch is not from ada");
    assert_eq!(find(&s, "-invoice"), ["t-report", "t-lunch"]);
    assert_eq!(find(&s, "-has:attachment -in:sent is:read"), ["t-report"]);
    assert_eq!(find(&s, "lunch -harbour"), ["t-lunch"], "the reply says lunch without harbour");
    assert!(find(&s, "harbour -lunch").is_empty());
    // OR joins the parts next to it; the rest still has to match.
    assert_eq!(find(&s, "from:ada OR from:grace"), ["t-report", "t-lunch"]);
    assert_eq!(find(&s, "from:ada OR from:grace is:unread"), ["t-lunch"]);
    assert_eq!(find(&s, "invoice OR harbour"), ["t-lunch", "t-invoice"]);
    assert_eq!(find(&s, "(from:ada OR has:attachment) -in:projects"), ["t-invoice"]);
    assert_eq!(find(&s, "from:zeppelin OR -is:read"), ["t-lunch"]);
    // Relative dates count back from now; the made-up mail is from 2024.
    assert!(find(&s, "newer_than:2d").is_empty());
    assert_eq!(find(&s, "older_than:1d"), ["t-report", "t-lunch", "t-invoice"]);
}

#[test]
fn categories_are_triage_or_gmail_tabs() {
    let mut s = seeded();
    let mut boxes = standard_mailboxes("personal");
    boxes.push(mailbox("personal", "CATEGORY_PROMOTIONS", MailboxRole::Custom));
    let promo = M { id: "promo", thread: "t-promo", from: ("Shop", "deals@shop.example.com"), subject: "Weekend deals", date: day("2024-05-02"), mailboxes: &["INBOX", "CATEGORY_PROMOTIONS"], ..M::default() }.meta();
    s.apply_delta(&a("personal"), &Delta { mailboxes: Some(boxes), upserted: vec![promo], ..Delta::default() }).unwrap();
    let triage = |c: Category| Triage { category: c, priority: 50, summary: "Made-up".into(), needs_reply: false, due: None, by: "rules".into(), at: 1, suspicious: false };
    s.put_triage(&a("personal"), &ThreadId::new("t-lunch"), &triage(Category::Personal)).unwrap();
    s.put_triage(&a("personal"), &ThreadId::new("t-report"), &triage(Category::Newsletter)).unwrap();
    assert_eq!(find(&s, "category:personal"), ["t-lunch"]);
    assert_eq!(find(&s, "category:newsletters"), ["t-report"], "plural is fine");
    assert_eq!(find(&s, "category:promotions"), ["t-promo"], "Gmail's tab, by its label");
    assert_eq!(find(&s, "category:personal OR category:newsletter"), ["t-report", "t-lunch"]);
    assert_eq!(find(&s, "-category:personal is:read"), ["t-report", "t-invoice"]);
    assert!(find(&s, "category:nonsense").is_empty());
}

#[test]
fn list_filters_since_needs_reply_and_priority() {
    let mut s = seeded();
    let t = |needs_reply: bool, priority: u8| Triage { category: Category::Personal, priority, summary: "Made-up".into(), needs_reply, due: None, by: "fake".into(), at: 1, suspicious: false };
    s.put_triage(&a("personal"), &ThreadId::new("t-lunch"), &t(true, 80)).unwrap();
    s.put_triage(&a("personal"), &ThreadId::new("t-report"), &t(false, 30)).unwrap();
    let list = |q: Query| ids(&s.list_threads(&q).unwrap());
    assert_eq!(list(Query { since: Some(day("2024-04-11")), ..q() }), ["personal/t-report", "personal/t-lunch"]);
    assert_eq!(list(Query { since: Some(day("2024-04-11")), role: Some(MailboxRole::Inbox), ..q() }), ["personal/t-lunch"], "a thread's newest mail counts");
    assert!(list(Query { since: Some(day("2024-05-02")), role: Some(MailboxRole::Inbox), ..q() }).is_empty());
    assert_eq!(list(Query { needs_reply: true, ..q() }), ["personal/t-lunch"]);
    assert_eq!(list(Query { needs_reply: true, role: Some(MailboxRole::Inbox), ..q() }), ["personal/t-lunch"]);
    assert_eq!(list(Query { min_priority: Some(50), ..q() }), ["personal/t-lunch"]);
    assert_eq!(list(Query { min_priority: Some(10), ..q() }), ["personal/t-report", "personal/t-lunch"], "untriaged t-invoice never matches");
    assert_eq!(list(Query { min_priority: Some(10), text: Some("report".into()), ..q() }), ["personal/t-report"]);
}
