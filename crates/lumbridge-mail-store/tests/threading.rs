//! Threads for providers without thread ids (Zoho listings): headers, then subject and people, then a
//! thread of one. Made-up mail only.

mod common;

use common::*;
use lumbridge_mail_core::*;
use lumbridge_mail_store::{SUBJECT_WINDOW_S, Store};

const DAY: i64 = 86_400;
const T0: i64 = 1_790_000_000;

/// A listing row as a threadless provider sends it: its own id as the thread, no headers, partial.
fn row(id: &str, subject: &str, from: &str, to: &str, date: i64) -> MessageMeta {
    let mut m = M { id, thread: id, subject, date, from: ("", from), ..M::default() }.meta();
    m.to = vec![Address::new(None, to)];
    m.rfc_message_id = None;
    m.partial = true;
    m
}

fn sync(s: &mut Store, mut metas: Vec<MessageMeta>) {
    s.assign_threads(&a("personal"), &mut metas).unwrap();
    s.apply_delta(&a("personal"), &Delta { upserted: metas, cursor: Some(SyncCursor("c".into())), ..Delta::default() }).unwrap();
}

fn thread_of(s: &Store, id: &str) -> String {
    s.message(&a("personal"), &MessageId::new(id)).unwrap().unwrap().thread.0
}

const ME: &str = "personal@example.com";

#[test]
fn replies_join_by_subject_and_people_and_strangers_do_not() {
    let mut s = store();
    sync(&mut s, vec![
        row("100", "Lunch on Friday", "maya@postbox.example", ME, T0),
        row("101", "RE: Lunch on Friday", ME, "maya@postbox.example", T0 + 3600),
        // Same subject, someone else entirely: not the same conversation.
        row("102", "Re: Lunch on Friday", "otto@elsewhere.example", ME, T0 + 7200),
        // A newsletter that happens to share a word: different subject.
        row("103", "Lunch deals this week", "deals@shop.example", ME, T0 + 7300),
    ]);
    assert_eq!(thread_of(&s, "100"), "100", "a first message keeps its own id as its thread");
    assert_eq!(thread_of(&s, "101"), "100");
    assert_eq!(thread_of(&s, "102"), "102");
    assert_eq!(thread_of(&s, "103"), "103");
    // The thread row counts both.
    let page = s.list_threads(&Query { accounts: Some(vec![a("personal")]), ..Query::default() }).unwrap();
    let lunch = page.threads.iter().find(|t| t.id.as_str() == "100").unwrap();
    assert_eq!(lunch.message_count, 2);
}

#[test]
fn rows_listed_again_keep_their_thread_and_late_parents_join_their_replies() {
    let mut s = store();
    // Newest first, one page at a time, like a full sync: the reply comes before the message it answers.
    sync(&mut s, vec![row("201", "Fwd: Flat viewing", "jonah@parkfamily.example", ME, T0 + DAY)]);
    sync(&mut s, vec![row("200", "Flat viewing", ME, "jonah@parkfamily.example", T0)]);
    assert_eq!(thread_of(&s, "200"), thread_of(&s, "201"));
    let t = thread_of(&s, "201");
    // The next round lists both again with their own ids as threads: nothing moves.
    sync(&mut s, vec![
        row("200", "Flat viewing", ME, "jonah@parkfamily.example", T0),
        row("201", "Fwd: Flat viewing", "jonah@parkfamily.example", ME, T0 + DAY),
    ]);
    assert_eq!((thread_of(&s, "200"), thread_of(&s, "201")), (t.clone(), t));
}

#[test]
fn the_window_and_plain_repeats_keep_threads_apart() {
    let mut s = store();
    sync(&mut s, vec![
        row("300", "Weekly report", "boss@work.example", ME, T0),
        // The same subject again, no reply prefix on either: separate reports.
        row("301", "Weekly report", "boss@work.example", ME, T0 + 7 * DAY),
        // A reply long after: a new conversation.
        row("302", "Re: Weekly report", ME, "boss@work.example", T0 + 7 * DAY + SUBJECT_WINDOW_S + DAY),
        // A reply within the window joins the closest report.
        row("303", "Re: Weekly report", ME, "boss@work.example", T0 + 7 * DAY + 3600),
    ]);
    assert_ne!(thread_of(&s, "300"), thread_of(&s, "301"));
    assert_eq!(thread_of(&s, "303"), thread_of(&s, "301"));
    assert_eq!(thread_of(&s, "302"), "302");
}

#[test]
fn headers_from_a_body_place_a_message_and_win_over_the_subject() {
    let mut s = store();
    sync(&mut s, vec![
        row("400", "Quarterly numbers", "priya@work.example", ME, T0),
        row("401", "Different words entirely", "priya@work.example", ME, T0 + 600),
    ]);
    assert_eq!(thread_of(&s, "401"), "401");
    // The bodies come: 400 is <q@work.example>, 401 answers it.
    let h = |k: &str, v: &str| (k.to_owned(), v.to_owned());
    assert!(s.learn_thread_headers(&a("personal"), &MessageId::new("400"), &[h("Message-ID", "<q@work.example>")]).unwrap().is_empty());
    let changed = s
        .learn_thread_headers(&a("personal"), &MessageId::new("401"), &[
            h("message-id", "<r@work.example>"),
            h("In-Reply-To", "<q@work.example>"),
            h("References", "<q@work.example>"),
        ])
        .unwrap();
    assert_eq!(thread_of(&s, "401"), "400");
    assert!(changed.iter().any(|t| t.as_str() == "400"), "{changed:?}");
    let m = s.message(&a("personal"), &MessageId::new("401")).unwrap().unwrap();
    assert_eq!((m.rfc_message_id.as_deref(), m.in_reply_to.as_deref()), (Some("r@work.example"), Some("q@work.example")));
    // A later listing (no headers, own id as thread) does not undo it.
    sync(&mut s, vec![row("401", "Different words entirely", "priya@work.example", ME, T0 + 600)]);
    assert_eq!(thread_of(&s, "401"), "400");
    // A new row whose headers are known joins by them, whatever its subject.
    let mut fresh = row("402", "Totally new subject", "priya@work.example", ME, T0 + 900);
    fresh.in_reply_to = Some("<r@work.example>".into());
    sync(&mut s, vec![fresh]);
    assert_eq!(thread_of(&s, "402"), "400");
}

#[test]
fn a_reply_whose_body_came_first_joins_when_the_parent_says_who_it_is() {
    let mut s = store();
    sync(&mut s, vec![
        row("600", "Contract draft", "lena@law.example", ME, T0),
        row("601", "Comments inline", "lena@law.example", ME, T0 + 600),
        row("602", "Re: Contract draft", "lena@law.example", ME, T0 + 900),
    ]);
    assert_eq!(thread_of(&s, "602"), "600");
    let h = |k: &str, v: &str| (k.to_owned(), v.to_owned());
    // 601's body first: it answers <c@law.example>, which no stored message is known as yet.
    let none = s.learn_thread_headers(&a("personal"), &MessageId::new("601"), &[h("In-Reply-To", "<c@law.example>")]).unwrap();
    assert!(none.is_empty() && thread_of(&s, "601") == "601");
    // Then 600's: it is <c@law.example>, so 601 comes over; 600's own thread stays.
    let changed = s.learn_thread_headers(&a("personal"), &MessageId::new("600"), &[h("Message-Id", "<c@law.example>")]).unwrap();
    assert_eq!((thread_of(&s, "600"), thread_of(&s, "601"), thread_of(&s, "602")), ("600".into(), "600".into(), "600".into()));
    assert!(changed.iter().any(|t| t.as_str() == "601"), "{changed:?}");
    // Learning the same again changes nothing.
    assert!(s.learn_thread_headers(&a("personal"), &MessageId::new("600"), &[h("Message-Id", "<c@law.example>")]).unwrap().is_empty());
}

#[test]
fn a_thread_id_from_the_provider_is_kept() {
    let mut s = store();
    let mut named = row("700", "Re: Lunch on Friday", "maya@postbox.example", ME, T0 + 60);
    named.thread = ThreadId::new("zoho-thread-9");
    sync(&mut s, vec![row("699", "Lunch on Friday", "maya@postbox.example", ME, T0), named]);
    assert_eq!(thread_of(&s, "700"), "zoho-thread-9");
}

#[test]
fn rethread_groups_mail_stored_one_message_per_thread() {
    let mut s = store();
    // Stored before this change: every message its own thread (no assign_threads).
    put(&mut s, &[
        row("500", "Dinner Saturday?", "maya@postbox.example", ME, T0),
        row("501", "Re: Dinner Saturday?", ME, "maya@postbox.example", T0 + 3600),
        row("502", "Re: Dinner Saturday?", "maya@postbox.example", ME, T0 + 7200),
        row("503", "Unrelated", "maya@postbox.example", ME, T0 + 9000),
    ]);
    assert_eq!(thread_of(&s, "501"), "501");
    let changed = s.rethread(&a("personal")).unwrap();
    assert_eq!((thread_of(&s, "500"), thread_of(&s, "501"), thread_of(&s, "502"), thread_of(&s, "503")),
               ("500".into(), "500".into(), "500".into(), "503".into()));
    assert!(changed.iter().any(|t| t.as_str() == "501") && changed.iter().any(|t| t.as_str() == "500"), "{changed:?}");
    // The emptied threads are gone; the inbox counts one thread for the three.
    let page = s.list_threads(&Query { accounts: Some(vec![a("personal")]), ..Query::default() }).unwrap();
    assert_eq!(page.threads.len(), 2);
    assert_eq!(page.threads.iter().find(|t| t.id.as_str() == "500").unwrap().message_count, 3);
    // Running it again changes nothing.
    assert!(s.rethread(&a("personal")).unwrap().is_empty());
    // Other accounts are untouched.
    put(&mut s, &[M { account: "work", id: "w1", thread: "w1", subject: "Re: Dinner Saturday?", ..M::default() }.meta()]);
    s.rethread(&a("personal")).unwrap();
    assert_eq!(s.message(&a("work"), &MessageId::new("w1")).unwrap().unwrap().thread.as_str(), "w1");
}
