//! The later core contract in the store: reset runs, "mailboxes unchanged", provider draft ids,
//! category counts and HTML-only bodies in the search index.

mod common;

use common::*;
use lumbridge_mail_core::rpc::{Caller, CategoryCount};
use lumbridge_mail_core::*;

fn mid(s: &str) -> MessageId {
    MessageId::new(s)
}

fn page(ids: &[&str], reset: bool, more: bool, cursor: Option<&str>) -> Delta {
    Delta {
        upserted: ids.iter().map(|id| M { id, thread: id, ..M::default() }.meta()).collect(),
        reset,
        more,
        cursor: cursor.map(|c| SyncCursor(c.into())),
        ..Delta::default()
    }
}

#[test]
fn a_reset_run_drops_what_it_did_not_see() {
    let mut s = store();
    let p = a("personal");
    s.apply_delta(&p, &page(&["m1", "m2", "m3"], true, false, Some("h1"))).unwrap();
    assert_eq!(s.list_threads(&q()).unwrap().threads.len(), 3);

    // Gmail's history expired: a full sync over two pages that no longer has m2.
    let r = s.apply_delta(&p, &page(&["m1"], true, true, Some("full-p2"))).unwrap();
    assert_eq!(r.removed, 0, "nothing is dropped before the last page");
    let r = s.apply_delta(&p, &page(&["m3", "m4"], true, false, Some("h9"))).unwrap();
    assert_eq!(r.removed, 1);
    assert!(s.message(&p, &mid("m2")).unwrap().is_none());
    assert!(r.changed_threads.contains(&ThreadId::new("m2")));
    let mut left: Vec<String> = s.list_threads(&q()).unwrap().threads.into_iter().map(|t| t.id.0).collect();
    left.sort();
    assert_eq!(left, ["m1", "m3", "m4"]);

    // Ordinary deltas never drop anything.
    let r = s.apply_delta(&p, &page(&["m5"], false, false, Some("h10"))).unwrap();
    assert_eq!(r.removed, 0);
    assert_eq!(s.list_threads(&q()).unwrap().threads.len(), 4);
    // Other accounts are untouched by a reset.
    put(&mut s, &[M { account: "work", id: "w1", thread: "w1", ..M::default() }.meta()]);
    s.apply_delta(&p, &page(&["m1"], true, false, Some("h11"))).unwrap();
    assert!(s.message(&a("work"), &mid("w1")).unwrap().is_some());
    assert_eq!(s.list_threads(&Query { accounts: Some(vec![p.clone()]), ..q() }).unwrap().threads.len(), 1);
}

#[test]
fn starting_over_from_no_cursor_starts_a_new_run() {
    let mut s = store();
    let p = a("personal");
    s.apply_delta(&p, &page(&["m1", "m2"], true, true, Some("full-p2"))).unwrap();
    // The engine dropped the cursor and called sync(None) again (ResyncNeeded): the half-done run is void.
    s.reset_sync_cursor(&p).unwrap();
    s.apply_delta(&p, &page(&["m2"], true, false, Some("h1"))).unwrap();
    assert!(s.message(&p, &mid("m1")).unwrap().is_none());
    assert!(s.message(&p, &mid("m2")).unwrap().is_some());
}

#[test]
fn mailboxes_none_is_unchanged_and_empty_is_none() {
    let mut s = store();
    let p = a("personal");
    s.apply_delta(&p, &Delta { mailboxes: None, ..Delta::default() }).unwrap();
    assert_eq!(s.mailboxes(Some(&p)).unwrap().len(), 5);
    s.apply_delta(&p, &Delta { mailboxes: Some(vec![]), ..Delta::default() }).unwrap();
    assert!(s.mailboxes(Some(&p)).unwrap().is_empty());
}

#[test]
fn the_providers_draft_id_is_kept_and_handed_back() {
    let mut s = store();
    let p = a("personal");
    let d = Draft { local_id: "d1".into(), subject: "Plans".into(), ..Draft::default() };
    let first = s.enqueue(&p, Action::SaveDraft { draft: d.clone() }, Caller::Agent).unwrap().id;
    // A second save is queued before the first one reaches the server.
    let second = s.enqueue(&p, Action::SaveDraft { draft: Draft { text: "v2".into(), ..d.clone() } }, Caller::Agent).unwrap().id;
    let job = s.next_action(i64::MAX / 2).unwrap().unwrap();
    assert_eq!(job.id, first);
    s.action_done_with(first, &ApplyOutcome::with_id("r-123")).unwrap();
    assert_eq!(s.draft(&p, "d1").unwrap().unwrap().draft.server_id.as_deref(), Some("r-123"));
    let job = s.next_action(i64::MAX / 2).unwrap().unwrap();
    assert_eq!(job.id, second);
    match &job.action {
        Action::SaveDraft { draft } => assert_eq!(draft.server_id.as_deref(), Some("r-123")),
        other => panic!("{other:?}"),
    }
    s.action_done(second).unwrap();
    // Later actions pick it up at enqueue time.
    let del = s.enqueue(&p, Action::DeleteDraft { local_id: "d1".into(), server_id: None }, Caller::Agent).unwrap().id;
    let q = s.actions(Some(&p)).unwrap().into_iter().find(|q| q.id == del).unwrap();
    assert_eq!(q.action, Action::DeleteDraft { local_id: "d1".into(), server_id: Some("r-123".into()) });
}

#[test]
fn counts_per_category_respect_the_anon_wall() {
    let mut s = store();
    put(
        &mut s,
        &[
            M { id: "n1", thread: "n1", ..M::default() }.meta(),
            M { id: "n2", thread: "n2", seen: true, ..M::default() }.meta(),
            M { id: "p1", thread: "p1", mailboxes: &["Archive"], ..M::default() }.meta(),
            M { account: "anon", id: "x1", thread: "x1", ..M::default() }.meta(),
        ],
    );
    let t = |c| Triage { category: c, priority: 1, summary: "s".into(), needs_reply: false, due: None, by: "rules".into(), at: i64::MAX, suspicious: false };
    s.put_triage(&a("personal"), &ThreadId::new("n1"), &t(Category::Newsletter)).unwrap();
    s.put_triage(&a("personal"), &ThreadId::new("n2"), &t(Category::Newsletter)).unwrap();
    s.put_triage(&a("personal"), &ThreadId::new("p1"), &t(Category::Personal)).unwrap();
    s.put_triage(&a("anon"), &ThreadId::new("x1"), &t(Category::Newsletter)).unwrap();
    let all = s.category_counts(None, None).unwrap();
    assert_eq!(
        all,
        vec![
            CategoryCount { category: Category::Personal, threads: 1, unread: 1 },
            CategoryCount { category: Category::Newsletter, threads: 2, unread: 1 },
        ]
    );
    let inbox = s.category_counts(None, Some(MailboxRole::Inbox)).unwrap();
    assert_eq!(inbox, vec![CategoryCount { category: Category::Newsletter, threads: 2, unread: 1 }]);
    let anon = s.category_counts(Some(&[a("anon")]), None).unwrap();
    assert_eq!(anon, vec![CategoryCount { category: Category::Newsletter, threads: 1, unread: 1 }]);
}

#[test]
fn html_only_bodies_are_searchable_by_their_text() {
    let mut s = store();
    put(&mut s, &[M { id: "h1", thread: "h1", subject: "Newsletter", snippet: "", ..M::default() }.meta()]);
    let body = Body {
        html: Some("<p>The <b>marmalade</b> festival</p><div style=\"display:none\">zanzibarhidden</div><script>quokkascript()</script>".into()),
        ..Body::default()
    };
    s.put_body(&a("personal"), &mid("h1"), &body).unwrap();
    let hits = |w: &str| s.list_threads(&Query { text: Some(w.into()), ..q() }).unwrap().threads.len();
    assert_eq!(hits("marmalade"), 1);
    assert_eq!(hits("zanzibarhidden"), 0, "hidden text is not indexed");
    assert_eq!(hits("quokkascript"), 0, "scripts are not indexed");
}
