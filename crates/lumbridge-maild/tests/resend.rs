//! The daemon with a Resend account (against the in-process fake Resend server) next to a demo account,
//! driven over its socket. Made-up mail and made-up keys only; nothing reaches resend.com.

use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_mail_resend::fake::{FAKE_KEY, FakeAttachment, FakeReceived, FakeResend, FakeSent, FakeState};
use lumbridge_maild::{Client, Daemon, DemoProvider, DemoSet, Options};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const T0: i64 = 1_794_211_200_000;

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

fn set_env(var: &str, value: &str) {
    // SAFETY: each test uses its own variable name; nothing else reads or writes it.
    unsafe { std::env::set_var(var, value) };
}

fn config(base: &str, key_env: &str, ro_key_env: &str) -> String {
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

[[accounts]]
id = "shop"
persona = "work"
label = "Shop"
email = "hello@shop.example"
name = "Shop"
[accounts.provider]
kind = "resend"
base_url = "{base}"
api_key_env = "{key_env}"
requests_per_second = 50

[[accounts]]
id = "shop-ro"
persona = "work"
label = "Shop (read-only)"
email = "hello@shop.example"
name = "Shop"
readonly = true
[accounts.provider]
kind = "resend"
base_url = "{base}"
api_key_env = "{ro_key_env}"
requests_per_second = 50
"##
    )
}

struct Env {
    _dir: tempfile::TempDir,
    paths: Paths,
}

impl Env {
    fn new(config: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home.clone().into_os_string()));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.config_file(), config).unwrap();
        Env { _dir: dir, paths }
    }

    async fn start(&self, keyring: Option<Arc<Keyring>>) -> Daemon {
        let mut o = Options::new(self.paths.clone());
        let demo = DemoProvider::new(AccountId::new("personal"), DemoSet::Personal, Address::new(Some("Sam Example"), "sam@example.com"));
        o.providers.insert(AccountId::new("personal"), Arc::new(demo) as Arc<dyn Provider>);
        if let Some(k) = keyring {
            o.secrets = Some(k);
        }
        Daemon::start(o).await.unwrap()
    }

    async fn person(&self) -> Client {
        let token = Client::read_person_token(&self.paths.person_token()).expect("token file");
        Client::connect(&self.paths.socket(), Some(token), "test person").await.unwrap()
    }

    async fn agent(&self) -> Client {
        Client::connect(&self.paths.socket(), None, "test agent").await.unwrap()
    }
}

/// A stand-in keyring that keeps IMAP passwords and Resend keys apart, like the real one.
#[derive(Default)]
struct Keyring {
    imap: Mutex<Vec<(String, String)>>,
    resend: Mutex<Vec<(String, String)>>,
}

impl lumbridge_maild::secrets::SecretStore for Keyring {
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.imap.lock().unwrap().push((account.into(), secret.into()));
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.imap.lock().unwrap().retain(|(a, _)| a != account);
        Ok(())
    }
    fn set_resend(&self, account: &str, key: &str) -> Result<()> {
        self.resend.lock().unwrap().push((account.into(), key.into()));
        Ok(())
    }
    fn delete_resend(&self, account: &str) -> Result<()> {
        self.resend.lock().unwrap().retain(|(a, _)| a != account);
        Ok(())
    }
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

fn shop_mail() -> FakeState {
    let mut st = FakeState::new();
    st.add_received(FakeReceived {
        from: "Ada Customer <ada@example.com>".into(),
        to: vec!["hello@shop.example".into()],
        subject: "Do you ship to Lisbon?".into(),
        text: Some("Hello, do you ship to Lisbon? Photo of the shelf attached.".into()),
        created_ms: T0,
        attachments: vec![FakeAttachment { filename: "shelf.jpg".into(), content_type: "image/jpeg".into(), bytes: b"made-up-jpeg".to_vec(), ..FakeAttachment::default() }],
        ..FakeReceived::default()
    });
    st.add_sent(FakeSent {
        from: "Shop <hello@shop.example>".into(),
        to: vec!["bo@example.com".into()],
        subject: "Your receipt".into(),
        text: Some("Thanks for your order.".into()),
        created_ms: T0 + 60_000,
        last_event: "delivered".into(),
        ..FakeSent::default()
    });
    st.add_sent(FakeSent {
        from: "Shop <hello@shop.example>".into(),
        to: vec!["cy@example.com".into()],
        subject: "Your trial ends soon".into(),
        created_ms: T0 + 120_000,
        scheduled_ms: Some(T0 + 400 * 86_400_000),
        last_event: "scheduled".into(),
        ..FakeSent::default()
    });
    st
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resend_account_next_to_a_demo_account() {
    let fake = FakeResend::start(shop_mail()).await;
    set_env("LBMAIL_TEST_RESEND_KEY_E2E", FAKE_KEY);
    let env = Env::new(&config(&fake.base_url(), "LBMAIL_TEST_RESEND_KEY_E2E", "LBMAIL_TEST_RESEND_KEY_E2E"));
    let d = env.start(None).await;
    let mut c = env.person().await;
    eventually!("shop synced", threads(&mut c, only("shop")).await.len() == 3);
    eventually!("demo synced", !threads(&mut c, only("personal")).await.is_empty());
    let st = status(&mut c, "shop").await;
    assert!(st.online && st.error.is_none(), "{st:?}");

    // The unified inbox has both accounts.
    let inbox = threads(&mut c, Query { role: Some(MailboxRole::Inbox), limit: 100, ..Query::default() }).await;
    assert!(inbox.iter().any(|t| t.account.as_str() == "personal"));
    let q = inbox.iter().find(|t| t.account.as_str() == "shop").expect("received mail in the unified inbox").clone();
    assert_eq!(q.subject, "Do you ship to Lisbon?");
    assert_eq!(q.unread_count, 1);
    let mbs: Vec<Mailbox> = c.call(method::MAILBOXES, json!({"account": "shop"})).await.unwrap();
    assert!(mbs.iter().any(|m| m.id.as_str() == "scheduled" && m.total == 1), "{mbs:?}");
    assert!(mbs.iter().any(|m| m.role == MailboxRole::Sent && m.total == 1), "{mbs:?}");

    // Body and attachment through the daemon.
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: q.account.clone(), thread: q.id.clone() }).await.unwrap();
    let m = msgs[0].clone();
    let body: Body = c.call(method::BODY, MessageRef { account: m.account.clone(), message: m.id.clone() }).await.unwrap();
    assert!(body.text.unwrap().contains("Lisbon"));
    let att = &body.attachments[0];
    let p: Value = c.call(method::ATTACHMENT, AttachmentRef { account: m.account.clone(), message: m.id.clone(), attachment: att.id.clone() }).await.unwrap();
    assert_eq!(std::fs::read(p["path"].as_str().unwrap()).unwrap(), b"made-up-jpeg");

    // Capabilities: Resend keeps no read state; the read-only account can do nothing.
    let caps: Vec<AccountCapabilities> = c.call(method::ACCOUNTS_CAPABILITIES, json!({})).await.unwrap();
    let cap = |id: &str| caps.iter().find(|x| x.account.as_str() == id).unwrap().capabilities.clone();
    assert_eq!(cap("shop").seen, Support::Local);
    assert!(cap("shop").send);
    assert_eq!(cap("personal").seen, Support::Server);
    assert!(!cap("shop-ro").send && cap("shop-ro").seen == Support::Unsupported);

    // Mark read: local only, and it survives the next sync.
    let calls_before = fake.lock().requests.iter().filter(|r| r.method != "GET").count();
    let _: Value = c.call(method::ACT, Act { account: m.account.clone(), action: Action::SetSeen { ids: vec![m.id.clone()], seen: true }, caller: Caller::Person }).await.unwrap();
    eventually!("queue empty", status(&mut c, "shop").await.queued == 0);
    let _: Value = c.call(method::SYNC, json!({"account": "shop"})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t = threads(&mut c, only("shop")).await.into_iter().find(|t| t.id == q.id).unwrap();
    assert_eq!(t.unread_count, 0);

    // An agent cannot send; neither can anyone on the read-only account.
    let draft = Draft {
        local_id: "e2e-reply-1".into(),
        to: vec![Address::new(Some("Ada Customer"), "ada@example.com")],
        subject: "Re: Do you ship to Lisbon?".into(),
        text: "Yes, in three days.".into(),
        reply_to: Some(m.id.clone()),
        ..Draft::default()
    };
    let mut agent = env.agent().await;
    let e = agent.call::<_, Value>(method::ACT, Act { account: m.account.clone(), action: Action::Send { draft: draft.clone() }, caller: Caller::Person }).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e:?}");
    let ro = AccountId::new("shop-ro");
    let e = c.call::<_, Value>(method::ACT, Act { account: ro.clone(), action: Action::Send { draft: draft.clone() }, caller: Caller::Person }).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e:?}");
    let e = c.call::<_, Value>(method::ACT, Act { account: ro, action: Action::SetSeen { ids: vec![m.id.clone()], seen: true }, caller: Caller::Person }).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e:?}");
    assert_eq!(fake.lock().requests.iter().filter(|r| r.method != "GET").count(), calls_before, "nothing but reads reached Resend");

    // A person sends: once, threaded, and it shows up in Sent in the same thread.
    let _: Value = c.call(method::ACT, Act { account: m.account.clone(), action: Action::Send { draft }, caller: Caller::Person }).await.unwrap();
    eventually!("sent", fake.lock().sent.len() == 3);
    eventually!("queue empty", status(&mut c, "shop").await.queued == 0);
    {
        let s = fake.lock();
        let e = s.sent.last().unwrap();
        assert_eq!(e.from, "\"Shop\" <hello@shop.example>");
        assert!(e.headers["In-Reply-To"].as_str().unwrap().ends_with("@sender.example>"));
    }
    eventually!("the reply joins the thread", {
        let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: q.account.clone(), thread: q.id.clone() }).await.unwrap();
        msgs.len() == 2
    });

    // Trash on the scheduled email cancels it at Resend.
    let sched = threads(&mut c, Query { accounts: Some(vec![AccountId::new("shop")]), mailbox: Some(MailboxId::new("scheduled")), limit: 10, ..Query::default() }).await;
    assert_eq!(sched.len(), 1);
    let sm: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: AccountId::new("shop"), thread: sched[0].id.clone() }).await.unwrap();
    let _: Value = c.call(method::ACT, Act { account: AccountId::new("shop"), action: Action::Trash { ids: vec![sm[0].id.clone()] }, caller: Caller::Person }).await.unwrap();
    eventually!("canceled at Resend", fake.lock().sent.iter().any(|e| e.last_event == "canceled"));

    // Domains and key names over RPC; not for other accounts.
    let domains: Vec<lumbridge_mail_resend::DomainInfo> = c.call(method::RESEND_DOMAINS, json!({"account": "shop"})).await.unwrap();
    assert_eq!(domains[0].name, "shop.example");
    let keys: Vec<lumbridge_mail_resend::ApiKeyInfo> = c.call(method::RESEND_API_KEYS, json!({"account": "shop"})).await.unwrap();
    assert_eq!(keys[0].name, "shop website");
    let e = c.call::<_, Value>(method::RESEND_DOMAINS, json!({"account": "personal"})).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_or_missing_key_shows_as_sign_in_needed() {
    let fake = FakeResend::start(shop_mail()).await;
    set_env("LBMAIL_TEST_RESEND_KEY_BAD", "re_wrong_madeup_key_987654");
    // shop has a wrong key; shop-ro reads a variable nobody set.
    let cfg = config(&fake.base_url(), "LBMAIL_TEST_RESEND_KEY_BAD", "LBMAIL_TEST_RESEND_KEY_UNSET");
    let env = Env::new(&cfg);
    let d = env.start(None).await;
    let mut c = env.person().await;
    eventually!("auth error shown", matches!(status(&mut c, "shop").await.error, Some(Error::Auth(_))));
    let e = status(&mut c, "shop").await.error.unwrap();
    assert!(!format!("{e:?}").contains("wrong_madeup"), "{e:?}");
    eventually!("missing key shown", matches!(status(&mut c, "shop-ro").await.error, Some(Error::Auth(_))));
    let e = status(&mut c, "shop-ro").await.error.unwrap();
    assert!(e.to_string().contains("LBMAIL_TEST_RESEND_KEY_UNSET"), "{e}");
    assert!(threads(&mut c, only("shop")).await.is_empty());
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adding_a_resend_account_keeps_the_key_in_the_keyring() {
    let env = Env::new("");
    let keyring = Arc::new(Keyring::default());
    let d = env.start(Some(keyring.clone())).await;
    let mut c = env.person().await;
    let n = NewAccount {
        id: None,
        persona: Persona::Work,
        label: "Shop".into(),
        email: "hello@shop.example".into(),
        name: "Shop".into(),
        readonly: false,
        provider: NewProvider::Resend { addresses: vec![], receiving: Some(true) },
        password: Some("re_made_up_added_key_1".into()),
    };
    let a: Account = c.call(method::ACCOUNTS_ADD, &n).await.unwrap();
    assert_eq!(a.provider, ProviderKind::Resend);
    assert_eq!(keyring.resend.lock().unwrap().as_slice(), &[(a.id.to_string(), "re_made_up_added_key_1".to_string())]);
    assert!(keyring.imap.lock().unwrap().is_empty());
    let text = std::fs::read_to_string(env.paths.config_file()).unwrap();
    assert!(text.contains("kind = \"resend\"") && !text.contains("re_made_up"), "{text}");
    d.shutdown().await;
}
