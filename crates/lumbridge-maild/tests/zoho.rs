//! The daemon with two Zoho accounts on one sign-in (against the in-process fake Zoho MCP server) next
//! to a demo account, driven over its socket. Made-up mail and made-up tokens only; nothing reaches
//! Zoho.

use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_mail_zoho::fake::{FAKE_CLIENT_ID, FakeAccount, FakeMail, FakeState, FakeZoho, now_ms};
use lumbridge_maild::{Client, Daemon, DemoProvider, DemoSet, Options};
use serde_json::{Value, json};
use std::sync::Arc;
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

fn config(fake: &FakeZoho, token_file: &std::path::Path) -> String {
    let account = |id: &str, email: &str, ro: bool| {
        format!(
            r##"
[[accounts]]
id = "{id}"
persona = "work"
label = "{id}"
email = "{email}"
name = "Sam"
readonly = {ro}
[accounts.provider]
kind = "zoho"
mcp_url = "{mcp}"
token_url = "{tok}"
client_id = "{FAKE_CLIENT_ID}"
token_file = "{file}"
poll_interval_s = 5
max_calls_per_min = 6000
"##,
            mcp = fake.mcp_url(),
            tok = fake.token_url(),
            file = token_file.display()
        )
    };
    format!(
        r##"
[daemon]
sync_interval_s = 30
first_retry_s = 1
max_retry_s = 2
max_backoff_s = 2
triage = false

[[accounts]]
id = "personal"
persona = "personal"
label = "Personal"
email = "sam@example.com"
name = "Sam Example"
[accounts.provider]
kind = "demo"
{}{}"##,
        account("zoho-one", "sam@one.example.test", false),
        account("zoho-two", "sam@two.example.test", true)
    )
}

fn only(a: &str) -> Query {
    Query { accounts: Some(vec![AccountId::new(a)]), limit: 100, ..Query::default() }
}

async fn threads(c: &mut Client, q: Query) -> Vec<ThreadSummary> {
    let p: ThreadPage = c.call(method::THREADS, &q).await.unwrap();
    p.threads
}

async fn status(c: &mut Client, id: &str) -> AccountStatus {
    let st: Vec<AccountStatus> = c.call(method::STATUS, json!({})).await.unwrap();
    st.into_iter().find(|s| s.account.as_str() == id).unwrap()
}

fn mail(account: &str, folder: &str, mins_ago: i64, subject: &str, to: &str) -> FakeMail {
    FakeMail {
        account: account.into(),
        folder: folder.into(),
        subject: subject.into(),
        from: "pat@example.com".into(),
        sender: "Pat Example".into(),
        to: vec![to.into()],
        received_ms: now_ms() - mins_ago * 60_000,
        summary: format!("About {subject}"),
        ..FakeMail::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_zoho_addresses_on_one_sign_in_next_to_a_demo_account() {
    let mut st = FakeState::new(vec![FakeAccount::standard("1001", "sam@one.example.test"), FakeAccount::standard("2002", "sam@two.example.test")]);
    let one = st.add(mail("1001", "100101", 10, "Invoice question", "sam@one.example.test"));
    st.add(mail("1001", "100103", 5, "Sent note", "kim@example.org"));
    st.add(mail("2002", "200201", 8, "Hello domain two", "sam@two.example.test"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    // An expired token: both accounts start together and must share one refresh.
    let token_file = fake.write_token_file(dir.path(), 0);
    fake.lock().access_token = "fake-access-stale".into();

    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_path_buf();
    let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home_path.clone().into_os_string()));
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(paths.config_file(), config(&fake, &token_file)).unwrap();
    let mut o = Options::new(paths.clone());
    let demo = DemoProvider::new(AccountId::new("personal"), DemoSet::Personal, Address::new(Some("Sam Example"), "sam@example.com"));
    o.providers.insert(AccountId::new("personal"), Arc::new(demo) as Arc<dyn Provider>);
    let d = Daemon::start(o).await.unwrap();
    let token = Client::read_person_token(&paths.person_token()).expect("token file");
    let mut c = Client::connect(&paths.socket(), Some(token), "test person").await.unwrap();

    eventually!("zoho-one synced", threads(&mut c, only("zoho-one")).await.len() == 2);
    eventually!("zoho-two synced", threads(&mut c, only("zoho-two")).await.len() == 1);
    assert_eq!(fake.lock().token_requests.len(), 1, "one refresh for the shared sign-in");
    let st1 = status(&mut c, "zoho-one").await;
    assert!(st1.online && st1.error.is_none(), "{st1:?}");
    let accounts: Vec<Account> = c.call(method::ACCOUNTS, json!({})).await.unwrap();
    assert!(accounts.iter().filter(|a| a.provider == ProviderKind::Zoho).count() == 2);

    // The unified inbox has both Zoho addresses.
    let inbox = threads(&mut c, Query { role: Some(MailboxRole::Inbox), limit: 100, ..Query::default() }).await;
    assert!(inbox.iter().any(|t| t.account.as_str() == "zoho-one" && t.subject == "Invoice question"));
    assert!(inbox.iter().any(|t| t.account.as_str() == "zoho-two"));

    // Capabilities: drafts local; the read-only account can do nothing.
    let caps: Vec<AccountCapabilities> = c.call(method::ACCOUNTS_CAPABILITIES, json!({})).await.unwrap();
    let cap = |id: &str| caps.iter().find(|x| x.account.as_str() == id).unwrap().capabilities.clone();
    assert_eq!((cap("zoho-one").drafts, cap("zoho-one").seen), (Support::Local, Support::Server));
    assert!(!cap("zoho-two").send && cap("zoho-two").seen == Support::Unsupported);

    // Body through the daemon.
    let t = inbox.iter().find(|t| t.account.as_str() == "zoho-one").unwrap().clone();
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: t.account.clone(), thread: t.id.clone() }).await.unwrap();
    assert_eq!(msgs[0].id.as_str(), one);
    let body: Body = c.call(method::BODY, MessageRef { account: t.account.clone(), message: msgs[0].id.clone() }).await.unwrap();
    assert!(body.html.unwrap().contains("About Invoice question"));

    // Mark read reaches Zoho; the read-only account refuses before anything is queued.
    let _: Value = c.call(method::ACT, Act { account: t.account.clone(), action: Action::SetSeen { ids: vec![msgs[0].id.clone()], seen: true }, caller: Caller::Person }).await.unwrap();
    eventually!("read at Zoho", fake.lock().mail(&one).unwrap().read);
    let two = threads(&mut c, only("zoho-two")).await;
    let m2: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: AccountId::new("zoho-two"), thread: two[0].id.clone() }).await.unwrap();
    let e = c
        .call::<_, Value>(method::ACT, Act { account: AccountId::new("zoho-two"), action: Action::Trash { ids: vec![m2[0].id.clone()] }, caller: Caller::Person })
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e:?}");
    assert_eq!(fake.lock().count("ZohoMail_move_mails_to_trash"), 0);

    // A person replies; it goes out once, as a reply.
    let draft = Draft {
        local_id: "zoho-e2e-1".into(),
        to: vec![Address::new(None, "pat@example.com")],
        subject: "Re: Invoice question".into(),
        text: "Paid yesterday.".into(),
        reply_to: Some(msgs[0].id.clone()),
        ..Draft::default()
    };
    let _: Value = c.call(method::ACT, Act { account: t.account.clone(), action: Action::Send { draft }, caller: Caller::Person }).await.unwrap();
    eventually!("sent", fake.lock().sends.len() == 1);
    eventually!("queue empty", status(&mut c, "zoho-one").await.queued == 0);
    assert_eq!(fake.lock().sends[0]["tool"], "ZohoMail_reply_to_mail");

    // The connection's budget shows in daemon.info.
    let info: DaemonInfo = c.call(method::DAEMON_INFO, json!({})).await.unwrap();
    assert!(info.load.iter().any(|l| l.scope.starts_with("zoho:")), "{:?}", info.load);
    d.shutdown().await;
}

/// Zoho's listings carry no thread id: the daemon groups them (subject and people, then the headers of
/// a fetched body), and mail stored one message per thread by an older daemon is grouped once at start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zoho_mail_is_grouped_into_threads_and_old_single_threads_are_regrouped() {
    let mut st = FakeState::new(vec![FakeAccount::standard("1001", "sam@one.example.test"), FakeAccount::standard("2002", "sam@two.example.test")]);
    let a = st.add(mail("1001", "100101", 120, "Venue for the offsite", "sam@one.example.test"));
    let b = st.add(FakeMail { from: "sam@one.example.test".into(), sender: "Sam".into(), ..mail("1001", "100103", 90, "RE: Venue for the offsite", "pat@example.com") });
    let c = st.add(mail("1001", "100101", 80, "Unrelated", "sam@one.example.test"));
    let d = st.add(FakeMail {
        headers: vec![format!("In-Reply-To: <{a}@fake.example.test>"), format!("References: <{a}@fake.example.test>")],
        ..mail("1001", "100101", 60, "Totally different words", "sam@one.example.test")
    });
    let e = st.add(mail("1001", "100101", 40, "Re: Venue for the offsite", "sam@one.example.test"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = fake.write_token_file(dir.path(), 3600);
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_path_buf();
    let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home_path.clone().into_os_string()));
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(paths.config_file(), config(&fake, &token_file)).unwrap();

    // What an older daemon left: a and b stored as threads of one message each, no cursor yet.
    {
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let mut s = lumbridge_mail_store::Store::open(paths.database()).unwrap();
        let acct = Account { id: AccountId::new("zoho-one"), persona: Persona::Work, label: "zoho-one".into(), email: "sam@one.example.test".into(),
                             name: "Sam".into(), provider: ProviderKind::Zoho, color: None, ai: AiPolicy::Off, readonly: false };
        s.upsert_account(&acct).unwrap();
        let meta = |id: &str, folder: &str, subject: &str, from: &str, to: &str, mins: i64| MessageMeta {
            id: MessageId::new(id), account: acct.id.clone(), thread: ThreadId::new(id), mailboxes: vec![MailboxId::new(folder)],
            from: vec![Address::new(None, from)], to: vec![Address::new(None, to)], cc: vec![], bcc: vec![],
            subject: subject.into(), snippet: String::new(), date: now_ms() / 1000 - mins * 60, flags: Flags::default(), has_attachments: false,
            size: 0, rfc_message_id: None, in_reply_to: None, references: vec![], partial: true,
        };
        s.apply_delta(&acct.id, &Delta { upserted: vec![
            meta(&a, "100101", "Venue for the offsite", "pat@example.com", "sam@one.example.test", 120),
            meta(&b, "100103", "RE: Venue for the offsite", "sam@one.example.test", "pat@example.com", 90),
        ], ..Delta::default() }).unwrap();
        assert_eq!(s.message(&acct.id, &MessageId::new(&b)).unwrap().unwrap().thread.as_str(), b);
    }

    let mut o = Options::new(paths.clone());
    let demo = DemoProvider::new(AccountId::new("personal"), DemoSet::Personal, Address::new(Some("Sam Example"), "sam@example.com"));
    o.providers.insert(AccountId::new("personal"), Arc::new(demo) as Arc<dyn Provider>);
    let dmn = Daemon::start(o).await.unwrap();
    let token = Client::read_person_token(&paths.person_token()).expect("token file");
    let mut cl = Client::connect(&paths.socket(), Some(token), "test person").await.unwrap();

    let thread_of = |ts: &[ThreadSummary], id: &str| -> Option<(String, u32)> {
        ts.iter().find(|t| t.id.as_str() == id).map(|t| (t.id.0.clone(), t.message_count))
    };
    // Synced: a, b and e in one thread (named by the oldest, a), c and d alone (d's headers are not known yet).
    eventually!("zoho-one synced and grouped", {
        let ts = threads(&mut cl, only("zoho-one")).await;
        ts.len() == 3 && thread_of(&ts, &a).is_some_and(|(_, n)| n == 3)
    });
    let ts = threads(&mut cl, only("zoho-one")).await;
    assert!(thread_of(&ts, &c).is_some() && thread_of(&ts, &d).is_some(), "{ts:?}");
    let marker = |id: &str| lumbridge_maild::threading_marker(&paths, &AccountId::new(id));
    assert!(marker("zoho-one").is_file() && marker("zoho-two").is_file(), "the upgrade is noted per Zoho account");
    assert!(!marker("personal").exists(), "the demo account has its own thread ids");

    // Opening d fetches its body: its In-Reply-To names a, whose Message-ID is not known yet (its body was
    // never fetched), so nothing moves. Opening a tells it: d joins a's thread.
    let body = |id: &str| MessageRef { account: AccountId::new("zoho-one"), message: MessageId::new(id) };
    let _: Body = cl.call(method::BODY, body(&d)).await.unwrap();
    assert_eq!(threads(&mut cl, only("zoho-one")).await.len(), 3);
    let _: Body = cl.call(method::BODY, body(&a)).await.unwrap();
    eventually!("d joined by its headers", {
        let ts = threads(&mut cl, only("zoho-one")).await;
        ts.len() == 2 && thread_of(&ts, &a).is_some_and(|(_, n)| n == 4)
    });
    let msgs: Vec<MessageMeta> = cl.call(method::THREAD, ThreadRef { account: AccountId::new("zoho-one"), thread: ThreadId::new(&a) }).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id.0.clone()).collect::<Vec<_>>(), [a.clone(), b.clone(), d.clone(), e.clone()]);
    // A sync round later (rows with their own ids as threads again), nothing comes apart.
    let _: Value = cl.call(method::SYNC, json!({"account": "zoho-one"})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let ts = threads(&mut cl, only("zoho-one")).await;
    assert_eq!(ts.len(), 2, "{ts:?}");
    let _ = c;
    dmn.shutdown().await;
}
