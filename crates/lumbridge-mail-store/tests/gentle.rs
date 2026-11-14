//! Store semantics the gentle gog sync depends on: windowed full syncs, partial records, metadata that
//! arrives with a body fetch, and accounts kept after they leave the config. Made-up mail only.

mod common;
use common::*;
use lumbridge_mail_core::*;

fn a(id: &str) -> AccountId {
    AccountId::new(id)
}
fn mid(id: &str) -> MessageId {
    MessageId::new(id)
}
fn q() -> Query {
    Query { limit: 100, ..Query::default() }
}

/// Messages `(id, date)`, one thread each.
fn dated(ids: &[(&str, i64)]) -> Vec<MessageMeta> {
    ids.iter().map(|(id, date)| M { id, thread: id, date: *date, ..M::default() }.meta()).collect()
}

fn page(msgs: Vec<MessageMeta>, floor: Option<i64>, more: bool, cursor: &str) -> Delta {
    Delta { upserted: msgs, reset: true, window_floor: floor, more, cursor: Some(SyncCursor(cursor.into())), ..Delta::default() }
}

#[test]
fn a_windowed_full_sync_never_deletes_mail_older_than_its_window() {
    let mut s = store();
    let p = a("personal");
    // Years of mail already stored (an earlier full sync, or a backfill).
    put(&mut s, &dated(&[("old1", 1_000), ("old2", 2_000), ("mid", 5_000), ("gone", 8_000), ("new1", 9_000)]));
    s.reset_sync_cursor(&p).unwrap();
    // A windowed full sync over two pages that covers dates 6_000..: it reports new1 and new2, not "gone".
    let r = s.apply_delta(&p, &page(dated(&[("new2", 9_500)]), Some(6_000), true, "full-p2")).unwrap();
    assert_eq!(r.removed, 0, "nothing goes before the last page");
    let r = s.apply_delta(&p, &page(dated(&[("new1", 9_000), ("w", 6_000)]), Some(6_000), false, "hist")).unwrap();
    assert_eq!(r.removed, 1, "only the unreported message inside the window");
    assert!(s.message(&p, &mid("gone")).unwrap().is_none());
    for keep in ["old1", "old2", "mid", "new1", "new2", "w"] {
        assert!(s.message(&p, &mid(keep)).unwrap().is_some(), "{keep} must stay");
    }
}

#[test]
fn an_empty_window_deletes_nothing_but_a_full_run_still_does() {
    let mut s = store();
    let p = a("personal");
    put(&mut s, &dated(&[("x", 1_000), ("y", 2_000)]));
    s.reset_sync_cursor(&p).unwrap();
    // Nothing in the last 90 days (floor 5_000): what is stored is all older, so it stays.
    let r = s.apply_delta(&p, &page(Vec::new(), Some(5_000), false, "hist")).unwrap();
    assert_eq!(r.removed, 0);
    assert_eq!(s.list_threads(&q()).unwrap().threads.len(), 2);
    // The old meaning is unchanged: a full (unwindowed) run drops everything it did not report.
    s.reset_sync_cursor(&p).unwrap();
    let r = s.apply_delta(&p, &page(dated(&[("y", 2_000)]), None, false, "hist2")).unwrap();
    assert_eq!(r.removed, 1);
    assert!(s.message(&p, &mid("x")).unwrap().is_none());
}

#[test]
fn partial_records_update_labels_and_flags_but_keep_stored_details() {
    let mut s = store();
    let p = a("personal");
    let full = M { id: "m1", snippet: "Lunch at noon?", seen: false, ..M::default() }.meta();
    put(&mut s, std::slice::from_ref(&full));
    // A listing (older gog search): labels changed, nothing else known.
    let mut light = M { id: "m1", snippet: "", seen: true, mailboxes: &["Projects"], ..M::default() }.meta();
    light.to.clear();
    light.rfc_message_id = None;
    light.size = 0;
    light.date = 0;
    light.partial = true;
    put(&mut s, &[light]);
    let got = s.message(&p, &mid("m1")).unwrap().unwrap();
    assert!(got.flags.seen, "flags come from the partial record");
    assert_eq!(got.mailboxes, vec![MailboxId::new("Projects")]);
    assert_eq!(got.snippet, "Lunch at noon?", "an empty snippet keeps the stored one");
    assert_eq!(got.to, full.to);
    assert_eq!(got.rfc_message_id, full.rfc_message_id);
    assert_eq!((got.size, got.date), (full.size, full.date));
    // A new message from a partial record is stored as it is.
    let mut fresh = M { id: "m9", thread: "t9", snippet: "", ..M::default() }.meta();
    fresh.partial = true;
    fresh.to.clear();
    put(&mut s, &[fresh]);
    assert!(s.message(&p, &mid("m9")).unwrap().unwrap().to.is_empty());
    // And a later full record replaces it.
    put(&mut s, &[M { id: "m9", thread: "t9", snippet: "now with details", ..M::default() }.meta()]);
    let m9 = s.message(&p, &mid("m9")).unwrap().unwrap();
    assert_eq!((m9.snippet.as_str(), m9.to.len()), ("now with details", 1));
}

#[test]
fn metadata_from_a_body_fetch_keeps_the_sync_cursor() {
    let mut s = store();
    let p = a("personal");
    s.apply_delta(&p, &Delta { upserted: dated(&[("m1", 10)]), cursor: Some(SyncCursor("h7".into())), ..Delta::default() }).unwrap();
    let r = s.merge_metas(&p, &dated(&[("m1", 10), ("m2", 20)])).unwrap();
    assert_eq!(r.new_messages, vec![mid("m2")]);
    assert_eq!(s.sync_cursor(&p).unwrap(), Some(SyncCursor("h7".into())));
    assert!(s.merge_metas(&p, &[M { account: "work", ..M::default() }.meta()]).is_err());
}

#[test]
fn kept_accounts_are_hidden_until_purged_or_added_again() {
    let mut s = store();
    put(&mut s, &[M { account: "work", id: "w1", thread: "w1", ..M::default() }.meta(), M { id: "p1", thread: "p1", ..M::default() }.meta()]);
    assert_eq!(s.list_threads(&q()).unwrap().threads.len(), 2);
    assert!(s.keep_account(&a("work")).unwrap());
    assert!(!s.keep_account(&a("nobody")).unwrap());
    assert_eq!(s.kept_accounts().unwrap(), vec![a("work")]);
    assert_eq!(ids(&s.list_threads(&q()).unwrap()), vec!["personal/p1"], "kept mail is out of the unified inbox");
    assert!(s.message(&a("work"), &mid("w1")).unwrap().is_some(), "but still stored");
    // Added again: live, with its mail.
    s.upsert_account(&account("work", Persona::Work)).unwrap();
    assert!(s.kept_accounts().unwrap().is_empty());
    assert_eq!(s.list_threads(&q()).unwrap().threads.len(), 2);
    // Purged: gone.
    s.keep_account(&a("work")).unwrap();
    assert!(s.remove_account(&a("work")).unwrap());
    assert!(s.kept_accounts().unwrap().is_empty());
}
