//! The daemon keeps its backends gentle: pause stops every provider call (and survives a restart),
//! quit stops the daemon and leaves a "do not start" marker, background triage fetches bodies only on
//! the way to a model and within its budget, gog accounts go through the process budget (and whole
//! threads come back from one body fetch), backfill pages are stored without announcing them, and
//! removed accounts keep their mail until purged. Made-up mail only; no real gog.

use lumbridge_mail_ai::ScriptedModel;
use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_mail_gog::fake::FakeRunner;
use lumbridge_maild::{Client, Daemon, DemoProvider, DemoSet, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

macro_rules! eventually {
    ($what:expr, $cond:expr) => {{
        let start = Instant::now();
        loop {
            if $cond {
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("timed out waiting for: {}", $what);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }};
}

fn home(config: &str) -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path().to_path_buf();
    let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| h.clone().into_os_string()));
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(paths.config_file(), config).unwrap();
    (dir, paths)
}

async fn person(paths: &Paths) -> Client {
    let token = Client::read_person_token(&paths.person_token()).expect("token file");
    Client::connect(&paths.socket(), Some(token), "test person").await.unwrap()
}

async fn agent(paths: &Paths) -> Client {
    Client::connect(&paths.socket(), None, "test agent").await.unwrap()
}

async fn statuses(c: &mut Client) -> Vec<AccountStatus> {
    c.call(method::STATUS, json!({})).await.unwrap()
}

async fn threads(c: &mut Client, q: Query) -> Vec<ThreadSummary> {
    let p: ThreadPage = c.call(method::THREADS, &q).await.unwrap();
    p.threads
}

fn only(a: &str) -> Query {
    Query { accounts: Some(vec![AccountId::new(a)]), limit: 100, ..Query::default() }
}

fn demo_account(id: &str, extra: &str) -> String {
    format!("[[accounts]]\nid = \"{id}\"\npersona = \"personal\"\nemail = \"{id}@example.com\"\nname = \"Sam Example\"\n{extra}[accounts.provider]\nkind = \"demo\"\nset = \"personal\"\n")
}

/// A provider that counts what it is asked, around another one.
struct Counting {
    inner: Arc<dyn Provider>,
    syncs: AtomicU32,
    bodies: Mutex<Vec<MessageId>>,
}

impl Counting {
    fn new(inner: Arc<dyn Provider>) -> Arc<Self> {
        Arc::new(Self { inner, syncs: AtomicU32::new(0), bodies: Mutex::new(Vec::new()) })
    }
}

#[async_trait::async_trait]
impl Provider for Counting {
    fn account(&self) -> &AccountId {
        self.inner.account()
    }
    async fn sync(&self, c: Option<&SyncCursor>) -> Result<Delta> {
        self.syncs.fetch_add(1, Ordering::SeqCst);
        self.inner.sync(c).await
    }
    async fn fetch_body(&self, id: &MessageId) -> Result<Body> {
        self.bodies.lock().unwrap().push(id.clone());
        self.inner.fetch_body(id).await
    }
    async fn fetch_raw(&self, id: &MessageId) -> Result<Vec<u8>> {
        self.inner.fetch_raw(id).await
    }
    async fn fetch_attachment(&self, m: &MessageId, a: &AttachmentId) -> Result<Vec<u8>> {
        self.inner.fetch_attachment(m, a).await
    }
    async fn apply(&self, a: &Action) -> Result<ApplyOutcome> {
        self.inner.apply(a).await
    }
    async fn wait_for_change(&self, t: u64) -> Result<()> {
        self.inner.wait_for_change(t.min(1)).await
    }
}

fn demo(id: &str) -> Arc<dyn Provider> {
    Arc::new(DemoProvider::new(AccountId::new(id), DemoSet::Personal, Address::new(Some("Sam Example"), &format!("{id}@example.com"))))
}

async fn start(paths: &Paths, providers: &[(&str, Arc<dyn Provider>)], model: Option<Arc<ScriptedModel>>, runner: Option<Arc<FakeRunner>>) -> Daemon {
    let mut o = Options::new(paths.clone());
    for (id, p) in providers {
        o.providers.insert(AccountId::new(*id), p.clone());
    }
    o.model = model.map(|m| m as Arc<dyn lumbridge_mail_ai::Model>);
    o.gog_runner = runner.map(|r| r as Arc<dyn lumbridge_mail_gog::Runner>);
    Daemon::start(o).await.unwrap()
}

#[tokio::test]
async fn pause_stops_every_provider_call_and_survives_a_restart() {
    let (_dir, paths) = home(&format!("[daemon]\nsync_interval_s = 1\ntriage = false\n\n{}", demo_account("personal", "")));
    let p = Counting::new(demo("personal"));
    let d = start(&paths, &[("personal", p.clone())], None, None).await;
    let mut c = person(&paths).await;
    eventually!("synced", !threads(&mut c, only("personal")).await.is_empty() && p.syncs.load(Ordering::SeqCst) >= 2);

    let info: DaemonInfo = c.call(method::DAEMON_PAUSE, json!({})).await.unwrap();
    assert!(info.paused);
    assert!(paths.paused_file().exists(), "persisted");
    eventually!("status says paused", statuses(&mut c).await.iter().all(|s| s.paused));
    let before = p.syncs.load(Ordering::SeqCst);
    let _: Value = c.call(method::SYNC, json!({})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(p.syncs.load(Ordering::SeqCst), before, "no sync while paused, not even on sync.now");
    // Bodies not cached yet cannot be fetched either.
    let t = threads(&mut c, only("personal")).await.remove(0);
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: t.account.clone(), thread: t.id.clone() }).await.unwrap();
    let e = c.call::<_, Body>(method::BODY, MessageRef { account: t.account.clone(), message: msgs[0].id.clone() }).await.unwrap_err();
    assert!(matches!(e, Error::Offline(_)) && e.to_string().contains("paused"), "{e}");
    assert!(p.bodies.lock().unwrap().is_empty());
    // Actions wait (queued locally), they are not sent.
    drop(c);
    d.shutdown().await;

    // A restart keeps the pause.
    let p2 = Counting::new(demo("personal"));
    let d = start(&paths, &[("personal", p2.clone())], None, None).await;
    assert!(d.is_paused());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(p2.syncs.load(Ordering::SeqCst), 0);
    let mut c = person(&paths).await;
    let info: DaemonInfo = c.call(method::DAEMON_RESUME, json!({})).await.unwrap();
    assert!(!info.paused && !paths.paused_file().exists());
    eventually!("syncing again", p2.syncs.load(Ordering::SeqCst) > 0);
    drop(c);
    d.shutdown().await;
}

#[tokio::test]
async fn quit_stops_the_daemon_and_marks_it_stopped_until_started_by_hand() {
    let (_dir, paths) = home(&demo_account("personal", ""));
    let d = start(&paths, &[("personal", demo("personal"))], None, None).await;
    let running = tokio::spawn(d.run_until_signal());
    let mut c = agent(&paths).await;
    let _: Value = c.call(method::DAEMON_QUIT, json!({})).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), running).await.expect("the daemon stops").unwrap().unwrap();
    assert!(paths.stopped_marker().exists(), "clients must not start it again");
    assert!(!paths.socket().exists());
    // Started on purpose: the marker goes.
    let d = start(&paths, &[("personal", demo("personal"))], None, None).await;
    assert!(!paths.stopped_marker().exists());
    // Stopping right after starting is quick too (the loops see the stop even before they first run).
    let t = Instant::now();
    d.shutdown().await;
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

/// Mail for the triage test: two promotions (Gmail's category label), one newsletter by its sender,
/// three people writing.
struct Inbox {
    account: AccountId,
    bodies: Mutex<Vec<String>>,
}

fn meta(account: &AccountId, n: u32, from: &str, subject: &str, boxes: &[&str]) -> MessageMeta {
    MessageMeta {
        id: MessageId::new(format!("m{n}")),
        account: account.clone(),
        thread: ThreadId::new(format!("t{n}")),
        mailboxes: boxes.iter().map(|b| MailboxId::new(*b)).collect(),
        from: vec![Address::new(Some("Someone"), from)],
        to: vec![Address::new(Some("Sam Example"), "sam@example.com")],
        cc: vec![],
        bcc: vec![],
        subject: subject.into(),
        snippet: format!("Made-up snippet {n}"),
        date: 1_700_000_000 + i64::from(n),
        flags: Flags::default(),
        has_attachments: false,
        size: 100,
        rfc_message_id: Some(format!("m{n}@mail.example.org")),
        in_reply_to: None,
        references: vec![],
        partial: false,
    }
}

#[async_trait::async_trait]
impl Provider for Inbox {
    fn account(&self) -> &AccountId {
        &self.account
    }
    async fn sync(&self, _: Option<&SyncCursor>) -> Result<Delta> {
        let a = &self.account;
        Ok(Delta {
            mailboxes: Some(vec![Mailbox { id: MailboxId::new("INBOX"), account: a.clone(), name: "Inbox".into(), role: MailboxRole::Inbox, unread: 0, total: 0 }]),
            upserted: vec![
                meta(a, 1, "deals@shop.example", "Your weekend", &["INBOX", "CATEGORY_PROMOTIONS"]),
                meta(a, 2, "hello@store.example", "Fresh picks", &["INBOX", "CATEGORY_PROMOTIONS"]),
                meta(a, 3, "noreply@news.example", "This week", &["INBOX"]),
                meta(a, 4, "ada@example.org", "Lunch?", &["INBOX"]),
                meta(a, 5, "grace@example.org", "The plan", &["INBOX"]),
                meta(a, 6, "maya@example.org", "Dinner", &["INBOX"]),
            ],
            cursor: Some(SyncCursor("c1".into())),
            ..Delta::default()
        })
    }
    async fn fetch_body(&self, id: &MessageId) -> Result<Body> {
        self.bodies.lock().unwrap().push(id.0.clone());
        Ok(Body { text: Some(format!("Made-up body of {id}")), ..Body::default() })
    }
    async fn fetch_raw(&self, _: &MessageId) -> Result<Vec<u8>> {
        Err(Error::Unsupported("test".into()))
    }
    async fn fetch_attachment(&self, _: &MessageId, _: &AttachmentId) -> Result<Vec<u8>> {
        Err(Error::Unsupported("test".into()))
    }
    async fn apply(&self, _: &Action) -> Result<ApplyOutcome> {
        Ok(ApplyOutcome::default())
    }
    async fn wait_for_change(&self, t: u64) -> Result<()> {
        tokio::time::sleep(Duration::from_secs(t)).await;
        Ok(())
    }
}

#[tokio::test]
async fn triage_fetches_bodies_only_for_the_model_and_within_its_budget() {
    let (_dir, paths) = home(&format!("[daemon]\ntriage_bodies_per_min = 2\n\n{}", demo_account("personal", "")));
    let inbox = Arc::new(Inbox { account: AccountId::new("personal"), bodies: Mutex::new(Vec::new()) });
    let model = Arc::new(ScriptedModel::with(|_| Ok(json!({"category": "personal", "priority": 50, "summary": "A note", "needs_reply": false, "due": null}).to_string())));
    let d = start(&paths, &[("personal", inbox.clone() as Arc<dyn Provider>)], Some(model.clone()), None).await;
    let mut c = person(&paths).await;
    eventually!("everything triaged", {
        let t = threads(&mut c, only("personal")).await;
        t.len() == 6 && t.iter().all(|t| t.triage.is_some())
    });
    let by: HashMap<String, (Category, String)> = threads(&mut c, only("personal")).await.into_iter().map(|t| {
        let tr = t.triage.unwrap();
        (t.id.0, (tr.category, tr.by))
    }).collect();
    assert_eq!(by["t1"].0, Category::Promotion, "Gmail's category label is a rule");
    assert_eq!(by["t2"].0, Category::Promotion);
    assert_eq!(by["t3"].0, Category::Notification);
    let mut fetched = inbox.bodies.lock().unwrap().clone();
    fetched.sort();
    assert_eq!(fetched, vec!["m5", "m6"], "2 per minute, the newest mail first");
    assert!(!fetched.iter().any(|m| ["m1", "m2", "m3"].contains(&m.as_str())), "never for mail the rules classified");
    assert_eq!(model.requests().len(), 3, "the three people, one of them on its snippet");
    drop(c);
    d.shutdown().await;
}

fn gog_config(extra: &str) -> String {
    format!(
        "[daemon]\nsync_interval_s = 600\ntriage = false\n\n[[accounts]]\nid = \"g\"\npersona = \"personal\"\nemail = \"test.user@example.com\"\n\
         name = \"Test User\"\nreadonly = true\n[accounts.provider]\nkind = \"gog\"\nbinary = \"/nonexistent/gog\"\n{extra}"
    )
}

fn row(n: usize, thread: usize) -> Value {
    json!({"id": format!("19b00000000000{n:02}"), "threadId": format!("19b00000000000{thread:02}"), "date": "2026-11-09 14:00",
           "internalDateIso": format!("2026-11-09T14:{n:02}:00Z"), "from": format!("Sender {n} <s{n}@example.org>"),
           "to": "Test User <test.user@example.com>", "subject": format!("Made-up {n}"), "labels": ["INBOX", "UNREAD"],
           "headers": {"Message-Id": format!("<g{n}@mail.example.org>")}, "body": format!("Made-up body {n}")})
}

fn full(n: usize, thread: usize, text: &str) -> Value {
    use base64::Engine;
    json!({"id": format!("19b00000000000{n:02}"), "threadId": format!("19b00000000000{thread:02}"), "labelIds": ["INBOX"],
           "historyId": "900", "internalDate": "1794232800000", "snippet": text,
           "payload": {"mimeType": "text/plain", "headers": [{"name": "From", "value": format!("s{n}@example.org")}, {"name": "Subject", "value": format!("Made-up {n}")}],
                       "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)}}})
}

#[tokio::test]
async fn a_gog_account_syncs_through_the_budget_with_a_handful_of_processes() {
    let (_dir, paths) = home(&gog_config("window_days = 30\n"));
    let f = Arc::new(FakeRunner::new());
    f.on_json("version", &[], json!({"version": "v0.48.0"}));
    f.on_json("gmail.labels.list", &[], json!({"labels": [{"id": "INBOX", "name": "INBOX", "type": "system"}, {"id": "UNREAD", "name": "UNREAD", "type": "system"}]}));
    f.on_json("gmail.messages.search", &["in:anywhere newer_than:30d"], json!({"messages": [row(3, 2), row(2, 2), row(1, 1)], "nextPageToken": ""}));
    f.on_json("gmail.raw", &["19b0000000000003", "minimal"], json!({"id": "19b0000000000003", "historyId": "900"}));
    f.on_json("gmail.history", &["--since", "900", "1"], json!({"historyId": "900", "messages": [], "nextPageToken": ""}));
    let d = start(&paths, &[], None, Some(f.clone())).await;
    let mut c = person(&paths).await;
    eventually!("synced", threads(&mut c, only("g")).await.len() == 2);
    // 3 messages: label list, version, one listing, one resume point, one probe.
    eventually!("first sync done", f.calls().len() == 5);
    assert_eq!(f.commands(), vec!["gmail.labels.list", "version", "gmail.messages.search", "gmail.raw", "gmail.history"]);
    let info: DaemonInfo = c.call(method::DAEMON_INFO, json!({})).await.unwrap();
    assert_eq!(info.load.len(), 1);
    assert_eq!((info.load[0].scope.as_str(), info.load[0].starts_total, info.load[0].max_in_flight), ("local", 5, 3));

    // Opening one message of thread 2 fetches the thread once; its sibling's body is then cached.
    f.on_json("gmail.thread.get", &["19b0000000000002"], json!({"thread": {"id": "19b0000000000002", "messages": [full(2, 2, "second"), full(3, 2, "third")]}}));
    let b: Body = c.call(method::BODY, MessageRef { account: AccountId::new("g"), message: MessageId::new("19b0000000000003") }).await.unwrap();
    assert_eq!(b.text.as_deref(), Some("third"));
    let b: Body = c.call(method::BODY, MessageRef { account: AccountId::new("g"), message: MessageId::new("19b0000000000002") }).await.unwrap();
    assert_eq!(b.text.as_deref(), Some("second"));
    assert_eq!(f.calls_to("gmail.thread.get").len(), 1);
    assert_eq!(f.calls().len(), 6, "two bodies, one process");

    // Backfill: one incremental round, then one page of older mail, stored but never announced.
    f.on_json("gmail.history", &["--since", "900", "100"], json!({"historyId": "900", "messages": [], "nextPageToken": ""}));
    f.on_json("gmail.messages.search", &["in:anywhere newer_than:7d"], json!({"messages": [], "nextPageToken": ""}));
    let floor = 1_794_232_860; // the oldest listed message: row 1 at 14:01
    let mut old = row(40, 40);
    old["internalDateIso"] = json!("2026-01-01T00:00:00Z");
    f.on_json("gmail.messages.search", &[&format!("in:anywhere before:{floor}")], json!({"messages": [old], "nextPageToken": ""}));
    let mut tail = agent(&paths).await;
    let v: Value = c.call(method::SYNC_BACKFILL, BackfillParams { account: Some(AccountId::new("g")), since: None }).await.unwrap();
    assert_eq!(v["accounts"], 1);
    eventually!("the old message is stored", threads(&mut c, only("g")).await.len() == 3);
    let mut new_mail = 0;
    while let Some(e) = tail.next_event(Duration::from_millis(300)).await {
        if matches!(e, Event::NewMail { .. }) {
            new_mail += 1;
        }
    }
    assert_eq!(new_mail, 0, "backfilled mail is not new mail");
    drop((c, tail));
    d.shutdown().await;
}

#[tokio::test]
async fn removed_accounts_keep_their_mail_until_a_person_purges_it() {
    let both = format!("[daemon]\ntriage = false\n\n{}{}", demo_account("personal", ""), demo_account("old", ""));
    let (_dir, paths) = home(&both);
    let d = start(&paths, &[("personal", demo("personal")), ("old", demo("old"))], None, None).await;
    let mut c = person(&paths).await;
    eventually!("both synced", !threads(&mut c, only("old")).await.is_empty() && !threads(&mut c, only("personal")).await.is_empty());
    std::fs::write(paths.config_file(), format!("[daemon]\ntriage = false\n\n{}", demo_account("personal", ""))).unwrap();
    let _: Value = c.call(method::CONFIG_RELOAD, json!({})).await.unwrap();
    let info: DaemonInfo = c.call(method::DAEMON_INFO, json!({})).await.unwrap();
    assert_eq!(info.kept_accounts, vec![AccountId::new("old")]);
    let all = threads(&mut c, Query { limit: 200, ..Query::default() }).await;
    assert!(all.iter().all(|t| t.account.as_str() == "personal"), "kept mail is out of the unified inbox");
    // Only a person may purge, and only accounts that left the config.
    let mut a = agent(&paths).await;
    assert!(matches!(a.call::<_, Value>(method::ACCOUNTS_PURGE, json!({"account": "old"})).await, Err(Error::Refused(_))));
    assert!(matches!(c.call::<_, Value>(method::ACCOUNTS_PURGE, json!({"account": "personal"})).await, Err(Error::Invalid(_))));
    let v: Value = c.call(method::ACCOUNTS_PURGE, json!({"account": "old"})).await.unwrap();
    assert_eq!(v["purged"], true);
    let info: DaemonInfo = c.call(method::DAEMON_INFO, json!({})).await.unwrap();
    assert!(info.kept_accounts.is_empty());
    assert!(matches!(c.call::<_, Value>(method::ACCOUNTS_PURGE, json!({"account": "old"})).await, Err(Error::NotFound(_))));
    drop((c, a));
    d.shutdown().await;
}

/// A gog stand-in whose listings take a while, and which counts the ones that ran to the end.
struct SlowGog {
    fake: Arc<FakeRunner>,
    started: AtomicU32,
    finished: AtomicU32,
}

#[async_trait::async_trait]
impl lumbridge_mail_gog::Runner for SlowGog {
    async fn run(&self, call: lumbridge_mail_gog::GogCall) -> Result<lumbridge_mail_gog::GogOutput> {
        if call.command() == Some("gmail.messages.search") {
            self.started.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(1500)).await;
            self.finished.fetch_add(1, Ordering::SeqCst);
        }
        self.fake.run(call).await
    }
}

#[tokio::test]
async fn a_config_reload_does_not_kill_gog_calls_in_flight() {
    let (_dir, paths) = home(&gog_config("window_days = 30\n"));
    let f = Arc::new(FakeRunner::new());
    f.on_json("version", &[], json!({"version": "v0.48.0"}));
    f.on_json("gmail.labels.list", &[], json!({"labels": [{"id": "INBOX", "name": "INBOX", "type": "system"}]}));
    f.on_json("gmail.messages.search", &[], json!({"messages": [], "nextPageToken": ""}));
    let slow = Arc::new(SlowGog { fake: f.clone(), started: AtomicU32::new(0), finished: AtomicU32::new(0) });
    let mut o = Options::new(paths.clone());
    o.gog_runner = Some(slow.clone());
    let d = Daemon::start(o).await.unwrap();
    let mut c = person(&paths).await;
    eventually!("a listing is running", slow.started.load(Ordering::SeqCst) == 1);
    // The account's entry changes: it restarts while its listing is still running.
    std::fs::write(paths.config_file(), gog_config("window_days = 20\n")).unwrap();
    let _: Value = c.call(method::CONFIG_RELOAD, json!({})).await.unwrap();
    eventually!("the old listing ran to its end", slow.finished.load(Ordering::SeqCst) >= 1);
    // And the restarted account lists with its new window.
    eventually!("new window", f.calls_to("gmail.messages.search").iter().any(|c| c.display_args().contains(&"in:anywhere newer_than:20d".to_owned())));
    drop(c);
    d.shutdown().await;
}
