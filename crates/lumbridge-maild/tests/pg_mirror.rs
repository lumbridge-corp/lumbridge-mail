//! The Postgres mirror end to end: the daemon with demo accounts mirrors into a throwaway Postgres
//! (LUMBRIDGE_MAIL_PG_TEST_URL; skipped without it), as mail_writer. Made-up mail only.

use lumbridge_mail_ai::ScriptedModel;
use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_mail_pg::migrate::{self, MigrateOptions};
use lumbridge_mail_pg::testing::{SERIAL, TestDb};
use lumbridge_maild::mirror::MirrorStatus;
use lumbridge_maild::{Client, Daemon, DemoMail, DemoProvider, DemoSet, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn config(dsn_file: &str, enabled: bool, personal_mirror: bool) -> String {
    format!(
        r##"
[daemon]
sync_interval_s = 30
first_retry_s = 1
max_retry_s = 2
max_backoff_s = 2

[pg]
enabled = {enabled}
dsn_file = "{dsn_file}"
poll_ms = 100
max_backoff_s = 1

[[accounts]]
id = "personal"
persona = "personal"
label = "Personal"
email = "sam@example.com"
name = "Sam Example"
mirror = {personal_mirror}
[accounts.provider]
kind = "demo"

[[accounts]]
id = "work"
persona = "work"
label = "Work"
email = "sam@work.example"
name = "Sam Example"
mirror = false
[accounts.provider]
kind = "demo"

[[accounts]]
id = "anon"
persona = "anon"
label = "Forum"
email = "otter@anon.example"
name = "Otter"
[accounts.provider]
kind = "demo"
"##
    )
}

struct Env {
    _dir: tempfile::TempDir,
    paths: Paths,
    dsn_file: std::path::PathBuf,
    demos: HashMap<&'static str, Arc<DemoProvider>>,
    db: TestDb,
}

fn answer(req: &lumbridge_mail_ai::ChatRequest) -> Result<String> {
    Ok(match req.task {
        "triage" => json!({"category": "personal", "priority": 50, "summary": "A note from a friend", "needs_reply": false, "due": null}),
        _ => json!({}),
    }
    .to_string())
}

impl Env {
    async fn new(label: &str) -> Option<Env> {
        let db = TestDb::create(label).await?;
        let mut admin = db.admin().await;
        migrate::migrate(&mut admin, &MigrateOptions::dim(8))
            .await
            .expect("migrate");
        db.set_role_passwords().await;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home.clone().into_os_string()));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let dsn_file = dir.path().join("pg.dsn");
        let me = |name: &str, email: &str| Address::new(Some(name), email);
        let mut demos = HashMap::new();
        demos.insert(
            "personal",
            Arc::new(DemoProvider::new(
                AccountId::new("personal"),
                DemoSet::Personal,
                me("Sam Example", "sam@example.com"),
            )),
        );
        demos.insert(
            "work",
            Arc::new(DemoProvider::new(
                AccountId::new("work"),
                DemoSet::Work,
                me("Sam Example", "sam@work.example"),
            )),
        );
        demos.insert(
            "anon",
            Arc::new(DemoProvider::new(
                AccountId::new("anon"),
                DemoSet::Anon,
                me("Otter", "otter@anon.example"),
            )),
        );
        let env = Env {
            _dir: dir,
            paths,
            dsn_file,
            demos,
            db,
        };
        env.write_dsn(&env.db.role_dsn("mail_writer"));
        env.write_config(true, true);
        Some(env)
    }

    fn write_dsn(&self, dsn: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&self.dsn_file, format!("{dsn}\n")).unwrap();
        std::fs::set_permissions(&self.dsn_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn write_config(&self, enabled: bool, personal_mirror: bool) {
        std::fs::write(
            self.paths.config_file(),
            config(self.dsn_file.to_str().unwrap(), enabled, personal_mirror),
        )
        .unwrap();
    }

    async fn start(&self) -> Daemon {
        let mut o = Options::new(self.paths.clone());
        for (id, p) in &self.demos {
            o.providers
                .insert(AccountId::new(*id), p.clone() as Arc<dyn Provider>);
        }
        o.model = Some(Arc::new(ScriptedModel::with(answer)));
        Daemon::start(o).await.unwrap()
    }

    async fn person(&self) -> Client {
        let token = Client::read_person_token(&self.paths.person_token()).expect("token file");
        Client::connect(&self.paths.socket(), Some(token), "test person")
            .await
            .unwrap()
    }

    async fn reader(&self) -> lumbridge_mail_pg::tokio_postgres::Client {
        lumbridge_mail_pg::connect::connect(&self.db.role_dsn("mail_reader"), &Default::default())
            .await
            .unwrap()
    }
}

macro_rules! eventually {
    ($what:expr, $cond:expr) => {{
        let start = Instant::now();
        loop {
            if $cond {
                break;
            }
            if start.elapsed() > Duration::from_secs(30) {
                panic!("timed out waiting for: {}", $what);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }};
}

async fn n(c: &lumbridge_mail_pg::tokio_postgres::Client, sql: &str) -> i64 {
    c.query_one(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get(0)
}

async fn status(c: &mut Client) -> MirrorStatus {
    c.call("mirror.status", json!({})).await.unwrap()
}

async fn store_count(c: &mut Client, account: &str) -> usize {
    // Message counts per thread, all pages.
    let mut total = 0;
    let mut page = None;
    loop {
        let q = Query {
            accounts: Some(vec![AccountId::new(account)]),
            limit: 500,
            page,
            ..Query::default()
        };
        let p: ThreadPage = c.call(method::THREADS, q).await.unwrap();
        total += p
            .threads
            .iter()
            .map(|t| t.message_count as usize)
            .sum::<usize>();
        match p.next {
            Some(n) => page = Some(n),
            None => return total,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_mirrors_mail_and_keeps_the_privacy_rules() {
    let _g = SERIAL.lock().await;
    let Some(env) = Env::new("roundtrip").await else {
        return;
    };
    let d = env.start().await;
    let mut c = env.person().await;
    let pg = env.reader().await;
    let personal = env.demos["personal"].messages().len() as i64;
    eventually!(
        "personal mail in Postgres",
        n(
            &pg,
            "SELECT count(*) FROM mail.messages WHERE account = 'personal'"
        )
        .await
            == personal
    );

    // Anon and mirror = false accounts: nothing at all, not even the account row.
    assert_eq!(
        n(
            &pg,
            "SELECT count(*) FROM mail.accounts WHERE id <> 'personal'"
        )
        .await,
        0
    );
    assert_eq!(
        n(
            &pg,
            "SELECT count(*) FROM mail.messages WHERE account <> 'personal'"
        )
        .await,
        0
    );
    assert_eq!(
        n(
            &pg,
            "SELECT count(*) FROM mail.mailboxes WHERE account <> 'personal'"
        )
        .await,
        0
    );
    assert_eq!(n(&pg, "SELECT count(*) FROM mail.messages WHERE participants LIKE '%anon.example%' OR participants LIKE '%work.example%'").await, 0);

    // The security code: metadata and the one line only.
    let r = pg.query_one("SELECT id, subject, snippet, sensitive_kind FROM mail.messages WHERE sensitive_kind IS NOT NULL", &[]).await.unwrap();
    let code_id: String = r.get(0);
    assert_eq!(r.get::<_, String>(1), "A security code from Acme");
    assert_eq!(r.get::<_, String>(2), "");
    assert_eq!(
        r.get::<_, Option<String>>(3).as_deref(),
        Some("A security code")
    );
    assert_eq!(n(&pg, "SELECT count(*) FROM mail.messages WHERE subject LIKE '%482913%' OR snippet LIKE '%482913%' OR subject LIKE '%verification%'").await, 0);

    // Bodies arrive when they are fetched; the code's body never does.
    let dinner = env.demos["personal"].find("Dinner Saturday?").unwrap();
    for id in [dinner.id.clone(), MessageId::new(code_id.as_str())] {
        let _: Body = c
            .call(
                method::BODY,
                MessageRef {
                    account: AccountId::new("personal"),
                    message: id,
                },
            )
            .await
            .unwrap();
    }
    eventually!(
        "the dinner body in Postgres",
        n(
            &pg,
            &format!(
                "SELECT count(*) FROM mail.bodies WHERE message_id = '{}'",
                dinner.id
            )
        )
        .await
            == 1
    );
    let text: String = pg
        .query_one(
            &format!(
                "SELECT body_text FROM mail.bodies WHERE message_id = '{}'",
                dinner.id
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(text.contains("noodle place"), "{text}");
    assert!(
        n(
            &pg,
            &format!(
                "SELECT count(*) FROM mail.jobs WHERE kind = 'embed' AND message_id = '{}'",
                dinner.id
            )
        )
        .await
            == 1,
        "embedding queued"
    );
    let st = status(&mut c).await;
    assert_eq!(st.state.stuck, 0);
    assert_eq!(
        n(
            &pg,
            &format!("SELECT count(*) FROM mail.bodies WHERE message_id = '{code_id}'")
        )
        .await,
        0
    );
    assert_eq!(
        n(
            &pg,
            "SELECT count(*) FROM mail.bodies WHERE body_text LIKE '%482913%'"
        )
        .await,
        0
    );
    assert_eq!(
        n(
            &pg,
            &format!("SELECT count(*) FROM mail.jobs WHERE message_id = '{code_id}'")
        )
        .await,
        0,
        "no work queued for the code"
    );

    // Triage arrives, the sensitive thread's summary is its line.
    eventually!(
        "triage in Postgres",
        n(&pg, "SELECT count(*) FROM mail.triage").await > 0
    );
    eventually!(
        "the code's triage",
        n(&pg, "SELECT count(*) FROM mail.triage WHERE thread_id = 't-acme-code' AND summary = 'A security code from Acme'").await == 1
    );

    // A flag change and an archive reach Postgres.
    let unread: String = pg
        .query_one("SELECT id FROM mail.messages WHERE NOT seen AND sensitive_kind IS NULL AND 'INBOX' = ANY (mailbox_ids) ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()
        .get(0);
    let _: Value = c
        .call(
            method::ACT,
            Act {
                account: AccountId::new("personal"),
                action: Action::SetSeen {
                    ids: vec![MessageId::new(unread.as_str())],
                    seen: true,
                },
                caller: Caller::Person,
            },
        )
        .await
        .unwrap();
    eventually!(
        "seen in Postgres",
        n(
            &pg,
            &format!("SELECT count(*) FROM mail.messages WHERE id = '{unread}' AND seen")
        )
        .await
            == 1
    );
    let _: Value = c
        .call(
            method::ACT,
            Act {
                account: AccountId::new("personal"),
                action: Action::Archive {
                    ids: vec![MessageId::new(unread.as_str())],
                },
                caller: Caller::Person,
            },
        )
        .await
        .unwrap();
    eventually!(
        "archived in Postgres",
        n(&pg, &format!("SELECT count(*) FROM mail.messages WHERE id = '{unread}' AND NOT ('INBOX' = ANY (mailbox_ids))")).await == 1
    );

    // Gone from the server: gone from Postgres (with its body).
    env.demos["personal"].vanish(&dinner.id);
    let _: Value = c
        .call(method::SYNC, json!({ "account": "personal" }))
        .await
        .unwrap();
    eventually!(
        "removed from Postgres",
        n(
            &pg,
            &format!(
                "SELECT count(*) FROM mail.messages WHERE id = '{}'",
                dinner.id
            )
        )
        .await
            == 0
    );
    assert_eq!(
        n(
            &pg,
            &format!(
                "SELECT count(*) FROM mail.bodies WHERE message_id = '{}'",
                dinner.id
            )
        )
        .await,
        0
    );

    // New mail arrives.
    env.demos["personal"].deliver(DemoMail::new(
        "lamps",
        "Shop",
        "orders@shop.example",
        "Your lamps have shipped",
        "Three lamps are on their way.",
    ));
    let _: Value = c
        .call(method::SYNC, json!({ "account": "personal" }))
        .await
        .unwrap();
    eventually!(
        "new mail in Postgres",
        n(
            &pg,
            "SELECT count(*) FROM mail.messages WHERE subject = 'Your lamps have shipped'"
        )
        .await
            == 1
    );
    assert_eq!(n(&pg, "SELECT count(*) FROM mail.search('lamps')").await, 1);

    // Status: caught up, counts from Postgres.
    eventually!("caught up", status(&mut c).await.state.pending == 0);
    let st = status(&mut c).await;
    assert!(st.configured && st.running && st.live.connected, "{st:?}");
    assert!(st.state.last_error.is_none(), "{st:?}");
    let counts = st.pg.expect("counts");
    assert_eq!(counts.accounts, 1);
    assert_eq!(counts.sensitive, 1);
    assert_eq!(st.excluded, vec![AccountId::new("work")]);

    // Opting the account out removes it from Postgres; opting back in sends it all again.
    env.write_config(true, false);
    let _: Value = c.call(method::CONFIG_RELOAD, json!({})).await.unwrap();
    eventually!(
        "opted out",
        n(&pg, "SELECT count(*) FROM mail.accounts").await == 0
    );
    assert_eq!(n(&pg, "SELECT count(*) FROM mail.messages").await, 0);
    env.write_config(true, true);
    let _: Value = c.call(method::CONFIG_RELOAD, json!({})).await.unwrap();
    let now_personal = env.demos["personal"].messages().len() as i64;
    eventually!(
        "back in",
        n(&pg, "SELECT count(*) FROM mail.messages").await == now_personal
    );
    let backfill: Value = c.call("mirror.backfill", json!({})).await.unwrap();
    assert!(backfill["queued"].as_u64().unwrap() > 0);

    drop(pg);
    d.shutdown().await;
    env.db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_outbox_survives_a_postgres_outage_and_a_restart() {
    let _g = SERIAL.lock().await;
    let Some(env) = Env::new("outage").await else {
        return;
    };
    // Postgres is down from the start.
    env.write_dsn(&env.db.dead_dsn());
    let d = env.start().await;
    let mut c = env.person().await;
    // The daemon works as usual without it.
    eventually!(
        "personal synced",
        store_count(&mut c, "personal").await == env.demos["personal"].messages().len()
    );
    eventually!(
        "the error is reported",
        status(&mut c).await.state.last_error.is_some()
    );
    let st = status(&mut c).await;
    assert!(st.state.pending > 0, "{st:?}");
    assert!(!st.live.connected);
    assert!(st.pg_error.is_some());
    let pending_before = st.state.pending;
    d.shutdown().await;

    // Restart with Postgres back: the outbox kept everything and drains.
    env.write_dsn(&env.db.role_dsn("mail_writer"));
    let d = env.start().await;
    let mut c = env.person().await;
    let pg = env.reader().await;
    let personal = env.demos["personal"].messages().len() as i64;
    eventually!(
        "drained after the restart",
        n(&pg, "SELECT count(*) FROM mail.messages").await == personal
    );
    eventually!("caught up", status(&mut c).await.state.pending == 0);
    let st = status(&mut c).await;
    assert!(
        st.state.sent >= pending_before.min(personal as u64),
        "{st:?}"
    );
    assert!(
        st.state.last_error.is_none(),
        "a success clears the error: {st:?}"
    );

    // Postgres goes away while running: changes wait, then go.
    env.write_dsn(&env.db.dead_dsn());
    let admin = env.db.admin().await;
    admin.batch_execute(&format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}' AND usename = 'mail_writer'", env.db.name)).await.unwrap();
    env.demos["personal"].deliver(DemoMail::new(
        "outage",
        "Ada Lovelace",
        "ada@example.com",
        "Written during the outage",
        "Hello",
    ));
    let _: Value = c
        .call(method::SYNC, json!({ "account": "personal" }))
        .await
        .unwrap();
    eventually!("queued while down", status(&mut c).await.state.pending > 0);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        n(
            &pg,
            "SELECT count(*) FROM mail.messages WHERE subject = 'Written during the outage'"
        )
        .await,
        0
    );
    env.write_dsn(&env.db.role_dsn("mail_writer"));
    eventually!(
        "sent after the outage",
        n(
            &pg,
            "SELECT count(*) FROM mail.messages WHERE subject = 'Written during the outage'"
        )
        .await
            == 1
    );

    drop((pg, admin));
    d.shutdown().await;
    env.db.drop_db().await;
}

/// Mail the store groups again (`Store::rethread`, what the daemon does once for Zoho accounts after a
/// threading upgrade) gets its new thread id in Postgres too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rethread_reaches_postgres() {
    let _g = SERIAL.lock().await;
    let Some(env) = Env::new("rethread").await else {
        return;
    };
    let d = env.start().await;
    let mut c = env.person().await;
    eventually!("synced", store_count(&mut c, "personal").await == env.demos["personal"].messages().len());
    drop(c);
    d.shutdown().await;

    // Two one-message threads of the kind a provider without thread ids leaves (thread = own id).
    let one = |id: &str, subject: &str, date: i64| MessageMeta {
        id: MessageId::new(id),
        account: AccountId::new("personal"),
        thread: ThreadId::new(id),
        mailboxes: vec![MailboxId::new("INBOX")],
        from: vec![Address::new(Some("Ada Lovelace"), "ada@example.org")],
        to: vec![Address::new(Some("Sam Example"), "sam@example.com")],
        cc: vec![],
        bcc: vec![],
        subject: subject.into(),
        snippet: "Made-up planning notes".into(),
        date,
        flags: Flags::default(),
        has_attachments: false,
        size: 100,
        rfc_message_id: None,
        in_reply_to: None,
        references: vec![],
        partial: false,
    };
    {
        let mut s = lumbridge_mail_store::Store::open(env.paths.database()).unwrap();
        s.merge_metas(&AccountId::new("personal"), &[one("z1", "Garden plans", 1_700_000_000), one("z2", "Re: Garden plans", 1_700_003_600)]).unwrap();
    }
    let d = env.start().await;
    let pg = env.reader().await;
    let thread_of = |id: &'static str| {
        let pg = &pg;
        async move {
            pg.query_opt("SELECT thread_id FROM mail.messages WHERE account = 'personal' AND id = $1", &[&id])
                .await
                .unwrap()
                .map(|r| r.get::<_, String>(0))
        }
    };
    eventually!("both mirrored apart", thread_of("z2").await.as_deref() == Some("z2") && thread_of("z1").await.as_deref() == Some("z1"));
    d.shutdown().await;

    {
        let mut s = lumbridge_mail_store::Store::open(env.paths.database()).unwrap();
        let changed = s.rethread(&AccountId::new("personal")).unwrap();
        assert!(changed.contains(&ThreadId::new("z2")), "{changed:?}");
        assert_eq!(s.message(&AccountId::new("personal"), &MessageId::new("z2")).unwrap().unwrap().thread, ThreadId::new("z1"));
    }
    let d = env.start().await;
    eventually!("the new thread id in Postgres", thread_of("z2").await.as_deref() == Some("z1"));
    assert_eq!(thread_of("z1").await.as_deref(), Some("z1"));
    assert_eq!(
        n(&pg, "SELECT count(DISTINCT thread_id) FROM mail.messages WHERE account = 'personal' AND id IN ('z1', 'z2')").await,
        1
    );
    drop(pg);
    d.shutdown().await;
    env.db.drop_db().await;
}

/// Not a pass/fail test: prints how fast a full backfill of made-up mail goes (LBMAIL_PG_BENCH_N
/// messages, default 3000) from turning the mirror on to the last row in Postgres.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mirror_throughput_on_made_up_mail() {
    let _g = SERIAL.lock().await;
    let Some(env) = Env::new("throughput").await else {
        return;
    };
    let total: usize = std::env::var("LBMAIL_PG_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    for i in 0..total {
        let mut m = DemoMail::new(
            &format!("bench-{}", i / 3),
            &format!("Sender {}", i % 97),
            &format!("sender{}@bench.example", i % 97),
            &format!("Made-up message {i} about invoices, lunches and lamps"),
            "Hello Sam,\n\nThis is made-up text for the throughput test. It talks about nothing in particular.\n\nBye",
        );
        m.hours_ago = (i as i64) % 2000;
        env.demos["personal"].deliver(m);
    }
    env.write_config(false, true);
    let d = env.start().await;
    let mut c = env.person().await;
    let expected = env.demos["personal"].messages().len();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(300) {
        let st: Vec<AccountStatus> = c.call(method::STATUS, json!({})).await.unwrap();
        if st.iter().all(|s| s.last_sync.is_some())
            && store_count(&mut c, "personal").await >= expected
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    eprintln!(
        "bench: {expected} messages synced into the store in {:.1} s",
        t0.elapsed().as_secs_f64()
    );

    env.write_config(true, true);
    let pg = env.reader().await;
    let t = Instant::now();
    let _: Value = c.call(method::CONFIG_RELOAD, json!({})).await.unwrap();
    let start_entries = status(&mut c).await.state.pending;
    loop {
        if n(&pg, "SELECT count(*) FROM mail.messages").await as usize >= expected {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(600), "backfill too slow");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "bench: backfill of {expected} messages ({start_entries} outbox entries) into Postgres in {secs:.2} s = {:.0} messages/s",
        expected as f64 / secs
    );
    let st = status(&mut c).await;
    eprintln!(
        "bench: last batch {} entries in {} ms",
        st.live.last_batch_items, st.live.last_batch_ms
    );

    // A burst of changes while running: flags on 500 messages.
    let ids: Vec<MessageId> = env.demos["personal"]
        .messages()
        .iter()
        .take(500)
        .map(|m| m.id.clone())
        .collect();
    let t = Instant::now();
    let _: Value = c
        .call(
            method::ACT,
            Act {
                account: AccountId::new("personal"),
                action: Action::SetFlagged { ids, flagged: true },
                caller: Caller::Person,
            },
        )
        .await
        .unwrap();
    loop {
        if n(&pg, "SELECT count(*) FROM mail.messages WHERE flagged").await >= 500 {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(120));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    eprintln!(
        "bench: 500 flag changes visible in Postgres after {:.2} s",
        t.elapsed().as_secs_f64()
    );
    drop(pg);
    d.shutdown().await;
    env.db.drop_db().await;
}
