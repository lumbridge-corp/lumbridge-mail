//! Schema version 2 and the workers' SQL against a real (throwaway) Postgres: set
//! LUMBRIDGE_MAIL_PG_TEST_URL (see tests/pg.rs). Made-up mail only.

use lumbridge_mail_core::*;
use lumbridge_mail_pg::migrate::{self, MigrateOptions};
use lumbridge_mail_pg::testing::{SERIAL, TestDb};
use lumbridge_mail_pg::work::*;
use lumbridge_mail_pg::*;
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;

async fn migrated(label: &str, dim: u32) -> Option<TestDb> {
    let db = TestDb::create(label).await?;
    let mut admin = db.admin().await;
    migrate::migrate(&mut admin, &MigrateOptions::dim(dim))
        .await
        .expect("migrate");
    db.set_role_passwords().await;
    Some(db)
}

fn account(id: &str) -> Account {
    Account {
        id: AccountId::new(id),
        persona: Persona::Personal,
        label: id.into(),
        email: format!("sam@{id}.example"),
        name: "Sam Example".into(),
        provider: ProviderKind::Demo,
        color: None,
        ai: AiPolicy::LocalOnly,
        readonly: false,
    }
}

fn meta(account: &str, id: &str, subject: &str, date: i64) -> MessageMeta {
    MessageMeta {
        id: MessageId::new(id),
        account: AccountId::new(account),
        thread: ThreadId::new(format!("t-{id}")),
        mailboxes: vec![MailboxId::new("INBOX")],
        from: vec![Address::new(Some("Ada Example"), "ada@example.com")],
        to: vec![Address::new(Some("Sam Example"), "sam@example.com")],
        cc: vec![],
        bcc: vec![],
        subject: subject.into(),
        snippet: subject.into(),
        date,
        flags: Flags::default(),
        has_attachments: false,
        size: 100,
        rfc_message_id: None,
        in_reply_to: None,
        references: vec![],
        partial: false,
    }
}

fn with_body(m: &MessageMeta, text: &str) -> Change {
    let b = Body {
        text: Some(text.into()),
        ..Body::default()
    };
    Change::message(
        MessageRow::from_meta(m),
        BodyChange::Put(Box::new(BodyRow::from_body(
            m.account.as_str(),
            m.id.as_str(),
            &b,
        ))),
    )
}

async fn n(c: &tokio_postgres::Client, sql: &str) -> i64 {
    c.query_one(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get(0)
}

async fn writer(db: &TestDb) -> Writer {
    Writer::connect(&db.role_dsn("mail_writer"), &ConnectOptions::default())
        .await
        .unwrap()
}

async fn work(db: &TestDb) -> Work {
    Work::connect(&db.role_dsn("mail_writer"), &ConnectOptions::default())
        .await
        .unwrap()
}

/// A unit vector along `axis` (with a little of the last axis so none is zero).
fn axis(dim: usize, a: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[a] = 1.0;
    v[dim - 1] += 0.05;
    v
}

#[tokio::test]
async fn version_1_with_data_moves_to_version_2() {
    let _g = SERIAL.lock().await;
    let Some(db) = TestDb::create("v1tov2").await else {
        return;
    };
    let mut admin = db.admin().await;
    let v1 = MigrateOptions {
        embedding_dim: 4,
        embedding_type: EmbeddingType::Vector,
        up_to: Some(1),
    };
    let r = migrate::migrate(&mut admin, &v1).await.unwrap();
    assert_eq!((r.from, r.to), (0, 1));
    // Data in version 1, as the mirror wrote it (a v1 writer; this build's refuses v1).
    admin
        .batch_execute(
            "SET ROLE mail_writer;
             INSERT INTO mail.accounts (id, persona, label, email, name, provider, ai_policy) VALUES ('personal', 'personal', 'P', 'sam@example.com', 'Sam', 'demo', 'any');
             INSERT INTO mail.messages (account, id, thread_id, subject) VALUES ('personal', 'm1', 't1', 'Flight to Lisbon'), ('personal', 'm2', 't2', 'Lunch');
             INSERT INTO mail.bodies (account, message_id, body_text, content_hash) VALUES ('personal', 'm1', 'Boarding at 8', 'h1'), ('personal', 'm2', 'Noon?', 'h2');
             DELETE FROM mail.jobs WHERE message_id = 'm2';
             DELETE FROM mail.jobs WHERE message_id = 'm1' AND kind = 'embed';
             INSERT INTO mail.embeddings (account, message_id, model, content_hash, embedding) VALUES ('personal', 'm1', 'old', 'x', '[1,0,0,0]');
             INSERT INTO mail.triage (account, thread_id, category, priority, summary, by, at) VALUES ('personal', 't1', 'transactional', 40, 'A flight', 'rules', now());
             RESET ROLE;",
        )
        .await
        .unwrap();
    // The dry run on the v1 database plans 0002 only.
    let plan = migrate::plan(&mut admin, &MigrateOptions::dim(8))
        .await
        .unwrap();
    assert_eq!(plan.current, 1);
    let names: Vec<&str> = plan.pending.iter().map(|p| p.1.as_str()).collect();
    assert_eq!(names, ["0002_embed_parse"]);
    assert!(plan.render().contains("halfvec(8)"));
    assert_eq!(
        n(&admin, "SELECT count(*) FROM mail.embeddings").await,
        1,
        "dry run changed nothing"
    );

    let r = migrate::migrate(&mut admin, &MigrateOptions::dim(8))
        .await
        .unwrap();
    assert_eq!(
        (r.from, r.to, r.applied.clone()),
        (1, 2, vec!["0002_embed_parse".to_string()])
    );
    // Everything else is still there.
    assert_eq!(n(&admin, "SELECT count(*) FROM mail.messages").await, 2);
    assert_eq!(n(&admin, "SELECT count(*) FROM mail.bodies").await, 2);
    assert_eq!(n(&admin, "SELECT count(*) FROM mail.triage").await, 1);
    assert_eq!(
        n(
            &admin,
            "SELECT count(*) FROM mail.bodies WHERE structured = '[]'::jsonb"
        )
        .await,
        2
    );
    // The old embedding is gone and its message queued again.
    assert_eq!(n(&admin, "SELECT count(*) FROM mail.embeddings").await, 0);
    assert_eq!(
        n(
            &admin,
            "SELECT count(*) FROM mail.jobs WHERE kind = 'embed' AND message_id = 'm1'"
        )
        .await,
        1
    );
    assert_eq!(
        n(
            &admin,
            "SELECT count(*) FROM mail.jobs WHERE message_id = 'm2'"
        )
        .await,
        0
    );
    let ty: String = admin
        .query_one("SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'mail.embeddings'::regclass AND attname = 'embedding'", &[])
        .await
        .unwrap()
        .get(0);
    assert!(ty.ends_with("halfvec(8)"), "{ty}");
    let ops: String = admin
        .query_one(
            "SELECT pg_get_indexdef('mail.embeddings_hnsw'::regclass)",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        ops.contains("hnsw") && ops.contains("halfvec_cosine_ops"),
        "{ops}"
    );
    let params: serde_json::Value = admin
        .query_one(
            "SELECT params FROM mail.schema_version WHERE version = 2",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        (
            params["embedding_dim"].clone(),
            params["embedding_type"].clone()
        ),
        (json!(8), json!("halfvec"))
    );
    // This build's writer and workers now accept it; the reader sees the new views.
    db.set_role_passwords().await;
    writer(&db).await;
    let w = work(&db).await;
    assert_eq!(w.dim(), 8);
    let reader = connect::connect(&db.role_dsn("mail_reader"), &ConnectOptions::default())
        .await
        .unwrap();
    for v in [
        "facts",
        "receipts",
        "trips",
        "packages",
        "subscriptions",
        "bills",
        "recruiting",
        "events",
        "newsletters",
        "job_stats",
    ] {
        n(&reader, &format!("SELECT count(*) FROM mail.{v}")).await;
    }
    assert!(
        reader
            .batch_execute("INSERT INTO mail.job_stats (kind, hour) VALUES ('embed', now())")
            .await
            .is_err(),
        "the reader cannot write"
    );
    // A database at version 1 is refused by the workers until migrated.
    db.drop_db().await;
}

#[tokio::test]
async fn the_workers_refuse_an_old_schema() {
    let _g = SERIAL.lock().await;
    let Some(db) = TestDb::create("workold").await else {
        return;
    };
    let mut admin = db.admin().await;
    migrate::migrate(
        &mut admin,
        &MigrateOptions {
            up_to: Some(1),
            ..MigrateOptions::dim(8)
        },
    )
    .await
    .unwrap();
    db.set_role_passwords().await;
    let e = Work::connect(&db.role_dsn("mail_writer"), &ConnectOptions::default())
        .await
        .err()
        .unwrap();
    assert!(e.to_string().contains("lbmail pg migrate"), "{e}");
    db.drop_db().await;
}

#[tokio::test]
async fn jobs_are_claimed_retried_parked_and_finished() {
    let _g = SERIAL.lock().await;
    let Some(db) = migrated("jobs", 8).await else {
        return;
    };
    let mut wr = writer(&db).await;
    let mut changes = vec![Change::UpsertAccount(
        AccountRow::from_account(&account("personal"), false).unwrap(),
    )];
    for i in 0..5 {
        changes.push(with_body(
            &meta("personal", &format!("m{i}"), "Hello", 1_700_000_000 + i),
            &format!("Body {i}"),
        ));
    }
    wr.apply(&changes).await.unwrap();
    let w = work(&db).await;
    let admin = db.admin().await;
    assert_eq!(
        n(
            &admin,
            "SELECT count(*) FROM mail.jobs WHERE kind = 'embed'"
        )
        .await,
        5
    );

    let lease = Duration::from_secs(60);
    let a = w.claim(JobKind::Embed, 2, lease).await.unwrap();
    assert_eq!(a.len(), 2);
    assert!(
        a.iter()
            .all(|j| j.attempts == 1 && j.kind == JobKind::Embed)
    );
    // Leased jobs are not handed out again.
    let b = w.claim(JobKind::Embed, 10, lease).await.unwrap();
    assert_eq!(b.len(), 3);
    assert!(w.claim(JobKind::Embed, 10, lease).await.unwrap().is_empty());

    // Done deletes; a retry backs off; parking after max attempts.
    assert!(w.done(&a[0]).await.unwrap());
    assert!(
        !w.retry(
            &a[1],
            "embedding endpoint said 503",
            Duration::from_millis(1),
            2
        )
        .await
        .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    let again = w.claim(JobKind::Embed, 10, lease).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(
        (again[0].message_id.clone(), again[0].attempts),
        (a[1].message_id.clone(), 2)
    );
    assert!(
        w.retry(&again[0], "embedding endpoint said 503", Duration::ZERO, 2)
            .await
            .unwrap(),
        "parked"
    );
    assert!(
        w.claim(JobKind::Embed, 10, lease).await.unwrap().is_empty(),
        "parked jobs are not claimed"
    );
    let st = queue_state(w.client()).await.unwrap();
    let embed = st.iter().find(|q| q.kind == "embed").unwrap();
    assert_eq!((embed.parked, embed.waiting), (1, 3));
    assert_eq!(
        embed.last_error.as_deref(),
        Some("embedding endpoint said 503")
    );
    // Requeue the parked one.
    let r = Requeue {
        kind: JobKind::Embed,
        account: None,
        model: None,
        all: false,
        parked_only: true,
    };
    assert_eq!(w.requeue(&r).await.unwrap(), 1);
    let c = w.claim(JobKind::Embed, 10, lease).await.unwrap();
    assert_eq!((c.len(), c[0].attempts), (1, 1));

    // A body that changes while its job is worked on: finishing the old work keeps the new job.
    let job = &b[0];
    admin
        .execute("UPDATE mail.bodies SET body_text = 'changed', content_hash = 'new' WHERE account = $1 AND message_id = $2", &[&job.account, &job.message_id])
        .await
        .unwrap();
    assert!(!w.done(job).await.unwrap(), "re-queued meanwhile");
    assert_eq!(
        n(&admin, &format!("SELECT count(*) FROM mail.jobs WHERE kind = 'embed' AND message_id = '{}' AND attempts = 0", job.message_id)).await,
        1
    );
    // Deferring does not count as a try.
    let p = w.claim(JobKind::Parse, 1, lease).await.unwrap();
    w.defer(&p[0], Duration::from_millis(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let p2 = w.claim(JobKind::Parse, 10, lease).await.unwrap();
    assert!(
        p2.iter()
            .any(|j| j.message_id == p[0].message_id && j.attempts == 1)
    );
    // Stats add up per hour.
    w.note(
        JobKind::Embed,
        &Tally {
            done: 3,
            calls: 1,
            items: 3,
            busy_ms: 30,
            ..Tally::default()
        },
    )
    .await
    .unwrap();
    w.note(
        JobKind::Embed,
        &Tally {
            done: 2,
            unchanged: 2,
            busy_ms: 10,
            ..Tally::default()
        },
    )
    .await
    .unwrap();
    let st = queue_state(w.client()).await.unwrap();
    let embed = st.iter().find(|q| q.kind == "embed").unwrap();
    assert_eq!(
        (
            embed.last_hour.done,
            embed.last_hour.unchanged,
            embed.total.calls,
            embed.last_day.busy_ms
        ),
        (5, 2, 1, 40)
    );
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_workers_never_claim_the_same_job() {
    let _g = SERIAL.lock().await;
    let Some(db) = migrated("twoworkers", 8).await else {
        return;
    };
    let mut wr = writer(&db).await;
    let mut changes = vec![Change::UpsertAccount(
        AccountRow::from_account(&account("personal"), false).unwrap(),
    )];
    for i in 0..200 {
        changes.push(with_body(
            &meta("personal", &format!("m{i}"), "Hello", 1_700_000_000 + i),
            &format!("Body {i}"),
        ));
    }
    wr.apply(&changes).await.unwrap();
    let run = |w: Work| async move {
        let mut mine = Vec::new();
        loop {
            let jobs = w
                .claim(JobKind::Embed, 7, Duration::from_secs(60))
                .await
                .unwrap();
            if jobs.is_empty() {
                break;
            }
            for j in jobs {
                assert!(w.done(&j).await.unwrap());
                mine.push(j.message_id);
            }
        }
        mine
    };
    let (a, b) = tokio::join!(
        tokio::spawn(run(work(&db).await)),
        tokio::spawn(run(work(&db).await))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let sa: HashSet<_> = a.iter().cloned().collect();
    let sb: HashSet<_> = b.iter().cloned().collect();
    assert!(sa.is_disjoint(&sb), "a job was claimed twice");
    assert_eq!(sa.len() + sb.len(), 200);
    assert!(
        !a.is_empty() && !b.is_empty(),
        "both worked: {} / {}",
        a.len(),
        b.len()
    );
    db.drop_db().await;
}

#[tokio::test]
async fn embeddings_and_facts_are_idempotent_and_sensitive_mail_is_refused() {
    let _g = SERIAL.lock().await;
    let Some(db) = migrated("idem", 8).await else {
        return;
    };
    let mut wr = writer(&db).await;
    let m1 = meta("personal", "m1", "Flight to Lisbon", 1_794_312_000);
    let code = meta("personal", "c1", "Your code", 1_794_312_000);
    wr.apply(&[
        Change::UpsertAccount(AccountRow::from_account(&account("personal"), false).unwrap()),
        with_body(&m1, "Boarding at 8"),
        Change::message(
            MessageRow::sensitive(&code, "A security code", "A security code from Acme"),
            BodyChange::Drop,
        ),
    ])
    .await
    .unwrap();
    let mut w = work(&db).await;
    let admin = db.admin().await;
    let src = w.embed_source("personal", "m1").await.unwrap().unwrap();
    assert_eq!(
        (
            src.subject.as_str(),
            src.body_text.as_deref(),
            src.ai_policy.as_str()
        ),
        ("Flight to Lisbon", Some("Boarding at 8"), "local_only")
    );
    let s = w.embed_source("personal", "c1").await.unwrap().unwrap();
    assert!(s.sensitive_kind.is_some() && s.body_text.is_none());
    assert!(w.embed_source("personal", "nope").await.unwrap().is_none());

    let rows = |h: &str| {
        vec![
            EmbeddingRow {
                chunk: 0,
                content_hash: h.into(),
                vector: axis(8, 0),
            },
            EmbeddingRow {
                chunk: 1,
                content_hash: "c1".into(),
                vector: axis(8, 1),
            },
        ]
    };
    w.put_embeddings("personal", "m1", "model-a", &rows("c0"))
        .await
        .unwrap();
    let at: String = admin
        .query_one(
            "SELECT created_at::text FROM mail.embeddings WHERE chunk = 0",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    tokio::time::sleep(Duration::from_millis(20)).await;
    w.put_embeddings("personal", "m1", "model-a", &rows("c0"))
        .await
        .unwrap();
    let at2: String = admin
        .query_one(
            "SELECT created_at::text FROM mail.embeddings WHERE chunk = 0",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(at, at2, "unchanged chunks are not rewritten");
    assert_eq!(
        w.chunk_hashes("personal", "m1", "model-a").await.unwrap(),
        vec![(0, "c0".to_string()), (1, "c1".to_string())]
    );
    // Fewer chunks: the extra goes. Another model: the old model's rows go.
    w.put_embeddings("personal", "m1", "model-a", &rows("c0")[..1])
        .await
        .unwrap();
    assert_eq!(n(&admin, "SELECT count(*) FROM mail.embeddings").await, 1);
    w.put_embeddings("personal", "m1", "model-b", &rows("z"))
        .await
        .unwrap();
    assert_eq!(
        n(
            &admin,
            "SELECT count(*) FROM mail.embeddings WHERE model = 'model-b'"
        )
        .await,
        2
    );
    assert_eq!(
        n(
            &admin,
            "SELECT count(*) FROM mail.embeddings WHERE model = 'model-a'"
        )
        .await,
        0
    );
    // Wrong size and bad values are refused before Postgres sees them.
    assert!(
        w.put_embeddings(
            "personal",
            "m1",
            "model-b",
            &[EmbeddingRow {
                chunk: 0,
                content_hash: "x".into(),
                vector: vec![1.0; 9]
            }]
        )
        .await
        .is_err()
    );
    assert!(
        w.put_embeddings(
            "personal",
            "m1",
            "model-b",
            &[EmbeddingRow {
                chunk: 0,
                content_hash: "x".into(),
                vector: vec![f32::NAN; 8]
            }]
        )
        .await
        .is_err()
    );
    // Sensitive mail: the schema refuses embeddings and facts whoever writes them.
    let e = w
        .put_embeddings("personal", "c1", "model-b", &rows("c0"))
        .await
        .unwrap_err();
    assert!(is_refusal(&e) && e.to_string().contains("sensitive"), "{e}");
    let fact = |kind: &str, parser: &str, total: f64| FactRow {
        kind: kind.into(),
        data: json!({"merchant": "Bean Co", "total": total}),
        parser: parser.into(),
        parser_version: "1".into(),
        confidence: 0.9,
    };
    assert!(
        w.put_facts(
            "personal",
            "c1",
            None,
            &[fact("receipt", "rules/jsonld", 1.0)]
        )
        .await
        .is_err()
    );

    // Facts: replaced per message; other parsers' rows are left alone; unchanged rows keep their time.
    admin.batch_execute("INSERT INTO mail.parsed (account, message_id, kind, data, parser) VALUES ('personal', 'm1', 'receipt', '{}', 'someone-else')").await.unwrap();
    w.put_facts(
        "personal",
        "m1",
        Some("h"),
        &[
            fact("receipt", "rules/jsonld", 4.2),
            fact("receipt", "model/x", 4.2),
        ],
    )
    .await
    .unwrap();
    let t1: String = admin
        .query_one(
            "SELECT updated_at::text FROM mail.parsed WHERE parser = 'rules/jsonld'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    tokio::time::sleep(Duration::from_millis(20)).await;
    w.put_facts(
        "personal",
        "m1",
        Some("h"),
        &[fact("receipt", "rules/jsonld", 4.2)],
    )
    .await
    .unwrap();
    let t2: String = admin
        .query_one(
            "SELECT updated_at::text FROM mail.parsed WHERE parser = 'rules/jsonld'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(t1, t2);
    assert_eq!(
        n(
            &admin,
            "SELECT count(*) FROM mail.parsed WHERE parser = 'model/x'"
        )
        .await,
        0,
        "a fact no parser found any more goes"
    );
    assert_eq!(
        n(
            &admin,
            "SELECT count(*) FROM mail.parsed WHERE parser = 'someone-else'"
        )
        .await,
        1
    );
    // Requeue skips sensitive mail; with a model, only what lacks its embeddings.
    let r = Requeue {
        kind: JobKind::Embed,
        account: None,
        model: Some("model-b".into()),
        all: false,
        parked_only: false,
    };
    admin.batch_execute("DELETE FROM mail.jobs").await.unwrap();
    assert_eq!(
        w.requeue(&r).await.unwrap(),
        0,
        "m1 has model-b embeddings; c1 is sensitive"
    );
    let r = Requeue {
        model: Some("model-c".into()),
        ..r
    };
    assert_eq!(w.requeue(&r).await.unwrap(), 1);
    db.drop_db().await;
}

#[tokio::test]
async fn semantic_search_ranks_by_vector_and_the_views_read_facts() {
    let _g = SERIAL.lock().await;
    let Some(db) = migrated("semantic", 8).await else {
        return;
    };
    let mut wr = writer(&db).await;
    let mut other = account("work");
    other.persona = Persona::Work;
    let msgs = [
        (
            "personal",
            "trip",
            "Your itinerary",
            "Example Air EX110 to LIS",
            0usize,
        ),
        (
            "personal",
            "lunch",
            "Lunch on Friday",
            "Noon at the noodle place",
            2,
        ),
        ("personal", "invoice", "Invoice 77", "Amount due 61.20", 1),
        (
            "work",
            "deploy",
            "Deploy notes",
            "The kernel build passed",
            3,
        ),
    ];
    let mut changes = vec![
        Change::UpsertAccount(AccountRow::from_account(&account("personal"), false).unwrap()),
        Change::UpsertAccount(AccountRow::from_account(&other, false).unwrap()),
    ];
    for (a, id, s, b, _) in msgs {
        changes.push(with_body(&meta(a, id, s, 1_794_312_000), b));
    }
    wr.apply(&changes).await.unwrap();
    let mut w = work(&db).await;
    for (a, id, _, _, ax) in msgs {
        w.put_embeddings(
            a,
            id,
            "m",
            &[EmbeddingRow {
                chunk: 0,
                content_hash: id.into(),
                vector: axis(8, ax),
            }],
        )
        .await
        .unwrap();
    }
    let reader = connect::connect(&db.role_dsn("mail_reader"), &ConnectOptions::default())
        .await
        .unwrap();
    let mut q = SearchArgs {
        q: "when do I fly".into(),
        limit: 3,
        vector: Some(axis(8, 0)),
        model: Some("m".into()),
        ..SearchArgs::default()
    };
    let hits = search(&reader, &q).await.unwrap();
    assert_eq!(hits[0].message_id, "trip", "{hits:?}");
    assert_eq!(hits[0].matched, ["vector"]);
    q.vector = Some(axis(8, 3));
    assert_eq!(search(&reader, &q).await.unwrap()[0].message_id, "deploy");
    // Account filters apply before the vector candidates are cut.
    q.accounts = Some(vec!["personal".into()]);
    q.limit = 1;
    let hits = search(&reader, &q).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_ne!(hits[0].account, "work");
    // Text and vector both: found by both ranks first.
    let q = SearchArgs {
        q: "invoice".into(),
        limit: 5,
        vector: Some(axis(8, 1)),
        model: Some("m".into()),
        ..SearchArgs::default()
    };
    let hits = search(&reader, &q).await.unwrap();
    assert_eq!(hits[0].message_id, "invoice");
    assert!(
        ["text", "vector"]
            .iter()
            .all(|m| hits[0].matched.contains(&m.to_string())),
        "{:?}",
        hits[0].matched
    );
    // Another model's vectors are not compared; no vector is plain text search.
    let q = SearchArgs {
        q: "invoice".into(),
        limit: 5,
        vector: Some(axis(8, 0)),
        model: Some("other".into()),
        ..SearchArgs::default()
    };
    assert!(
        search(&reader, &q)
            .await
            .unwrap()
            .iter()
            .all(|h| !h.matched.contains(&"vector".to_string()))
    );
    assert!(
        search(
            &reader,
            &SearchArgs {
                q: "x".into(),
                vector: Some(vec![1.0; 3]),
                ..SearchArgs::default()
            }
        )
        .await
        .is_err(),
        "wrong size"
    );

    // Facts and the views (one bad row cannot break a view).
    let f = |kind: &str, data: serde_json::Value, parser: &str, c: f32| FactRow {
        kind: kind.into(),
        data,
        parser: parser.into(),
        parser_version: "1".into(),
        confidence: c,
    };
    w.put_facts("personal", "invoice", None, &[
        f("bill", json!({"payee": "City Water", "amount": 61.2, "currency": "USD", "due_date": "2026-12-01"}), "rules/jsonld", 0.95),
        f("bill", json!({"payee": "City Water Co", "amount": 61.2}), "model/x", 0.8),
    ]).await.unwrap();
    w.put_facts("personal", "trip", None, &[f("travel", json!({"mode": "flight", "carrier": "Example Air", "from": "SFO", "to": "LIS", "depart": "2027-03-14T08:05", "confirmation": "RXJ34P"}), "rules/jsonld", 0.95)]).await.unwrap();
    w.put_facts(
        "personal",
        "lunch",
        None,
        &[f(
            "receipt",
            json!({"merchant": "Noodles", "total": "not a number", "date": "2026-02-31"}),
            "model/x",
            0.6,
        )],
    )
    .await
    .unwrap();
    let r = reader
        .query_one(
            "SELECT payee, amount::text, due_date::text, parser FROM mail.bills",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        (
            r.get::<_, String>(0),
            r.get::<_, String>(1),
            r.get::<_, String>(2),
            r.get::<_, String>(3)
        ),
        (
            "City Water".into(),
            "61.2".into(),
            "2026-12-01".into(),
            "rules/jsonld".into()
        ),
        "the most confident fact"
    );
    let r = reader
        .query_one(
            "SELECT mode, origin, destination, depart_date::text, confirmation FROM mail.trips",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(r.get::<_, String>(4), "RXJ34P");
    assert_eq!(r.get::<_, String>(3), "2027-03-14");
    let r = reader
        .query_one(
            "SELECT merchant, total::text, date::text FROM mail.receipts",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(r.get::<_, String>(0), "Noodles");
    assert!(
        r.get::<_, Option<String>>(1).is_none() && r.get::<_, Option<String>>(2).is_none(),
        "bad values read as NULL"
    );
    let rows = parsed(
        &reader,
        &ParsedQuery {
            kind: Some("bill".into()),
            limit: 10,
            ..ParsedQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].data["payee"], "City Water");
    let all = parsed(
        &reader,
        &ParsedQuery {
            since: Some(1_794_000_000),
            accounts: Some(vec!["personal".into()]),
            limit: 10,
            ..ParsedQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(all.len(), 3);
    db.drop_db().await;
}
