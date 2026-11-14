//! Against a real (throwaway) Postgres: set LUMBRIDGE_MAIL_PG_TEST_URL to a superuser DSN of a server
//! made for tests (e.g. a pgvector/pgvector:pg17 container). Each test makes its own database.
//! Made-up mail only.

use lumbridge_mail_core::*;
use lumbridge_mail_pg::migrate::{self, API_ROLES, MigrateOptions};
use lumbridge_mail_pg::testing::{SERIAL, TestDb};
use lumbridge_mail_pg::*;

fn opts(dim: u32) -> MigrateOptions {
    MigrateOptions::dim(dim)
}

async fn migrated(label: &str, dim: u32) -> Option<TestDb> {
    let db = TestDb::create(label).await?;
    let mut admin = db.admin().await;
    migrate::migrate(&mut admin, &opts(dim))
        .await
        .expect("migrate");
    db.set_role_passwords().await;
    Some(db)
}

fn account(id: &str, persona: Persona) -> Account {
    Account {
        id: AccountId::new(id),
        persona,
        label: id.into(),
        email: format!("sam@{id}.example"),
        name: "Sam Example".into(),
        provider: ProviderKind::Demo,
        color: Some("#3b82f6".into()),
        ai: AiPolicy::LocalOnly,
        readonly: false,
    }
}

fn meta(
    account: &str,
    id: &str,
    thread: &str,
    from: (&str, &str),
    subject: &str,
    snippet: &str,
    date: i64,
) -> MessageMeta {
    MessageMeta {
        id: MessageId::new(id),
        account: AccountId::new(account),
        thread: ThreadId::new(thread),
        mailboxes: vec![MailboxId::new("INBOX")],
        from: vec![Address::new(Some(from.0), from.1)],
        to: vec![Address::new(Some("Sam Example"), "sam@example.com")],
        cc: vec![],
        bcc: vec![],
        subject: subject.into(),
        snippet: snippet.into(),
        date,
        flags: Flags::default(),
        has_attachments: false,
        size: 2048,
        rfc_message_id: Some(format!("{id}@mail.example")),
        in_reply_to: None,
        references: vec![],
        partial: false,
    }
}

fn text_body(t: &str) -> Body {
    Body {
        text: Some(t.into()),
        ..Body::default()
    }
}

fn put(m: &MessageMeta, body: Option<&Body>) -> Change {
    let b = match body {
        Some(b) => BodyChange::Put(Box::new(BodyRow::from_body(
            m.account.as_str(),
            m.id.as_str(),
            b,
        ))),
        None => BodyChange::Keep,
    };
    Change::message(MessageRow::from_meta(m), b)
}

async fn count(c: &tokio_postgres::Client, sql: &str) -> i64 {
    c.query_one(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get(0)
}

#[tokio::test]
async fn migrate_from_empty_then_again_changes_nothing() {
    let _g = SERIAL.lock().await;
    let Some(db) = TestDb::create("migrate").await else {
        return;
    };
    let mut admin = db.admin().await;

    // The dry run on an empty database plans everything and writes nothing.
    let plan = migrate::plan(&mut admin, &opts(1024)).await.unwrap();
    assert_eq!(plan.current, 0);
    assert_eq!(plan.pending.len(), 2);
    let text = plan.render();
    assert!(
        text.contains("applied version: 0")
            && text.contains("halfvec(1024)")
            && text.contains("to apply: 0001_init, 0002_embed_parse"),
        "{text}"
    );
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM pg_namespace WHERE nspname = 'mail'"
        )
        .await,
        0,
        "dry run wrote nothing"
    );

    let r = migrate::migrate(&mut admin, &opts(1024)).await.unwrap();
    assert_eq!(
        (r.from, r.to, r.applied.clone()),
        (
            0,
            2,
            vec!["0001_init".to_string(), "0002_embed_parse".to_string()]
        )
    );
    assert!(r.warnings.is_empty());
    for t in [
        "accounts",
        "mailboxes",
        "messages",
        "bodies",
        "attachments",
        "triage",
        "extracted",
        "parsed",
        "embeddings",
        "jobs",
        "job_stats",
        "schema_version",
    ] {
        let owner: String = admin
            .query_one("SELECT tableowner::text FROM pg_tables WHERE schemaname = 'mail' AND tablename = $1", &[&t])
            .await
            .unwrap_or_else(|e| panic!("mail.{t}: {e}"))
            .get(0);
        assert_eq!(owner, "mail_writer", "mail.{t}");
    }
    let schema_owner: String = admin
        .query_one(
            "SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname = 'mail'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(schema_owner, "mail_writer");
    let dim: i32 = admin
        .query_one("SELECT atttypmod FROM pg_attribute WHERE attrelid = 'mail.embeddings'::regclass AND attname = 'embedding'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(dim, 1024);
    let params: serde_json::Value = admin
        .query_one(
            "SELECT params FROM mail.schema_version WHERE version = 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(params["embedding_dim"], 1024);

    // Again: nothing to apply, nothing breaks, the grants are re-checked.
    let ty: String = admin
        .query_one("SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'mail.embeddings'::regclass AND attname = 'embedding'", &[])
        .await
        .unwrap()
        .get(0);
    assert!(ty.ends_with("halfvec(1024)"), "{ty}");
    let r = migrate::migrate(&mut admin, &opts(1024)).await.unwrap();
    assert_eq!((r.from, r.to), (2, 2));
    assert!(r.applied.is_empty());
    let plan = migrate::plan(&mut admin, &opts(1024)).await.unwrap();
    assert!(plan.pending.is_empty());
    assert!(plan.render().contains("no migrations to apply"));
    assert_eq!(
        count(&admin, "SELECT count(*) FROM mail.schema_version").await,
        2
    );
    db.drop_db().await;
}

#[tokio::test]
async fn the_schema_is_closed_to_postgrest_and_the_reader_only_reads() {
    let _g = SERIAL.lock().await;
    let Some(db) = TestDb::create("grants").await else {
        return;
    };
    let mut admin = db.admin().await;
    // Pretend to be Supabase: its API roles exist, and someone left a default grant that would hand
    // mail_writer's new tables to anon.
    for r in API_ROLES {
        admin
            .batch_execute(&format!(
                "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{r}') THEN CREATE ROLE {r} NOLOGIN; END IF; \
                 EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$;"
            ))
            .await
            .unwrap();
    }
    admin
        .batch_execute(
            "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mail_writer') THEN CREATE ROLE mail_writer LOGIN; END IF; \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$; \
             ALTER DEFAULT PRIVILEGES FOR ROLE mail_writer GRANT ALL ON TABLES TO anon, authenticated, service_role; \
             ALTER DEFAULT PRIVILEGES FOR ROLE mail_writer GRANT ALL ON FUNCTIONS TO anon;",
        )
        .await
        .unwrap();
    migrate::migrate(&mut admin, &opts(8)).await.unwrap();
    db.set_role_passwords().await;

    // No API role and not PUBLIC: no schema usage, no table, no function.
    for r in API_ROLES {
        let usage: bool = admin
            .query_one("SELECT has_schema_privilege($1, 'mail', 'USAGE')", &[r])
            .await
            .unwrap()
            .get(0);
        assert!(!usage, "{r} can use schema mail");
    }
    let acl_holders = |sql: &'static str| {
        let admin = &admin;
        async move {
            admin
                .query(sql, &[])
                .await
                .unwrap()
                .iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
                .collect::<Vec<_>>()
        }
    };
    let allowed = ["mail_writer", "mail_reader"];
    let tables = acl_holders(
        "SELECT c.relname::text, CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE a.grantee::regrole::text END
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace, aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
         WHERE n.nspname = 'mail' AND c.relkind IN ('r', 'v', 'S', 'p')",
    )
    .await;
    assert!(!tables.is_empty());
    for (t, who) in &tables {
        assert!(
            allowed.contains(&who.as_str()),
            "mail.{t} is granted to {who}"
        );
    }
    let functions = acl_holders(
        "SELECT p.proname::text, CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE a.grantee::regrole::text END
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace, aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a
         WHERE n.nspname = 'mail'",
    )
    .await;
    assert!(functions.iter().any(|(f, _)| f == "search"));
    for (f, who) in &functions {
        assert!(
            allowed.contains(&who.as_str()),
            "mail.{f}() is granted to {who}"
        );
    }
    let schema = acl_holders(
        "SELECT n.nspname::text, CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE a.grantee::regrole::text END
         FROM pg_namespace n, aclexplode(coalesce(n.nspacl, acldefault('n', n.nspowner))) a WHERE n.nspname = 'mail'",
    )
    .await;
    for (_, who) in &schema {
        assert!(
            allowed.contains(&who.as_str()),
            "schema mail is granted to {who}"
        );
    }

    // The reader reads and searches, and cannot write.
    let reader = lumbridge_mail_pg::connect::connect(
        &db.role_dsn("mail_reader"),
        &ConnectOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        count(&reader, "SELECT count(*) FROM mail.messages").await,
        0
    );
    assert_eq!(
        count(&reader, "SELECT count(*) FROM mail.search('anything')").await,
        0
    );
    let e = reader.batch_execute("INSERT INTO mail.accounts (id, persona, label, email, name, provider, ai_policy) VALUES ('x','work','x','x@x.example','x','demo','off')").await;
    assert!(e.is_err(), "the reader wrote");
    let e = reader
        .batch_execute("BEGIN READ WRITE; DELETE FROM mail.messages; COMMIT")
        .await;
    assert!(
        e.unwrap_err()
            .as_db_error()
            .is_some_and(|d| d.code().code() == "42501"),
        "permission denied for the reader"
    );
    // The writer writes.
    let writer = lumbridge_mail_pg::connect::connect(
        &db.role_dsn("mail_writer"),
        &ConnectOptions::default(),
    )
    .await
    .unwrap();
    writer
        .batch_execute("INSERT INTO mail.accounts (id, persona, label, email, name, provider, ai_policy) VALUES ('x','work','x','x@x.example','x','demo','off')")
        .await
        .unwrap();
    // An anon account is refused by the schema itself.
    let e = writer
        .batch_execute("INSERT INTO mail.accounts (id, persona, label, email, name, provider, ai_policy) VALUES ('y','anon','y','y@y.example','y','demo','off')")
        .await;
    assert!(e.is_err(), "an anon account row was accepted");
    drop((reader, writer));
    db.drop_db().await;
}

#[tokio::test]
async fn the_writer_round_trips_changes_idempotently() {
    let _g = SERIAL.lock().await;
    let Some(db) = migrated("writer", 8).await else {
        return;
    };
    let mut w = Writer::connect(&db.role_dsn("mail_writer"), &ConnectOptions::default())
        .await
        .unwrap();
    let admin = db.admin().await;
    let acct = AccountRow::from_account(&account("personal", Persona::Personal), false).unwrap();
    let inbox = MailboxRow::from_mailbox(&Mailbox {
        id: MailboxId::new("INBOX"),
        account: AccountId::new("personal"),
        name: "Inbox".into(),
        role: MailboxRole::Inbox,
        unread: 0,
        total: 0,
    });
    let m1 = meta(
        "personal",
        "m1",
        "t1",
        ("Ada Lovelace", "ada@example.com"),
        "Lunch on Friday?",
        "Shall we meet at noon",
        1_700_000_000,
    );
    let mut body = text_body("Shall we meet at noon at the usual place?\n\n-- \nAda");
    body.attachments.push(Attachment {
        id: AttachmentId::new("a1"),
        filename: "menu.pdf".into(),
        mime: "application/pdf".into(),
        size: 5000,
        content_id: None,
    });
    let triage = Triage {
        category: Category::Personal,
        priority: 60,
        summary: "Ada asks about lunch on Friday".into(),
        needs_reply: true,
        due: Some(1_700_100_000),
        by: "rules".into(),
        at: 1_700_000_100,
        suspicious: false,
    };
    let batch = vec![
        Change::UpsertMailbox(inbox.clone()),
        put(&m1, Some(&body)),
        Change::UpsertTriage(TriageRow::from_triage(
            "personal",
            &ThreadId::new("t1"),
            &triage,
            None,
        )),
        // Account last in the list: the writer still writes it first.
        Change::UpsertAccount(acct.clone()),
    ];
    w.apply(&batch).await.unwrap();
    let c = w.counts().await.unwrap();
    assert_eq!(
        (
            c.accounts,
            c.mailboxes,
            c.messages,
            c.bodies,
            c.attachments,
            c.triage,
            c.jobs
        ),
        (1, 1, 1, 1, 1, 1, 2)
    );
    let row = admin
        .query_one("SELECT subject, from_email, seen, extract(epoch FROM sent_at)::bigint, mailbox_ids, xmin::text FROM mail.messages WHERE id = 'm1'", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "Lunch on Friday?");
    assert_eq!(row.get::<_, String>(1), "ada@example.com");
    assert!(!row.get::<_, bool>(2));
    assert_eq!(row.get::<_, i64>(3), 1_700_000_000);
    assert_eq!(row.get::<_, Vec<String>>(4), vec!["INBOX".to_string()]);
    let xmin: String = row.get(5);
    let body_text: String = admin
        .query_one(
            "SELECT body_text FROM mail.bodies WHERE message_id = 'm1'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(body_text.contains("usual place"), "{body_text}");

    // The same batch again: no error, and unchanged rows are not rewritten.
    w.apply(&batch).await.unwrap();
    let xmin2: String = admin
        .query_one("SELECT xmin::text FROM mail.messages WHERE id = 'm1'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(xmin, xmin2, "an unchanged message was rewritten");
    assert_eq!(w.counts().await.unwrap(), c);

    // Read and archived: flags and membership change; the body stays.
    let mut m1b = m1.clone();
    m1b.flags.seen = true;
    m1b.mailboxes = vec![MailboxId::new("Archive")];
    w.apply(&[put(&m1b, None)]).await.unwrap();
    let row = admin
        .query_one(
            "SELECT seen, mailbox_ids FROM mail.messages WHERE id = 'm1'",
            &[],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>(0));
    assert_eq!(row.get::<_, Vec<String>>(1), vec!["Archive".to_string()]);
    assert_eq!(count(&admin, "SELECT count(*) FROM mail.bodies").await, 1);

    // Gone from the server: the message and everything hanging off it go.
    admin
        .batch_execute("INSERT INTO mail.parsed (account, message_id, kind, data, parser) VALUES ('personal', 'm1', 'receipt', '{\"total\": 12}', 'test')")
        .await
        .unwrap();
    admin
        .batch_execute("INSERT INTO mail.embeddings (account, message_id, model, content_hash, embedding) VALUES ('personal', 'm1', 'test-embed', 'h', '[1,0,0,0,0,0,0,0]')")
        .await
        .unwrap();
    w.apply(&[
        Change::DeleteMessage {
            account: "personal".into(),
            id: "m1".into(),
        },
        Change::DeleteTriage {
            account: "personal".into(),
            thread_id: "t1".into(),
        },
    ])
    .await
    .unwrap();
    let c = w.counts().await.unwrap();
    assert_eq!(
        (
            c.messages,
            c.bodies,
            c.attachments,
            c.parsed,
            c.embeddings,
            c.jobs,
            c.triage
        ),
        (0, 0, 0, 0, 0, 0, 0)
    );
    // Deleting what is not there is fine (idempotent).
    w.apply(&[Change::DeleteMessage {
        account: "personal".into(),
        id: "m1".into(),
    }])
    .await
    .unwrap();

    // Removing the account removes the rest.
    w.apply(&[put(&m1, Some(&body))]).await.unwrap();
    w.apply(&[Change::DeleteAccount {
        id: "personal".into(),
    }])
    .await
    .unwrap();
    let c = w.counts().await.unwrap();
    assert_eq!(
        (c.accounts, c.mailboxes, c.messages, c.bodies),
        (0, 0, 0, 0)
    );
    assert!(w.account_ids().await.unwrap().is_empty());
    drop(w);
    db.drop_db().await;
}

#[tokio::test]
async fn sensitive_mail_has_metadata_only_and_the_schema_enforces_it() {
    let _g = SERIAL.lock().await;
    let Some(db) = migrated("sensitive", 8).await else {
        return;
    };
    let mut w = Writer::connect(&db.role_dsn("mail_writer"), &ConnectOptions::default())
        .await
        .unwrap();
    let admin = db.admin().await;
    let acct = AccountRow::from_account(&account("personal", Persona::Personal), false).unwrap();
    // First seen as plain mail with a body (the code is only in the body)...
    let m = meta(
        "personal",
        "c1",
        "tc",
        ("Acme", "hello@acme.example"),
        "Hello",
        "see inside",
        1_700_000_000,
    );
    let body = text_body("Your code is 482913");
    w.apply(&[Change::UpsertAccount(acct), put(&m, Some(&body))])
        .await
        .unwrap();
    admin
        .batch_execute("INSERT INTO mail.parsed (account, message_id, kind, data, parser) VALUES ('personal', 'c1', 'x', '{}', 'test')")
        .await
        .unwrap();
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM mail.jobs WHERE message_id = 'c1'"
        )
        .await,
        2
    );
    // ...then judged sensitive: the content goes, the line stays.
    let row = MessageRow::sensitive(&m, "A security code", "A security code from Acme");
    w.apply(&[Change::message(
        row,
        BodyChange::Put(Box::new(BodyRow::from_body("personal", "c1", &body))),
    )])
    .await
    .unwrap();
    let r = admin
        .query_one(
            "SELECT subject, snippet, sensitive_kind FROM mail.messages WHERE id = 'c1'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(r.get::<_, String>(0), "A security code from Acme");
    assert_eq!(r.get::<_, String>(1), "");
    assert_eq!(
        r.get::<_, Option<String>>(2).as_deref(),
        Some("A security code")
    );
    for t in ["bodies", "attachments", "parsed", "embeddings", "jobs"] {
        assert_eq!(
            count(
                &admin,
                &format!("SELECT count(*) FROM mail.{t} WHERE message_id = 'c1'")
            )
            .await,
            0,
            "mail.{t}"
        );
    }
    // Nothing can add content to it afterwards, whoever tries.
    for sql in [
        "INSERT INTO mail.bodies (account, message_id, body_text, content_hash) VALUES ('personal', 'c1', 'x', 'h')",
        "INSERT INTO mail.parsed (account, message_id, kind, data, parser) VALUES ('personal', 'c1', 'x', '{}', 't')",
        "INSERT INTO mail.embeddings (account, message_id, model, content_hash, embedding) VALUES ('personal', 'c1', 'm', 'h', '[1,0,0,0,0,0,0,0]')",
        "INSERT INTO mail.jobs (kind, account, message_id) VALUES ('embed', 'personal', 'c1')",
        "INSERT INTO mail.extracted (account, thread_id, kind, text, by) VALUES ('personal', 'tc', 'action', 'x', 't')",
    ] {
        let e = admin.batch_execute(sql).await;
        assert!(e.is_err(), "accepted: {sql}");
    }
    drop(w);
    db.drop_db().await;
}

#[tokio::test]
async fn search_combines_full_text_fuzzy_and_vectors() {
    let _g = SERIAL.lock().await;
    let Some(db) = migrated("search", 4).await else {
        return;
    };
    let mut w = Writer::connect(&db.role_dsn("mail_writer"), &ConnectOptions::default())
        .await
        .unwrap();
    let mut batch = vec![
        Change::UpsertAccount(
            AccountRow::from_account(&account("personal", Persona::Personal), false).unwrap(),
        ),
        Change::UpsertAccount(
            AccountRow::from_account(&account("work", Persona::Work), false).unwrap(),
        ),
    ];
    let msgs = [
        (
            "personal",
            "p1",
            ("Ada Lovelace", "ada@example.com"),
            "Lunch on Friday?",
            "noon at the usual place",
            "Shall we meet for lunch?",
            1_700_000_000,
        ),
        (
            "personal",
            "p2",
            ("Shop", "orders@shop.example"),
            "Your order has shipped",
            "tracking inside",
            "Invoice 2291 for your order of three lamps.",
            1_700_100_000,
        ),
        (
            "work",
            "w1",
            ("Grace Hopper", "grace@work.example"),
            "Quarterly invoice review",
            "numbers attached",
            "Please check the invoices before Monday.",
            1_700_200_000,
        ),
        (
            "work",
            "w2",
            ("Linus", "linus@work.example"),
            "Kernel meeting notes",
            "see notes",
            "Notes from the meeting about schedulers.",
            1_600_000_000,
        ),
    ];
    for (a, id, from, subject, snippet, text, date) in msgs {
        let m = meta(a, id, &format!("t-{id}"), from, subject, snippet, date);
        batch.push(put(&m, Some(&text_body(text))));
    }
    w.apply(&batch).await.unwrap();
    let reader = lumbridge_mail_pg::connect::connect(
        &db.role_dsn("mail_reader"),
        &ConnectOptions::default(),
    )
    .await
    .unwrap();
    let ids = |rows: Vec<tokio_postgres::Row>| {
        rows.iter()
            .map(|r| r.get::<_, String>("message_id"))
            .collect::<Vec<_>>()
    };

    // Full text over subject and body (stemmed: "invoices" finds "invoice").
    let r = ids(reader
        .query("SELECT * FROM mail.search('invoice')", &[])
        .await
        .unwrap());
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0], "w1", "the subject hit ranks first");
    assert!(r.contains(&"p2".to_string()), "the body hit is found");
    // People, by name or address.
    assert_eq!(
        ids(reader
            .query("SELECT * FROM mail.search('lovelace')", &[])
            .await
            .unwrap()),
        ["p1"]
    );
    let r = ids(reader
        .query("SELECT * FROM mail.search('grace@work.example')", &[])
        .await
        .unwrap());
    assert_eq!(
        r.first().map(String::as_str),
        Some("w1"),
        "the exact address first (others on the domain may follow): {r:?}"
    );
    // A typo still finds the subject (trigram).
    let rows = reader
        .query(
            "SELECT message_id, matched FROM mail.search('quartrly')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.first().map(|r| r.get::<_, String>(0)).as_deref(),
        Some("w1")
    );
    assert_eq!(rows[0].get::<_, Vec<String>>(1), vec!["fuzzy".to_string()]);
    // Account and date filters.
    let only_work: Vec<String> = vec!["work".into()];
    assert_eq!(
        ids(reader
            .query("SELECT * FROM mail.search('invoice', $1)", &[&only_work])
            .await
            .unwrap()),
        ["w1"]
    );
    assert_eq!(
        ids(reader
            .query(
                "SELECT * FROM mail.search('kernel', NULL, 10, to_timestamp(1650000000))",
                &[]
            )
            .await
            .unwrap()),
        Vec::<String>::new(),
        "w2 is older than since"
    );
    // Web-search syntax: a phrase, and -exclusion.
    assert_eq!(
        ids(reader
            .query("SELECT * FROM mail.search('invoice -lamps')", &[])
            .await
            .unwrap()),
        ["w1"]
    );

    // Vectors (dimension 4 in this database): the nearest message joins the result.
    let admin = db.admin().await;
    admin
        .batch_execute(
            "INSERT INTO mail.embeddings (account, message_id, model, content_hash, embedding) VALUES
             ('personal', 'p1', 'test-embed', 'h1', '[1,0,0,0]'), ('work', 'w2', 'test-embed', 'h2', '[0,1,0,0]')",
        )
        .await
        .unwrap();
    let rows = reader
        .query("SELECT message_id, matched FROM mail.search('schedulers', NULL, 10, NULL, NULL, '[0,0.9,0.1,0]', 'test-embed')", &[])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, String>(0), "w2");
    assert_eq!(
        rows[0].get::<_, Vec<String>>(1),
        vec!["text".to_string(), "vector".to_string()],
        "found by both"
    );
    let rows = reader
        .query(
            "SELECT message_id FROM mail.search('', NULL, 10, NULL, NULL, '[1,0,0,0]')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.first().map(|r| r.get::<_, String>(0)).as_deref(),
        Some("p1"),
        "vector only"
    );
    // The thread view.
    let n: i64 = reader
        .query_one("SELECT count(*) FROM mail.threads", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 4);
    drop((w, reader));
    db.drop_db().await;
}

#[tokio::test]
async fn the_writer_refuses_a_database_without_the_schema() {
    let _g = SERIAL.lock().await;
    let Some(db) = TestDb::create("noschema").await else {
        return;
    };
    match Writer::connect(&db.admin_dsn(), &ConnectOptions::default()).await {
        Err(Error::Refused(m)) => assert!(m.contains("lbmail pg migrate"), "{m}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("connected to a database without the schema"),
    }
    let e = Writer::connect(&db.dead_dsn(), &ConnectOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::Offline(_)), "{e}");
    db.drop_db().await;
}

/// Not a pass/fail test: how fast the writer puts made-up mail into Postgres, in batches of 500
/// (metadata alone, then metadata with a body), and how fast an unchanged resend is.
#[tokio::test]
async fn writer_throughput_on_made_up_mail() {
    let _g = SERIAL.lock().await;
    let Some(db) = migrated("bench", 8).await else {
        return;
    };
    let mut w = Writer::connect(&db.role_dsn("mail_writer"), &ConnectOptions::default())
        .await
        .unwrap();
    w.apply(&[Change::UpsertAccount(
        AccountRow::from_account(&account("personal", Persona::Personal), false).unwrap(),
    )])
    .await
    .unwrap();
    let total = 10_000usize;
    let text = "Hello Sam,\n\nThis is made-up text for a throughput test, about invoices, lunches and lamps. ".repeat(8);
    for (label, with_body) in [
        ("metadata", false),
        ("metadata + body", true),
        ("unchanged resend", true),
    ] {
        let t = std::time::Instant::now();
        for chunk in (0..total).collect::<Vec<_>>().chunks(500) {
            let batch: Vec<Change> = chunk
                .iter()
                .map(|i| {
                    let m = meta(
                        "personal",
                        &format!("b{i}"),
                        &format!("t{}", i / 3),
                        ("Sender", "sender@bench.example"),
                        &format!("Made-up message {i}"),
                        "snippet",
                        1_700_000_000 + *i as i64,
                    );
                    put(&m, with_body.then(|| text_body(&text)).as_ref())
                })
                .collect();
            w.apply(&batch).await.unwrap();
        }
        let s = t.elapsed().as_secs_f64();
        eprintln!(
            "bench: writer, {label}: {total} messages in {s:.2} s = {:.0}/s",
            total as f64 / s
        );
    }
    drop(w);
    db.drop_db().await;
}
