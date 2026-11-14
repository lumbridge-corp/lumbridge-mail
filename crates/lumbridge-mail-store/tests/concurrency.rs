//! Two connections on one database file, as in the daemon: the main one (sync, bodies, triage)
//! and the Postgres mirror's own (outbox drain). Both write. In WAL mode a transaction that starts
//! by reading and then writes cannot wait for the other writer once that writer has committed (its
//! snapshot is stale): SQLite answers SQLITE_BUSY at once, the busy timeout does not apply. Every
//! write transaction must therefore take the write lock when it begins (BEGIN IMMEDIATE).
mod common;

use common::*;
use lumbridge_mail_core::*;
use lumbridge_mail_store::Store;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn setup(path: &std::path::Path) -> Store {
    let mut s = Store::open(path).unwrap();
    s.upsert_account(&account("personal", Persona::Personal))
        .unwrap();
    s.apply_delta(
        &AccountId::new("personal"),
        &Delta {
            mailboxes: Some(standard_mailboxes("personal")),
            ..Delta::default()
        },
    )
    .unwrap();
    s.set_mirror_enabled(true).unwrap();
    s
}

#[test]
fn sync_writes_and_the_mirror_drain_never_see_busy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mail.db");
    let mut main = setup(&path);
    let mut mirror = Store::open(&path).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let d2 = done.clone();
    // The mirror: drain as fast as it can, like a busy outbox task.
    let drain = std::thread::spawn(move || {
        let mut sent = 0usize;
        let mut errors = Vec::new();
        while !d2.load(Ordering::SeqCst)
            || mirror
                .mirror_state()
                .map(|s| s.pending > 0)
                .unwrap_or(false)
        {
            match mirror.mirror_take(25) {
                Ok(items) => {
                    sent += items.len();
                    if let Err(e) = mirror.mirror_done(&items) {
                        errors.push(e.to_string());
                    }
                }
                Err(e) => errors.push(e.to_string()),
            }
        }
        (sent, errors)
    });
    // Sync: pages of messages (threading reads first, then writes), bodies, flag changes.
    let mut errors = Vec::new();
    let pages = 80;
    for p in 0..pages {
        let ids: Vec<String> = (0..20).map(|i| format!("m{p}-{i}")).collect();
        let metas: Vec<MessageMeta> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                M {
                    id,
                    thread: if i % 3 == 0 { "t-shared" } else { id },
                    date: 1_700_000_000 + (p * 20 + i) as i64,
                    seen: p % 2 == 0,
                    ..M::default()
                }
                .meta()
            })
            .collect();
        if let Err(e) = main.apply_delta(
            &AccountId::new("personal"),
            &Delta {
                upserted: metas,
                cursor: Some(SyncCursor(format!("c{p}"))),
                ..Delta::default()
            },
        ) {
            errors.push(format!("apply_delta page {p}: {e}"));
        }
        let body = Body {
            text: Some(format!("made-up body {p}")),
            ..Body::default()
        };
        if let Err(e) = main.put_body(&AccountId::new("personal"), &MessageId::new(&ids[0]), &body)
        {
            errors.push(format!("put_body page {p}: {e}"));
        }
    }
    done.store(true, Ordering::SeqCst);
    let (sent, drain_errors) = drain.join().unwrap();
    assert!(errors.is_empty(), "sync writes failed: {errors:?}");
    assert!(
        drain_errors.is_empty(),
        "the drain failed: {drain_errors:?}"
    );
    assert!(
        sent >= pages * 20,
        "every message went through the outbox ({sent})"
    );
    let st = main.mirror_state().unwrap();
    assert_eq!((st.pending, st.stuck), (0, 0));
}
