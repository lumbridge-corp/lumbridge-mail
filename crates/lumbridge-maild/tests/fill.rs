//! The background body fill (`[daemon] body_fill`): newest first, one fetch at a time per account, never
//! faster than its own rate nor than the provider's budget allows, out of the way of everything else,
//! stopped by pause, carried across a restart, per-account opt-out, and `sync.bodies`. Made-up mail
//! only; no real gog.

use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_mail_gog::fake::FakeRunner;
use lumbridge_maild::{Client, Daemon, Options};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

async fn fill_of(c: &mut Client, account: &str) -> BodyFillStatus {
    let st: Vec<AccountStatus> = c.call(method::STATUS, json!({})).await.unwrap();
    st.into_iter().find(|s| s.account.as_str() == account).and_then(|s| s.body_fill).expect("body_fill in status")
}

/// `[daemon]` for these tests: triage off (it fetches bodies of its own), a long sync interval (each
/// sync makes the account busy for a moment, which the fill waits out).
fn config(daemon: &str, accounts: &[(&str, &str)]) -> String {
    let mut s = format!("[daemon]\nsync_interval_s = 600\ntriage = false\n{daemon}\n");
    for (id, extra) in accounts {
        s.push_str(&format!(
            "[[accounts]]\nid = \"{id}\"\npersona = \"personal\"\nemail = \"{id}@example.com\"\nname = \"Sam Example\"\n{extra}\n[accounts.provider]\nkind = \"demo\"\n\n"
        ));
    }
    s
}

#[derive(Debug, Clone)]
struct Fetch {
    id: String,
    start: Instant,
    end: Instant,
}

/// A provider of `n` one-message threads (m1 oldest .. mN newest, plus a draft), recording every body
/// fetch: when it started and ended, and the most at once.
struct Bag {
    account: AccountId,
    metas: Vec<MessageMeta>,
    delay: Duration,
    gone: HashSet<String>,
    fetches: Mutex<Vec<Fetch>>,
    in_flight: AtomicU32,
    most: AtomicU32,
    offline: AtomicBool,
}

fn meta(account: &AccountId, n: u32, draft: bool) -> MessageMeta {
    MessageMeta {
        id: MessageId::new(format!("m{n}")),
        account: account.clone(),
        thread: ThreadId::new(format!("t{n}")),
        mailboxes: vec![MailboxId::new(if draft { "DRAFT" } else { "INBOX" })],
        from: vec![Address::new(Some("Someone"), &format!("person{n}@example.org"))],
        to: vec![Address::new(Some("Sam Example"), &format!("{account}@example.com"))],
        cc: vec![],
        bcc: vec![],
        subject: format!("Made-up subject {n}"),
        snippet: format!("Made-up snippet {n}"),
        date: 1_700_000_000 + i64::from(n) * 60,
        flags: Flags { draft, ..Flags::default() },
        has_attachments: false,
        size: 100,
        rfc_message_id: Some(format!("m{n}@mail.example.org")),
        in_reply_to: None,
        references: vec![],
        partial: false,
    }
}

impl Bag {
    fn new(account: &str, n: u32, delay: Duration) -> Arc<Self> {
        let account = AccountId::new(account);
        let mut metas: Vec<MessageMeta> = (1..=n).map(|i| meta(&account, i, false)).collect();
        metas.push(meta(&account, 999, true));
        Arc::new(Self {
            account,
            metas,
            delay,
            gone: HashSet::new(),
            fetches: Mutex::new(Vec::new()),
            in_flight: AtomicU32::new(0),
            most: AtomicU32::new(0),
            offline: AtomicBool::new(false),
        })
    }
    fn with_gone(self: Arc<Self>, ids: &[&str]) -> Arc<Self> {
        let mut b = Arc::try_unwrap(self).ok().expect("not shared yet");
        b.gone = ids.iter().map(|s| s.to_string()).collect();
        Arc::new(b)
    }
    fn fetched(&self) -> Vec<String> {
        self.fetches.lock().unwrap().iter().map(|f| f.id.clone()).collect()
    }
}

#[async_trait::async_trait]
impl Provider for Bag {
    fn account(&self) -> &AccountId {
        &self.account
    }
    async fn sync(&self, _: Option<&SyncCursor>) -> Result<Delta> {
        if self.offline.load(Ordering::SeqCst) {
            return Err(Error::Offline("made-up outage".into()));
        }
        let a = &self.account;
        Ok(Delta {
            mailboxes: Some(vec![
                Mailbox { id: MailboxId::new("INBOX"), account: a.clone(), name: "Inbox".into(), role: MailboxRole::Inbox, unread: 0, total: 0 },
                Mailbox { id: MailboxId::new("DRAFT"), account: a.clone(), name: "Drafts".into(), role: MailboxRole::Drafts, unread: 0, total: 0 },
            ]),
            upserted: self.metas.clone(),
            cursor: Some(SyncCursor("c1".into())),
            ..Delta::default()
        })
    }
    async fn fetch_body(&self, id: &MessageId) -> Result<Body> {
        let start = Instant::now();
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.fetches.lock().unwrap().push(Fetch { id: id.0.clone(), start, end: Instant::now() });
        if self.offline.load(Ordering::SeqCst) {
            return Err(Error::Offline("made-up outage".into()));
        }
        if self.gone.contains(id.as_str()) {
            return Err(Error::NotFound(format!("message {id}")));
        }
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

async fn start(paths: &Paths, providers: &[(&str, Arc<dyn Provider>)]) -> Daemon {
    let mut o = Options::new(paths.clone());
    for (id, p) in providers {
        o.providers.insert(AccountId::new(*id), p.clone());
    }
    Daemon::start(o).await.unwrap()
}

#[tokio::test]
async fn the_fill_fetches_every_body_newest_first_and_leaves_drafts_alone() {
    let (_dir, paths) = home(&config("body_fill = true\nbody_fill_per_min = 600\n", &[("personal", "")]));
    let bag = Bag::new("personal", 8, Duration::ZERO);
    let d = start(&paths, &[("personal", bag.clone() as Arc<dyn Provider>)]).await;
    let mut c = person(&paths).await;
    eventually!("every body fetched", {
        let f = fill_of(&mut c, "personal").await;
        f.total == 8 && f.have == 8
    });
    assert_eq!(bag.fetched(), (1..=8).rev().map(|i| format!("m{i}")).collect::<Vec<_>>(), "newest first, each once");
    let f = fill_of(&mut c, "personal").await;
    assert!(f.on && f.per_min == 600 && f.last_hour == 8, "{f:?}");
    eventually!("done", fill_of(&mut c, "personal").await.done);
    // The bodies are the store's now: opening one costs no fetch.
    let b: Body = c.call(method::BODY, MessageRef { account: AccountId::new("personal"), message: MessageId::new("m3") }).await.unwrap();
    assert_eq!(b.text.as_deref(), Some("Made-up body of m3"));
    assert_eq!(bag.fetched().len(), 8, "drafts are never fetched, nothing twice");
    drop(c);
    d.shutdown().await;
}

#[tokio::test]
async fn the_fill_keeps_to_its_rate_one_fetch_at_a_time() {
    // 120 a minute: one every 0.5 s. Fetches take 0.3 s, so a second one could overlap if allowed.
    let (_dir, paths) = home(&config("body_fill = true\nbody_fill_per_min = 120\n", &[("personal", "")]));
    let bag = Bag::new("personal", 40, Duration::from_millis(300));
    let d = start(&paths, &[("personal", bag.clone() as Arc<dyn Provider>)]).await;
    let c = person(&paths).await;
    eventually!("the fill started", !bag.fetched().is_empty());
    let first = bag.fetches.lock().unwrap()[0].start;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let f: Vec<Fetch> = bag.fetches.lock().unwrap().clone();
    let window = f.last().unwrap().start.duration_since(first);
    assert!(f.len() >= 3, "it does fill: {}", f.len());
    assert!(f.len() as f64 <= window.as_secs_f64() * 2.0 + 1.0, "{} fetches in {window:?}: faster than 120/min", f.len());
    for w in f.windows(2) {
        let gap = w[1].start.duration_since(w[0].start);
        assert!(gap >= Duration::from_millis(490), "fetches {gap:?} apart");
        assert!(w[1].start >= w[0].end, "one at a time");
    }
    assert_eq!(bag.most.load(Ordering::SeqCst), 1, "never two at once");
    drop(c);
    d.shutdown().await;
}

#[tokio::test]
async fn a_person_opening_mail_comes_first_and_pause_stops_the_fill() {
    let (_dir, paths) = home(&config("body_fill = true\nbody_fill_per_min = 600\n", &[("personal", "")]));
    let bag = Bag::new("personal", 60, Duration::from_millis(100));
    let d = start(&paths, &[("personal", bag.clone() as Arc<dyn Provider>)]).await;
    let mut c = person(&paths).await;
    eventually!("the fill started", bag.fetched().len() >= 2);
    // The oldest message is the fill's last: a person opens it now.
    let asked = Instant::now();
    let _: Body = c.call(method::BODY, MessageRef { account: AccountId::new("personal"), message: MessageId::new("m1") }).await.unwrap();
    let served = Instant::now();
    let f: Vec<Fetch> = bag.fetches.lock().unwrap().clone();
    let open = f.iter().find(|x| x.id == "m1").expect("fetched for the person").clone();
    assert!(open.start.duration_since(asked) < Duration::from_millis(400), "served at once, not queued behind the fill");
    tokio::time::sleep(Duration::from_millis(4000)).await;
    let after: Vec<Fetch> = bag.fetches.lock().unwrap().iter().filter(|x| x.start > served && x.id != "m1").cloned().collect();
    assert!(!after.is_empty(), "the fill goes on afterwards");
    assert!(after[0].start.duration_since(open.end) >= Duration::from_millis(1900), "it waits until the account is quiet");
    let st = fill_of(&mut c, "personal").await;
    assert!(st.have < st.total);

    // Pause: no fetch at all until resumed.
    let _: DaemonInfo = c.call(method::DAEMON_PAUSE, json!({})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n = bag.fetched().len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(bag.fetched().len(), n, "paused");
    assert!(fill_of(&mut c, "personal").await.waiting.unwrap_or_default().contains("paused"));
    let _: DaemonInfo = c.call(method::DAEMON_RESUME, json!({})).await.unwrap();
    eventually!("going again", bag.fetched().len() > n);
    drop(c);
    d.shutdown().await;
}

#[tokio::test]
async fn the_fill_waits_out_an_outage_and_sets_aside_what_the_server_lost() {
    let (_dir, paths) = home(&config("body_fill = true\nbody_fill_per_min = 600\n", &[("personal", "")]));
    let bag = Bag::new("personal", 4, Duration::ZERO).with_gone(&["m3"]);
    let d = start(&paths, &[("personal", bag.clone() as Arc<dyn Provider>)]).await;
    let mut c = person(&paths).await;
    eventually!("done", fill_of(&mut c, "personal").await.done);
    let f = fill_of(&mut c, "personal").await;
    assert_eq!((f.have, f.total), (3, 4), "m3 is gone on the server");
    assert_eq!(bag.fetched().iter().filter(|m| *m == "m3").count(), 1, "asked once, then set aside");
    drop(c);
    d.shutdown().await;

    // A restart: the walk is not done again, and what was set aside stays aside.
    let bag2 = Bag::new("personal", 4, Duration::ZERO).with_gone(&["m3"]);
    let d = start(&paths, &[("personal", bag2.clone() as Arc<dyn Provider>)]).await;
    let mut c = person(&paths).await;
    eventually!("synced", fill_of(&mut c, "personal").await.total == 4);
    tokio::time::sleep(Duration::from_millis(3000)).await;
    assert!(bag2.fetched().is_empty(), "nothing fetched again: {:?}", bag2.fetched());

    // Widening with --since looks at what was set aside again (and wakes the fill).
    let v: Value = c.call(method::SYNC_BODIES, BodyFillParams { account: Some(AccountId::new("personal")), since: Some(0) }).await.unwrap();
    assert_eq!(v["accounts"], 1);
    eventually!("asked again", bag2.fetched() == vec!["m3"]);
    drop(c);
    d.shutdown().await;
}

#[tokio::test]
async fn a_restart_goes_on_where_the_fill_stopped() {
    let (_dir, paths) = home(&config("body_fill = true\nbody_fill_per_min = 300\n", &[("personal", "")]));
    let bag = Bag::new("personal", 12, Duration::ZERO);
    let d = start(&paths, &[("personal", bag.clone() as Arc<dyn Provider>)]).await;
    eventually!("some bodies", bag.fetched().len() >= 4);
    d.shutdown().await;
    let first = bag.fetched();
    assert!(first.len() < 12);

    let bag2 = Bag::new("personal", 12, Duration::ZERO);
    let d = start(&paths, &[("personal", bag2.clone() as Arc<dyn Provider>)]).await;
    let mut c = person(&paths).await;
    eventually!("all bodies", fill_of(&mut c, "personal").await.have == 12);
    let second = bag2.fetched();
    let both: Vec<String> = first.iter().chain(second.iter()).cloned().collect();
    let unique: HashSet<&String> = both.iter().collect();
    assert_eq!(both.len(), 12, "each message fetched once across the restart: {first:?} then {second:?}");
    assert_eq!(unique.len(), 12);
    drop(c);
    d.shutdown().await;
}

#[tokio::test]
async fn accounts_opt_out_and_sync_bodies_runs_a_fill_the_config_has_off() {
    let (_dir, paths) = home(&config("", &[("personal", ""), ("private", "body_fill = false")]));
    let p = Bag::new("personal", 5, Duration::ZERO);
    let q = Bag::new("private", 5, Duration::ZERO);
    let d = start(&paths, &[("personal", p.clone() as Arc<dyn Provider>), ("private", q.clone() as Arc<dyn Provider>)]).await;
    let mut c = agent(&paths).await;
    eventually!("synced", fill_of(&mut c, "personal").await.total == 5);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(p.fetched().is_empty() && q.fetched().is_empty(), "off by default");
    assert!(!fill_of(&mut c, "personal").await.on);

    // Named and opted out: refused. All accounts: the opted-out one is skipped.
    let e = c.call::<_, Value>(method::SYNC_BODIES, BodyFillParams { account: Some(AccountId::new("private")), since: None }).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    let v: Value = c.call(method::SYNC_BODIES, BodyFillParams::default()).await.unwrap();
    assert_eq!(v["accounts"], 1);
    eventually!("filled on request", fill_of(&mut c, "personal").await.have == 5);
    assert!(q.fetched().is_empty());
    eventually!("and then off again", !fill_of(&mut c, "personal").await.on);
    drop(c);
    d.shutdown().await;
}

#[tokio::test]
async fn an_offline_account_is_not_filled() {
    let (_dir, paths) = home(&config("body_fill = true\nbody_fill_per_min = 600\n", &[("personal", "")]));
    let bag = Bag::new("personal", 3, Duration::ZERO);
    bag.offline.store(true, Ordering::SeqCst);
    let d = start(&paths, &[("personal", bag.clone() as Arc<dyn Provider>)]).await;
    let mut c = person(&paths).await;
    eventually!("says why", fill_of(&mut c, "personal").await.waiting.is_some_and(|w| w.contains("first sync") || w.contains("offline")));
    assert!(bag.fetched().is_empty());
    drop(c);
    d.shutdown().await;
}

// ---- gog: the process budget and whole threads -------------------------------------------------

/// gog with a counter of processes running at once.
struct CountingGog {
    fake: Arc<FakeRunner>,
    now: AtomicU32,
    most: AtomicU32,
}

#[async_trait::async_trait]
impl lumbridge_mail_gog::Runner for CountingGog {
    async fn run(&self, call: lumbridge_mail_gog::GogCall) -> Result<lumbridge_mail_gog::GogOutput> {
        let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(n, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let r = self.fake.run(call).await;
        self.now.fetch_sub(1, Ordering::SeqCst);
        r
    }
}

fn row(n: usize, thread: usize) -> Value {
    json!({"id": format!("19c00000000000{n:02}"), "threadId": format!("19c00000000000{thread:02}"), "date": "2026-11-09 14:00",
           "internalDateIso": format!("2026-11-09T14:{n:02}:00Z"), "from": format!("Sender {n} <s{n}@example.org>"),
           "to": "Test User <test.user@example.com>", "subject": format!("Made-up {n}"), "labels": ["INBOX"],
           "headers": {"Message-Id": format!("<f{n}@mail.example.org>")}, "body": format!("Made-up body {n}")})
}

fn full(n: usize, thread: usize) -> Value {
    use base64::Engine;
    let text = format!("Made-up full body {n}");
    json!({"id": format!("19c00000000000{n:02}"), "threadId": format!("19c00000000000{thread:02}"), "labelIds": ["INBOX"],
           "historyId": "900", "internalDate": "1794232800000", "snippet": text,
           "payload": {"mimeType": "text/plain", "headers": [{"name": "From", "value": format!("s{n}@example.org")}, {"name": "Subject", "value": format!("Made-up {n}")}],
                       "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)}}})
}

#[tokio::test]
async fn gog_fills_whole_threads_inside_the_process_budget() {
    // A strict host budget: 20 process starts a minute, one at a time. The fill may use half of it.
    let cfg = "[daemon]\nsync_interval_s = 600\ntriage = false\nbody_fill = true\nbody_fill_per_min = 600\n\n\
               [gog]\nlocal_max_in_flight = 1\nmax_starts_per_min = 20\nburst = 1\n\n\
               [[accounts]]\nid = \"g\"\npersona = \"personal\"\nemail = \"test.user@example.com\"\nname = \"Test User\"\nreadonly = true\n\
               [accounts.provider]\nkind = \"gog\"\nbinary = \"/nonexistent/gog\"\nwindow_days = 30\n";
    let (_dir, paths) = home(cfg);
    let f = Arc::new(FakeRunner::new());
    f.on_json("version", &[], json!({"version": "v0.48.0"}));
    f.on_json("gmail.labels.list", &[], json!({"labels": [{"id": "INBOX", "name": "INBOX", "type": "system"}]}));
    // 6 messages in 3 threads.
    let rows: Vec<Value> = [(6, 3), (5, 3), (4, 2), (3, 2), (2, 1), (1, 1)].iter().map(|(n, t)| row(*n, *t)).collect();
    f.on_json("gmail.messages.search", &["in:anywhere newer_than:30d"], json!({"messages": rows, "nextPageToken": ""}));
    f.on_json("gmail.raw", &["minimal"], json!({"id": "19c0000000000006", "historyId": "900"}));
    f.on_json("gmail.history", &[], json!({"historyId": "900", "messages": [], "nextPageToken": ""}));
    for t in 1..=3usize {
        let id = format!("19c00000000000{t:02}");
        f.on_json("gmail.thread.get", &[&id], json!({"thread": {"id": id, "messages": [full(2 * t - 1, t), full(2 * t, t)]}}));
    }
    let gog = Arc::new(CountingGog { fake: f.clone(), now: AtomicU32::new(0), most: AtomicU32::new(0) });
    let mut o = Options::new(paths.clone());
    o.gog_runner = Some(gog.clone());
    let d = Daemon::start(o).await.unwrap();
    let mut c = person(&paths).await;
    eventually!("every body", {
        let st = fill_of(&mut c, "g").await;
        st.total == 6 && st.have == 6
    });
    assert_eq!(f.calls_to("gmail.thread.get").len(), 3, "one process per thread, not per message");
    assert_eq!(gog.most.load(Ordering::SeqCst), 1, "never more than one gog process");
    let info: DaemonInfo = c.call(method::DAEMON_INFO, json!({})).await.unwrap();
    let load = &info.load[0];
    assert!(load.starts_last_min <= load.max_starts_per_min, "{load:?}");
    // The fill starts a process only while less than half the minute's budget is used.
    assert!(load.starts_last_min <= load.max_starts_per_min / 2 + 1, "{load:?}");
    drop(c);
    d.shutdown().await;
}
