mod common;

use common::*;
use lumbridge_mail_core::*;
use lumbridge_mail_store::{ActionState, Caller, DraftState, FailOutcome, RetryPolicy, Store};

fn mid(s: &str) -> MessageId {
    MessageId::new(s)
}

fn unread(s: &Store, thread: &str) -> u32 {
    s.thread_summary(&a("personal"), &ThreadId::new(thread)).unwrap().unwrap().unread_count
}

fn inbox_ids(s: &Store) -> Vec<String> {
    ids(&s.list_threads(&Query { role: Some(MailboxRole::Inbox), ..q() }).unwrap())
}

fn later() -> i64 {
    // Far enough ahead that every backoff has passed.
    i64::MAX / 4
}

fn seeded() -> Store {
    let mut s = store();
    put(
        &mut s,
        &[
            M { id: "m1", thread: "t1", date: 1, ..M::default() }.meta(),
            M { id: "m2", thread: "t2", date: 2, ..M::default() }.meta(),
            M { account: "work", id: "w1", thread: "wt", date: 3, ..M::default() }.meta(),
        ],
    );
    s
}

#[test]
fn enqueue_applies_locally_at_once() {
    let mut s = seeded();
    let e = s.enqueue(&a("personal"), Action::SetSeen { ids: vec![mid("m1")], seen: true }, Caller::Agent).unwrap();
    assert_eq!(e.changed_threads, vec![ThreadId::new("t1")]);
    assert_eq!(unread(&s, "t1"), 0);
    let inbox = s.mailboxes(Some(&a("personal"))).unwrap().into_iter().find(|b| b.id.0 == "INBOX").unwrap();
    assert_eq!(inbox.unread, 1);
    assert_eq!(s.queued_count(&a("personal")).unwrap(), 1);

    s.enqueue(&a("personal"), Action::SetFlagged { ids: vec![mid("m2")], flagged: true }, Caller::Person).unwrap();
    assert!(s.thread_summary(&a("personal"), &ThreadId::new("t2")).unwrap().unwrap().flagged);

    s.enqueue(&a("personal"), Action::Archive { ids: vec![mid("m2")] }, Caller::Person).unwrap();
    assert_eq!(inbox_ids(&s), ["work/wt", "personal/t1"]);
    assert_eq!(s.message(&a("personal"), &mid("m2")).unwrap().unwrap().mailboxes, vec![MailboxId::new("Archive")]);

    s.enqueue(&a("personal"), Action::Trash { ids: vec![mid("m1")] }, Caller::Person).unwrap();
    assert_eq!(s.message(&a("personal"), &mid("m1")).unwrap().unwrap().mailboxes, vec![MailboxId::new("Trash")]);

    s.enqueue(&a("personal"), Action::Move { ids: vec![mid("m1")], from: Some(MailboxId::new("Trash")), to: MailboxId::new("Projects") }, Caller::Person).unwrap();
    assert_eq!(s.message(&a("personal"), &mid("m1")).unwrap().unwrap().mailboxes, vec![MailboxId::new("Projects")]);

    let bad = Action::Move { ids: vec![mid("m1")], from: None, to: MailboxId::new("Nowhere") };
    assert!(matches!(s.enqueue(&a("personal"), bad, Caller::Person), Err(Error::NotFound(_))));
    // Messages we have not synced yet are still queued for the server.
    s.enqueue(&a("personal"), Action::SetSeen { ids: vec![mid("not-synced")], seen: true }, Caller::Person).unwrap();
    assert_eq!(s.queued_count(&a("personal")).unwrap(), 6);
}

#[test]
fn only_a_person_can_queue_a_send() {
    let mut s = seeded();
    let d = Draft { local_id: "d1".into(), subject: "Hi".into(), ..Draft::default() };
    let err = s.enqueue(&a("personal"), Action::Send { draft: d.clone() }, Caller::Agent).unwrap_err();
    assert_eq!(err, Error::Refused(lumbridge_mail_core::safety::SEND_NEEDS_A_PERSON.into()));
    assert_eq!(s.queued_count(&a("personal")).unwrap(), 0);
    // An agent may still save a draft.
    s.enqueue(&a("personal"), Action::SaveDraft { draft: d.clone() }, Caller::Agent).unwrap();
    assert_eq!(s.draft(&a("personal"), "d1").unwrap().unwrap().state, DraftState::Editing);

    let e = s.enqueue(&a("personal"), Action::Send { draft: d }, Caller::Person).unwrap();
    assert_eq!(s.draft(&a("personal"), "d1").unwrap().unwrap().state, DraftState::Sending);
    let first = s.next_action(later()).unwrap().unwrap();
    assert!(matches!(first.action, Action::SaveDraft { .. }));
    s.action_done(first.id).unwrap();
    let send = s.next_action(later()).unwrap().unwrap();
    assert_eq!(send.id, e.id);
    s.action_done(send.id).unwrap();
    assert!(s.draft(&a("personal"), "d1").unwrap().is_none());
}

#[test]
fn actions_go_out_in_order_one_at_a_time_per_account() {
    let mut s = seeded();
    let p1 = s.enqueue(&a("personal"), Action::SetSeen { ids: vec![mid("m1")], seen: true }, Caller::Person).unwrap().id;
    let w1 = s.enqueue(&a("work"), Action::SetSeen { ids: vec![mid("w1")], seen: true }, Caller::Person).unwrap().id;
    let p2 = s.enqueue(&a("personal"), Action::SetSeen { ids: vec![mid("m1")], seen: false }, Caller::Person).unwrap().id;

    let first = s.next_action(later()).unwrap().unwrap();
    assert_eq!((first.id, first.state), (p1, ActionState::Running));
    // personal is busy with p1, so work's head is next; then nothing until p1 is done.
    assert_eq!(s.next_action(later()).unwrap().unwrap().id, w1);
    assert!(s.next_action(later()).unwrap().is_none());
    assert!(s.next_action_for(&a("personal"), later()).unwrap().is_none());
    s.action_done(p1).unwrap();
    assert_eq!(s.next_action(later()).unwrap().unwrap().id, p2);
    s.action_done(p2).unwrap();
    s.action_done(w1).unwrap();
    assert!(s.next_action(later()).unwrap().is_none());
    assert_eq!(s.next_action_due(None).unwrap(), None);
    assert!(matches!(s.action_done(p1), Err(Error::NotFound(_))));
}

#[test]
fn offline_failures_back_off_and_then_give_up() {
    let mut s = seeded();
    s.set_retry_policy(RetryPolicy { first_delay_s: 10, max_delay_s: 25, max_attempts: 3 });
    let id = s.enqueue(&a("personal"), Action::SetSeen { ids: vec![mid("m1")], seen: true }, Caller::Person).unwrap().id;
    let behind = s.enqueue(&a("personal"), Action::SetFlagged { ids: vec![mid("m2")], flagged: true }, Caller::Person).unwrap().id;
    let now = 1_000;
    let offline = Error::Offline("no route".into());

    let q = s.next_action(later()).unwrap().unwrap();
    assert_eq!(s.action_failed(q.id, &offline, now).unwrap(), FailOutcome::Retry { at: now + 10, attempts: 1 });
    // While the head waits out its backoff, the action behind it waits too.
    assert!(s.next_action(now + 5).unwrap().is_none());
    assert_eq!(s.next_action_due(Some(&a("personal"))).unwrap(), Some(now + 10));
    let q = s.next_action(now + 10).unwrap().unwrap();
    assert_eq!((q.id, q.attempts, q.last_error.as_deref()), (id, 1, Some("offline: no route")));
    assert_eq!(s.action_failed(id, &offline, now + 10).unwrap(), FailOutcome::Retry { at: now + 30, attempts: 2 });
    let q = s.next_action(now + 30).unwrap().unwrap();
    assert_eq!(q.id, id);
    // Third failure: out of attempts. The local change is rolled back; the flag behind it stays.
    let out = s.action_failed(id, &offline, now + 30).unwrap();
    assert_eq!(out, FailOutcome::GaveUp { changed_threads: vec![ThreadId::new("t1")] });
    assert_eq!(unread(&s, "t1"), 1);
    assert!(s.thread_summary(&a("personal"), &ThreadId::new("t2")).unwrap().unwrap().flagged);
    let all = s.actions(Some(&a("personal"))).unwrap();
    assert_eq!(all[0].state, ActionState::Failed);
    assert_eq!(s.queued_count(&a("personal")).unwrap(), 1);
    // The failed action no longer blocks the queue.
    assert_eq!(s.next_action(later()).unwrap().unwrap().id, behind);
    assert!(s.clear_failed(id).unwrap());
    assert!(!s.clear_failed(id).unwrap());
}

#[test]
fn a_refusal_fails_at_once_and_later_actions_on_the_same_message_survive() {
    let mut s = seeded();
    let archive = s.enqueue(&a("personal"), Action::Archive { ids: vec![mid("m1")] }, Caller::Person).unwrap().id;
    s.enqueue(&a("personal"), Action::SetSeen { ids: vec![mid("m1")], seen: true }, Caller::Person).unwrap();
    assert_eq!(inbox_ids(&s), ["work/wt", "personal/t2"]);
    let q = s.next_action(later()).unwrap().unwrap();
    assert_eq!(q.id, archive);
    let out = s.action_failed(archive, &Error::Auth("expired".into()), 0).unwrap();
    assert!(matches!(out, FailOutcome::GaveUp { .. }));
    // Back in the inbox, but still read: the SetSeen behind it is pending and stays applied.
    assert_eq!(inbox_ids(&s), ["work/wt", "personal/t2", "personal/t1"]);
    assert_eq!(unread(&s, "t1"), 0);
}

#[test]
fn a_sync_with_old_server_state_does_not_undo_a_pending_action() {
    let mut s = seeded();
    s.enqueue(&a("personal"), Action::SetSeen { ids: vec![mid("m1")], seen: true }, Caller::Person).unwrap();
    s.enqueue(&a("personal"), Action::Archive { ids: vec![mid("m1")] }, Caller::Person).unwrap();
    // The server has not heard yet: still unread, still in the inbox.
    put(&mut s, &[M { id: "m1", thread: "t1", date: 1, ..M::default() }.meta()]);
    assert_eq!(unread(&s, "t1"), 0);
    assert!(!inbox_ids(&s).contains(&"personal/t1".to_owned()));
    // Once the actions are done, the server's word is final.
    while let Some(q) = s.next_action(later()).unwrap() {
        s.action_done(q.id).unwrap();
    }
    put(&mut s, &[M { id: "m1", thread: "t1", date: 1, ..M::default() }.meta()]);
    assert_eq!(unread(&s, "t1"), 1);
}

#[test]
fn interrupted_actions_come_back_marked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mail.db");
    let id;
    {
        let mut s = Store::open(&path).unwrap();
        s.upsert_account(&account("personal", Persona::Personal)).unwrap();
        let d = Draft { local_id: "d1".into(), ..Draft::default() };
        id = s.enqueue(&a("personal"), Action::Send { draft: d }, Caller::Person).unwrap().id;
        assert_eq!(s.next_action(later()).unwrap().unwrap().id, id);
        // The daemon dies here, mid-send.
    }
    let mut s = Store::open(&path).unwrap();
    assert!(s.next_action(later()).unwrap().is_none(), "still marked running");
    let back = s.recover_interrupted().unwrap();
    assert_eq!(back.len(), 1);
    assert!(back[0].interrupted);
    assert_eq!(back[0].state, ActionState::Pending);
    let q = s.next_action(later()).unwrap().unwrap();
    assert!(q.interrupted && q.id == id);
}

#[test]
fn a_pending_action_can_be_cancelled_and_rolled_back() {
    let mut s = seeded();
    let d = Draft { local_id: "d1".into(), subject: "Oops".into(), ..Draft::default() };
    s.save_draft(&a("personal"), &d).unwrap();
    let send = s.enqueue(&a("personal"), Action::Send { draft: d }, Caller::Person).unwrap().id;
    s.cancel_action(send).unwrap();
    assert_eq!(s.draft(&a("personal"), "d1").unwrap().unwrap().state, DraftState::Editing);
    assert_eq!(s.queued_count(&a("personal")).unwrap(), 0);

    let trash = s.enqueue(&a("personal"), Action::Trash { ids: vec![mid("m2")] }, Caller::Person).unwrap().id;
    assert_eq!(s.cancel_action(trash).unwrap(), vec![ThreadId::new("t2")]);
    assert_eq!(s.message(&a("personal"), &mid("m2")).unwrap().unwrap().mailboxes, vec![MailboxId::new("INBOX")]);

    let running = s.enqueue(&a("personal"), Action::DeleteDraft { local_id: "d1".into(), server_id: None }, Caller::Person).unwrap().id;
    assert!(s.draft(&a("personal"), "d1").unwrap().is_none());
    s.next_action(later()).unwrap();
    assert!(matches!(s.cancel_action(running), Err(Error::Invalid(_))));
}

#[test]
fn backoff_is_exponential_and_capped() {
    let p = RetryPolicy::default();
    assert_eq!((p.delay(1), p.delay(2), p.delay(3)), (5, 10, 20));
    assert_eq!(p.delay(40), 30 * 60);
}
