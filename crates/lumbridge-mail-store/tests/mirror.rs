//! The Postgres mirror's outbox: what the triggers record, when, and that it survives a restart.
mod common;

use common::*;
use lumbridge_mail_core::*;
use lumbridge_mail_store::{OutboxItem, OutboxKind, Store};

fn entries(s: &mut Store) -> Vec<(OutboxKind, String, String)> {
    s.mirror_take(10_000)
        .unwrap()
        .into_iter()
        .map(|i| (i.kind, i.account.0, i.item))
        .collect()
}

fn drain(s: &mut Store) {
    let all = s.mirror_take(10_000).unwrap();
    s.mirror_done(&all).unwrap();
}

#[test]
fn nothing_is_recorded_while_the_mirror_is_off() {
    let mut s = store();
    assert!(!s.mirror_enabled().unwrap());
    put(&mut s, &[M::default().meta()]);
    assert!(entries(&mut s).is_empty());
    assert!(
        s.mirror_backfill(None).is_err(),
        "backfill is refused while off"
    );
}

#[test]
fn changes_are_recorded_once_per_item_in_order() {
    let mut s = store();
    assert!(!s.set_mirror_enabled(true).unwrap());
    put(
        &mut s,
        &[
            M::default().meta(),
            M {
                id: "m2",
                thread: "t2",
                ..M::default()
            }
            .meta(),
        ],
    );
    let e = entries(&mut s);
    assert_eq!(
        e,
        vec![
            (OutboxKind::Message, "personal".into(), "m1".into()),
            (OutboxKind::Message, "personal".into(), "m2".into()),
        ]
    );
    drain(&mut s);
    assert!(entries(&mut s).is_empty());

    // The same message listed again unchanged: nothing to send.
    put(&mut s, &[M::default().meta()]);
    assert!(entries(&mut s).is_empty(), "{:?}", entries(&mut s));

    // A flag change and an archive (membership only) are both recorded.
    put(
        &mut s,
        &[M {
            seen: true,
            ..M::default()
        }
        .meta()],
    );
    assert_eq!(
        entries(&mut s),
        vec![(OutboxKind::Message, "personal".into(), "m1".into())]
    );
    drain(&mut s);
    put(
        &mut s,
        &[M {
            seen: true,
            mailboxes: &["Archive"],
            ..M::default()
        }
        .meta()],
    );
    assert_eq!(
        entries(&mut s),
        vec![(OutboxKind::Message, "personal".into(), "m1".into())]
    );
    drain(&mut s);

    // Body, triage, removal.
    let body = Body {
        text: Some("Hello there".into()),
        ..Body::default()
    };
    s.put_body(&a("personal"), &MessageId::new("m1"), &body)
        .unwrap();
    assert_eq!(
        entries(&mut s),
        vec![(OutboxKind::Body, "personal".into(), "m1".into())]
    );
    drain(&mut s);
    let triage = Triage {
        category: Category::Personal,
        priority: 10,
        summary: "A hello".into(),
        needs_reply: false,
        due: None,
        by: "rules".into(),
        at: 1,
        suspicious: false,
    };
    s.put_triage(&a("personal"), &ThreadId::new("t1"), &triage)
        .unwrap();
    assert_eq!(
        entries(&mut s),
        vec![(OutboxKind::Triage, "personal".into(), "t1".into())]
    );
    drain(&mut s);
    s.apply_delta(
        &a("personal"),
        &Delta {
            removed: vec![MessageId::new("m1")],
            ..Delta::default()
        },
    )
    .unwrap();
    let e = entries(&mut s);
    assert!(
        e.contains(&(OutboxKind::Message, "personal".into(), "m1".into())),
        "{e:?}"
    );
    assert!(
        e.contains(&(OutboxKind::Triage, "personal".into(), "t1".into())),
        "the emptied thread's triage goes: {e:?}"
    );
}

#[test]
fn anon_accounts_record_nothing_but_their_account_row() {
    let mut s = store();
    s.set_mirror_enabled(true).unwrap();
    put(
        &mut s,
        &[M {
            account: "anon",
            ..M::default()
        }
        .meta()],
    );
    s.put_body(
        &a("anon"),
        &MessageId::new("m1"),
        &Body {
            text: Some("secret forum stuff".into()),
            ..Body::default()
        },
    )
    .unwrap();
    s.apply_delta(
        &a("anon"),
        &Delta {
            mailboxes: Some(vec![mailbox("anon", "New", MailboxRole::Custom)]),
            ..Delta::default()
        },
    )
    .unwrap();
    assert!(entries(&mut s).is_empty(), "{:?}", entries(&mut s));
    s.mirror_backfill(None).unwrap();
    assert!(
        entries(&mut s).iter().all(|(_, acct, _)| acct != "anon"),
        "{:?}",
        entries(&mut s)
    );
    // A persona change is recorded (so the mirror can purge it).
    drain(&mut s);
    s.upsert_account(&account("work", Persona::Anon)).unwrap();
    assert_eq!(
        entries(&mut s),
        vec![(OutboxKind::Account, "work".into(), String::new())]
    );
}

#[test]
fn a_change_made_while_an_entry_is_in_flight_is_sent_again() {
    let mut s = store();
    s.set_mirror_enabled(true).unwrap();
    put(&mut s, &[M::default().meta()]);
    let taken: Vec<OutboxItem> = s.mirror_take(10).unwrap();
    // While it is being sent, the message changes again.
    put(
        &mut s,
        &[M {
            seen: true,
            ..M::default()
        }
        .meta()],
    );
    s.mirror_done(&taken).unwrap();
    assert_eq!(
        entries(&mut s),
        vec![(OutboxKind::Message, "personal".into(), "m1".into())]
    );
    let st = s.mirror_state().unwrap();
    assert_eq!((st.pending, st.sent), (1, 1));
}

#[test]
fn backfill_queues_everything_but_anon_newest_first() {
    let mut s = store();
    put(
        &mut s,
        &[
            M {
                id: "old",
                thread: "t-old",
                date: 1_600_000_000,
                ..M::default()
            }
            .meta(),
            M {
                id: "new",
                thread: "t-new",
                date: 1_700_000_000,
                ..M::default()
            }
            .meta(),
            M {
                account: "work",
                id: "w1",
                ..M::default()
            }
            .meta(),
        ],
    );
    s.put_body(
        &a("personal"),
        &MessageId::new("new"),
        &Body {
            text: Some("x".into()),
            ..Body::default()
        },
    )
    .unwrap();
    s.set_mirror_enabled(true).unwrap();
    assert!(entries(&mut s).is_empty());
    let n = s.mirror_backfill(None).unwrap();
    let e = entries(&mut s);
    assert_eq!(n as usize, e.len());
    assert_eq!(e.iter().filter(|x| x.0 == OutboxKind::Account).count(), 2);
    assert_eq!(e.iter().filter(|x| x.0 == OutboxKind::Mailbox).count(), 10);
    let msgs: Vec<_> = e
        .iter()
        .filter(|x| matches!(x.0, OutboxKind::Message | OutboxKind::Body))
        .collect();
    assert_eq!(msgs.len(), 3);
    assert_eq!(
        msgs.iter().find(|x| x.2 == "new").unwrap().0,
        OutboxKind::Body,
        "a cached body comes with its message"
    );
    let pos = |id: &str| e.iter().position(|x| x.2 == id).unwrap();
    assert!(pos("new") < pos("old"), "newest first");
    // Running it again does not duplicate.
    s.mirror_backfill(None).unwrap();
    assert_eq!(entries(&mut s).len(), e.len());
    assert!(s.mirror_state().unwrap().backfill_at.is_some());
    // One account only.
    drain(&mut s);
    s.mirror_backfill(Some(&a("work"))).unwrap();
    assert!(entries(&mut s).iter().all(|x| x.1 == "work"));
}

#[test]
fn the_outbox_survives_a_restart_and_off_empties_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mail.db");
    {
        let mut s = Store::open(&path).unwrap();
        s.upsert_account(&account("personal", Persona::Personal))
            .unwrap();
        s.set_mirror_enabled(true).unwrap();
        put(&mut s, &[M::default().meta()]);
    }
    let mut s = Store::open(&path).unwrap();
    assert!(s.mirror_enabled().unwrap());
    assert!(entries(&mut s).contains(&(OutboxKind::Message, "personal".into(), "m1".into())));
    // A second connection (the mirror's own) sees the same entries.
    let mut other = Store::open(&path).unwrap();
    assert_eq!(
        other.mirror_take(10).unwrap().len(),
        2,
        "the message and the INBOX it named"
    );
    assert!(s.set_mirror_enabled(false).unwrap());
    assert!(entries(&mut s).is_empty());
}

#[test]
fn entries_that_keep_failing_are_set_aside() {
    let mut s = store();
    s.set_mirror_enabled(true).unwrap();
    put(&mut s, &[M::default().meta()]);
    let item = s.mirror_take(1).unwrap().remove(0);
    for _ in 0..lumbridge_mail_store::MIRROR_MAX_ATTEMPTS {
        s.mirror_failed(&item, "refused").unwrap();
    }
    assert!(s.mirror_take(10).unwrap().is_empty());
    let st = s.mirror_state().unwrap();
    assert_eq!((st.pending, st.stuck), (0, 1));
    assert_eq!(st.last_error.as_deref(), Some("refused"));
    assert_eq!(s.mirror_retry_stuck().unwrap(), 1);
    assert_eq!(s.mirror_take(10).unwrap().len(), 1);
}

#[test]
fn removing_an_account_records_it() {
    let mut s = store();
    s.set_mirror_enabled(true).unwrap();
    put(&mut s, &[M::default().meta()]);
    drain(&mut s);
    s.remove_account(&a("personal")).unwrap();
    assert!(entries(&mut s).contains(&(OutboxKind::Account, "personal".into(), String::new())));
}
