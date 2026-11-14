mod common;

use common::*;
use lumbridge_mail_core::*;
use lumbridge_mail_store::{DraftState, SCHEMA_VERSION, Store};

#[test]
fn opens_a_file_in_wal_mode_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("mail.db");
    {
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        s.upsert_account(&account("personal", Persona::Personal)).unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.accounts().unwrap().len(), 1);
    let c = rusqlite::Connection::open(&path).unwrap();
    let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn a_database_from_a_newer_build_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mail.db");
    drop(Store::open(&path).unwrap());
    rusqlite::Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 999").unwrap();
    assert!(matches!(Store::open(&path), Err(Error::Invalid(_))));
}

#[test]
fn the_schema_has_nowhere_to_put_a_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mail.db");
    drop(Store::open(&path).unwrap());
    let c = rusqlite::Connection::open(&path).unwrap();
    let mut st = c
        .prepare("SELECT m.name, lower(p.name) FROM sqlite_master m JOIN pragma_table_info(m.name) p WHERE m.type='table'")
        .unwrap();
    let columns: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert!(columns.len() > 50, "{columns:?}");
    for (table, column) in columns {
        for word in ["password", "token", "secret", "oauth", "credential", "key"] {
            assert!(!column.contains(word), "{table}.{column} looks like a place for a secret");
        }
    }
}

#[test]
fn accounts_can_be_added_updated_and_removed_with_their_mail() {
    let mut s = store();
    assert_eq!(s.accounts().unwrap().iter().map(|a| a.id.0.clone()).collect::<Vec<_>>(), ["personal", "work", "anon"]);
    let mut p = account("personal", Persona::Personal);
    p.label = "Home".into();
    s.upsert_account(&p).unwrap();
    assert_eq!(s.account(&a("personal")).unwrap().unwrap().label, "Home");
    assert_eq!(s.accounts().unwrap().len(), 3);

    put(&mut s, &[M::default().meta()]);
    s.save_draft(&a("personal"), &Draft { local_id: "d1".into(), ..Draft::default() }).unwrap();
    assert!(s.remove_account(&a("personal")).unwrap());
    assert!(!s.remove_account(&a("personal")).unwrap());
    assert!(s.account(&a("personal")).unwrap().is_none());
    assert!(s.list_threads(&q()).unwrap().threads.is_empty());
    assert!(s.list_threads(&text("hello")).unwrap().threads.is_empty());
    assert!(matches!(s.mailboxes(Some(&a("personal"))), Err(Error::NotFound(_))));
    assert!(s.upsert_account(&account("", Persona::Work)).is_err());
}

#[test]
fn a_delta_builds_threads_and_counts() {
    let mut s = store();
    let r = s
        .apply_delta(
            &a("personal"),
            &Delta {
                upserted: vec![
                    M { id: "m1", subject: "Plans", snippet: "first", date: 100, seen: true, ..M::default() }.meta(),
                    M { id: "m2", from: ("Grace", "grace@example.org"), subject: "Re: Plans", snippet: "second", date: 200, ..M::default() }.meta(),
                    M { id: "m3", from: ("Ada Lovelace", "ADA@example.com"), subject: "Re: Plans", snippet: "third", date: 300, mailboxes: &["INBOX", "Projects"], ..M::default() }.meta(),
                ],
                cursor: Some(SyncCursor("h42".into())),
                ..Delta::default()
            },
        )
        .unwrap();
    assert_eq!(r.new_messages.len(), 3);
    assert_eq!(r.changed_threads, vec![ThreadId::new("t1")]);
    assert_eq!(s.sync_cursor(&a("personal")).unwrap(), Some(SyncCursor("h42".into())));
    assert!(s.last_sync(&a("personal")).unwrap().is_some());

    let t = s.thread_summary(&a("personal"), &ThreadId::new("t1")).unwrap().unwrap();
    assert_eq!(t.subject, "Plans");
    assert_eq!(t.snippet, "third");
    assert_eq!(t.message_count, 3);
    assert_eq!(t.unread_count, 2);
    assert_eq!(t.date, 300);
    assert_eq!(t.participants.iter().map(|p| p.email.as_str()).collect::<Vec<_>>(), ["ada@example.com", "grace@example.org"]);
    assert_eq!(t.mailboxes, vec![MailboxId::new("INBOX"), MailboxId::new("Projects")]);

    let boxes = s.mailboxes(Some(&a("personal"))).unwrap();
    let inbox = boxes.iter().find(|b| b.id.0 == "INBOX").unwrap();
    assert_eq!((inbox.total, inbox.unread), (3, 2));
    let projects = boxes.iter().find(|b| b.id.0 == "Projects").unwrap();
    assert_eq!((projects.total, projects.unread), (1, 1));

    // Same messages again with one flag change: nothing new, counts follow.
    let r = s
        .apply_delta(
            &a("personal"),
            &Delta { upserted: vec![M { id: "m2", date: 200, seen: true, ..M::default() }.meta()], cursor: None, ..Delta::default() },
        )
        .unwrap();
    assert!(r.new_messages.is_empty());
    assert_eq!(s.sync_cursor(&a("personal")).unwrap(), None);
    assert_eq!(s.thread_summary(&a("personal"), &ThreadId::new("t1")).unwrap().unwrap().unread_count, 1);

    let msgs = s.thread(&a("personal"), &ThreadId::new("t1")).unwrap();
    assert_eq!(msgs.iter().map(|m| m.id.0.as_str()).collect::<Vec<_>>(), ["m1", "m2", "m3"]);
    assert_eq!(msgs[2].mailboxes.len(), 2);
    assert_eq!(s.message(&a("personal"), &MessageId::new("m3")).unwrap().unwrap(), msgs[2]);
}

#[test]
fn messages_can_move_between_threads_and_be_removed() {
    let mut s = store();
    put(&mut s, &[M { id: "m1", thread: "t1", ..M::default() }.meta(), M { id: "m2", thread: "t1", date: 2, ..M::default() }.meta()]);
    put(&mut s, &[M { id: "m2", thread: "t2", date: 2, ..M::default() }.meta()]);
    assert_eq!(s.thread(&a("personal"), &ThreadId::new("t1")).unwrap().len(), 1);
    assert_eq!(s.thread(&a("personal"), &ThreadId::new("t2")).unwrap().len(), 1);

    let r = s.apply_delta(&a("personal"), &Delta { removed: vec![MessageId::new("m2"), MessageId::new("nope")], ..Delta::default() }).unwrap();
    assert_eq!(r.removed, 1);
    assert_eq!(r.changed_threads, vec![ThreadId::new("t2")]);
    assert!(s.thread_summary(&a("personal"), &ThreadId::new("t2")).unwrap().is_none());
    assert!(s.message(&a("personal"), &MessageId::new("m2")).unwrap().is_none());
    assert_eq!(ids(&s.list_threads(&q()).unwrap()), ["personal/t1"]);
}

#[test]
fn a_message_for_another_account_is_refused() {
    let mut s = store();
    let m = M { account: "work", ..M::default() }.meta();
    assert!(matches!(
        s.apply_delta(&a("personal"), &Delta { upserted: vec![m], ..Delta::default() }),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn the_mailbox_list_is_replaced_and_roles_follow() {
    let mut s = store();
    put(&mut s, &[M { id: "m1", thread: "t1", mailboxes: &["Projects"], ..M::default() }.meta()]);
    assert!(s.list_threads(&Query { role: Some(MailboxRole::Inbox), ..q() }).unwrap().threads.is_empty());
    // The provider now says Projects is the inbox, and Trash is gone.
    let mut list = standard_mailboxes("personal");
    list.retain(|b| b.id.0 != "Trash" && b.id.0 != "INBOX");
    list.iter_mut().find(|b| b.id.0 == "Projects").unwrap().role = MailboxRole::Inbox;
    s.apply_delta(&a("personal"), &Delta { mailboxes: Some(list), ..Delta::default() }).unwrap();
    assert_eq!(ids(&s.list_threads(&Query { role: Some(MailboxRole::Inbox), ..q() }).unwrap()), ["personal/t1"]);
    let names: Vec<String> = s.mailboxes(Some(&a("personal"))).unwrap().into_iter().map(|b| b.id.0).collect();
    assert_eq!(names, ["Sent", "Archive", "Projects"]);
}

#[test]
fn an_unknown_mailbox_on_a_message_gets_a_placeholder() {
    let mut s = store();
    put(&mut s, &[M { id: "m1", mailboxes: &["STARRED", "Label_7"], ..M::default() }.meta()]);
    let boxes = s.mailboxes(Some(&a("personal"))).unwrap();
    assert_eq!(boxes.iter().find(|b| b.id.0 == "STARRED").unwrap().role, MailboxRole::Starred);
    assert_eq!(boxes.iter().find(|b| b.id.0 == "Label_7").unwrap().role, MailboxRole::Custom);
}

#[test]
fn the_unified_inbox_keeps_anon_out_unless_named() {
    let mut s = store();
    put(
        &mut s,
        &[
            M { account: "personal", id: "p1", thread: "pt", date: 10, ..M::default() }.meta(),
            M { account: "work", id: "w1", thread: "wt", date: 20, ..M::default() }.meta(),
            M { account: "anon", id: "x1", thread: "xt", date: 30, ..M::default() }.meta(),
            M { account: "work", id: "w2", thread: "wsent", date: 40, mailboxes: &["Sent"], ..M::default() }.meta(),
        ],
    );
    let inbox = Query { role: Some(MailboxRole::Inbox), ..q() };
    assert_eq!(ids(&s.list_threads(&inbox).unwrap()), ["work/wt", "personal/pt"]);
    assert_eq!(ids(&s.list_threads(&q()).unwrap()), ["work/wsent", "work/wt", "personal/pt"]);
    assert_eq!(ids(&s.list_threads(&Query { mailbox: Some(MailboxId::new("INBOX")), ..q() }).unwrap()), ["work/wt", "personal/pt"]);
    assert!(s.list_threads(&text("hello")).unwrap().threads.iter().all(|t| t.account.0 != "anon"));

    let named = Query { accounts: Some(vec![a("anon")]), role: Some(MailboxRole::Inbox), ..q() };
    assert_eq!(ids(&s.list_threads(&named).unwrap()), ["anon/xt"]);
    let both = Query { accounts: Some(vec![a("anon"), a("personal")]), text: Some("hello".into()), ..q() };
    assert_eq!(ids(&s.list_threads(&both).unwrap()), ["anon/xt", "personal/pt"]);
    let unknown = Query { accounts: Some(vec![a("nobody")]), ..q() };
    assert!(s.list_threads(&unknown).unwrap().threads.is_empty());
}

#[test]
fn pages_walk_every_thread_once_newest_first() {
    let mut s = store();
    let msgs: Vec<MessageMeta> = (0..120)
        .map(|i| {
            let (id, t) = (format!("m{i}"), format!("t{i}"));
            // Some threads share a date, to test the tie-break.
            let mut m = M { date: 1000 + (i / 3) as i64, ..M::default() }.meta();
            m.id = MessageId(id);
            m.thread = ThreadId(t);
            m
        })
        .collect();
    put(&mut s, &msgs);
    for base in [q(), Query { role: Some(MailboxRole::Inbox), ..q() }, Query { mailbox: Some(MailboxId::new("INBOX")), ..q() }, text("hello")] {
        let mut seen = Vec::new();
        let mut page = None;
        loop {
            let p = s.list_threads(&Query { limit: 50, page: page.clone(), ..base.clone() }).unwrap();
            assert!(p.threads.len() <= 50);
            seen.extend(p.threads.iter().map(|t| (t.date, t.id.0.clone())));
            page = p.next;
            if page.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), 120, "{base:?}");
        let mut sorted = seen.clone();
        sorted.sort_by_key(|x| std::cmp::Reverse(x.0));
        assert_eq!(seen.iter().map(|x| x.0).collect::<Vec<_>>(), sorted.iter().map(|x| x.0).collect::<Vec<_>>());
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 120);
    }
    assert!(matches!(s.list_threads(&Query { page: Some("garbage".into()), ..q() }), Err(Error::Invalid(_))));
    assert_eq!(s.list_threads(&q()).unwrap().threads.len(), 50);
}

#[test]
fn unread_only_and_category_filter_threads() {
    let mut s = store();
    put(
        &mut s,
        &[
            M { id: "m1", thread: "read", seen: true, date: 3, ..M::default() }.meta(),
            M { id: "m2", thread: "unread", date: 2, ..M::default() }.meta(),
            M { id: "m3", thread: "news", date: 1, seen: true, ..M::default() }.meta(),
        ],
    );
    let inbox = Query { role: Some(MailboxRole::Inbox), unread_only: true, ..q() };
    assert_eq!(ids(&s.list_threads(&inbox).unwrap()), ["personal/unread"]);
    assert_eq!(ids(&s.list_threads(&Query { unread_only: true, ..q() }).unwrap()), ["personal/unread"]);

    let triage = Triage {
        category: ai::Category::Newsletter,
        priority: 10,
        summary: "A weekly digest".into(),
        needs_reply: false,
        due: None,
        by: "rules".into(),
        at: 10,
        suspicious: false,
    };
    assert_eq!(s.threads_needing_triage(10).unwrap().len(), 3);
    s.put_triage(&a("personal"), &ThreadId::new("news"), &triage).unwrap();
    assert_eq!(s.triage(&a("personal"), &ThreadId::new("news")).unwrap(), Some(triage.clone()));
    assert_eq!(s.threads_needing_triage(10).unwrap().len(), 2);
    let news = Query { category: Some(ai::Category::Newsletter), role: Some(MailboxRole::Inbox), ..q() };
    let page = s.list_threads(&news).unwrap();
    assert_eq!(ids(&page), ["personal/news"]);
    assert_eq!(page.threads[0].triage.as_ref().unwrap().summary, "A weekly digest");
    assert_eq!(ids(&s.list_threads(&Query { category: Some(ai::Category::Newsletter), text: Some("hello".into()), ..q() }).unwrap()), ["personal/news"]);
    assert!(matches!(s.put_triage(&a("personal"), &ThreadId::new("nope"), &triage), Err(Error::NotFound(_))));

    // New mail after the triage puts the thread back on the AI's list.
    put(&mut s, &[M { id: "m4", thread: "news", date: 50, ..M::default() }.meta()]);
    assert!(s.threads_needing_triage(10).unwrap().iter().any(|t| t.thread.0 == "news"));
}

#[test]
fn threads_are_found_by_references() {
    let mut s = store();
    put(&mut s, &[M { id: "m1", thread: "t1", ..M::default() }.meta()]);
    let refs = vec!["<unknown@x.example>".to_owned(), "<m1@mail.example.com>".to_owned()];
    assert_eq!(s.thread_for_references(&a("personal"), &refs).unwrap(), Some(ThreadId::new("t1")));
    assert_eq!(s.thread_for_references(&a("work"), &refs).unwrap(), None);
}

#[test]
fn bodies_and_attachments_are_cached_and_searchable() {
    let mut s = store();
    put(&mut s, &[M { id: "m1", ..M::default() }.meta()]);
    let body = Body {
        text: Some("The quarterly giraffe census is attached.".into()),
        html: Some("<p>The quarterly <b>giraffe</b> census</p>".into()),
        attachments: vec![Attachment {
            id: AttachmentId::new("a1"),
            filename: "census-results.pdf".into(),
            mime: "application/pdf".into(),
            size: 2048,
            content_id: None,
        }],
        headers: vec![("List-Unsubscribe".into(), "<mailto:u@example.com>".into())],
    };
    assert!(s.body(&a("personal"), &MessageId::new("m1")).unwrap().is_none());
    assert!(s.list_threads(&text("giraffe")).unwrap().threads.is_empty());
    s.put_body(&a("personal"), &MessageId::new("m1"), &body).unwrap();
    assert_eq!(s.body(&a("personal"), &MessageId::new("m1")).unwrap(), Some(body.clone()));
    assert_eq!(ids(&s.list_threads(&text("giraffe")).unwrap()), ["personal/t1"]);
    assert_eq!(ids(&s.list_threads(&text("filename:census")).unwrap()), ["personal/t1"]);

    let (m, att) = (MessageId::new("m1"), AttachmentId::new("a1"));
    assert_eq!(s.attachment_path(&a("personal"), &m, &att).unwrap(), None);
    s.set_attachment_path(&a("personal"), &m, &att, "/cache/a1.pdf").unwrap();
    assert_eq!(s.attachment_path(&a("personal"), &m, &att).unwrap().as_deref(), Some("/cache/a1.pdf"));
    assert!(matches!(s.set_attachment_path(&a("personal"), &m, &AttachmentId::new("zz"), "/x"), Err(Error::NotFound(_))));
    // Re-caching keeps the saved path.
    s.put_body(&a("personal"), &m, &body).unwrap();
    assert_eq!(s.attachment_path(&a("personal"), &m, &att).unwrap().as_deref(), Some("/cache/a1.pdf"));
    assert_eq!(s.attachments(&a("personal"), &m).unwrap().len(), 1);

    // A later header change re-indexes from the store, body included.
    put(&mut s, &[M { id: "m1", subject: "Census", ..M::default() }.meta()]);
    assert_eq!(ids(&s.list_threads(&text("giraffe")).unwrap()), ["personal/t1"]);

    assert_eq!(s.evict_bodies(i64::MAX).unwrap(), 1);
    assert!(s.body(&a("personal"), &m).unwrap().is_none());
    assert!(matches!(s.put_body(&a("personal"), &MessageId::new("zz"), &body), Err(Error::NotFound(_))));
}

#[test]
fn drafts_are_saved_listed_and_deleted_locally() {
    let mut s = store();
    let d = Draft { local_id: "d1".into(), subject: "Lunch?".into(), to: vec![Address::new(None, "bob@example.net")], ..Draft::default() };
    s.save_draft(&a("personal"), &d).unwrap();
    s.save_draft(&a("personal"), &Draft { local_id: "d2".into(), ..Draft::default() }).unwrap();
    let got = s.draft(&a("personal"), "d1").unwrap().unwrap();
    assert_eq!((got.draft, got.state), (d, DraftState::Editing));
    assert_eq!(s.drafts(&a("personal")).unwrap().len(), 2);
    assert!(s.drafts(&a("work")).unwrap().is_empty());
    assert!(s.delete_draft(&a("personal"), "d1").unwrap());
    assert!(!s.delete_draft(&a("personal"), "d1").unwrap());
    assert!(s.save_draft(&a("personal"), &Draft::default()).is_err());
}

#[test]
fn a_search_with_many_matches_pages_like_a_small_one() {
    let mut s = store();
    let msgs: Vec<MessageMeta> = (0..5_200)
        .map(|i| {
            let mut m = M { date: 10_000 + i as i64, seen: i % 2 == 0, ..M::default() }.meta();
            m.id = MessageId(format!("m{i}"));
            m.thread = ThreadId(format!("t{i}"));
            m
        })
        .collect();
    put(&mut s, &msgs);
    // "hello" matches all 5 200 (walk by date); "is:read hello" matches 2 600 (fetch and sort).
    for (t, want) in [("hello", 5_200), ("hello is:read", 2_600)] {
        let mut page = None;
        let mut seen = std::collections::HashSet::new();
        let mut last_date = i64::MAX;
        loop {
            let p = s.list_threads(&Query { text: Some(t.into()), limit: 500, page, ..q() }).unwrap();
            for th in &p.threads {
                assert!(th.date <= last_date);
                last_date = th.date;
                assert!(seen.insert(th.id.0.clone()));
            }
            page = p.next;
            if page.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), want, "{t}");
    }
}

#[test]
fn category_views_can_be_limited_to_a_mailbox_or_role() {
    let mut s = store();
    put(
        &mut s,
        &[
            M { id: "m1", thread: "in", date: 2, ..M::default() }.meta(),
            M { id: "m2", thread: "arch", date: 1, mailboxes: &["Archive"], ..M::default() }.meta(),
        ],
    );
    let t = Triage { category: ai::Category::Promotion, priority: 1, summary: String::new(), needs_reply: false, due: None, by: "rules".into(), at: 5, suspicious: false };
    s.put_triage(&a("personal"), &ThreadId::new("in"), &t).unwrap();
    s.put_triage(&a("personal"), &ThreadId::new("arch"), &t).unwrap();
    let cat = Some(ai::Category::Promotion);
    assert_eq!(ids(&s.list_threads(&Query { category: cat, ..q() }).unwrap()), ["personal/in", "personal/arch"]);
    assert_eq!(ids(&s.list_threads(&Query { category: cat, role: Some(MailboxRole::Inbox), ..q() }).unwrap()), ["personal/in"]);
    assert_eq!(ids(&s.list_threads(&Query { category: cat, mailbox: Some(MailboxId::new("Archive")), ..q() }).unwrap()), ["personal/arch"]);
    assert_eq!(ids(&s.list_threads(&Query { category: cat, limit: 1, page: Some(s.list_threads(&Query { category: cat, limit: 1, ..q() }).unwrap().next.unwrap()), ..q() }).unwrap()), ["personal/arch"]);
    assert!(s.list_threads(&Query { category: Some(ai::Category::Spam), ..q() }).unwrap().threads.is_empty());
}
