//! The daemon in-process on a temp LUMBRIDGE_MAIL_HOME with demo accounts, driven over its socket.
//! Made-up mail only.

use lumbridge_mail_ai::ScriptedModel;
use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_maild::{Client, Daemon, DemoMail, DemoProvider, DemoSet, Options};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CONFIG: &str = r##"
[daemon]
sync_interval_s = 30
first_retry_s = 1
max_retry_s = 2
max_backoff_s = 2

[[accounts]]
id = "personal"
persona = "personal"
label = "Personal"
email = "sam@example.com"
name = "Sam Example"
[accounts.provider]
kind = "demo"

[[accounts]]
id = "work"
persona = "work"
label = "Work"
email = "sam@work.example"
name = "Sam Example"
ai = "off"
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

[[accounts]]
id = "ro"
persona = "personal"
label = "Read-only"
email = "sam@readonly.example"
name = "Sam Example"
readonly = true
[accounts.provider]
kind = "demo"
set = "personal"
"##;

struct Env {
    _dir: tempfile::TempDir,
    paths: Paths,
    demos: HashMap<&'static str, Arc<DemoProvider>>,
    model: Arc<ScriptedModel>,
}

fn answer(req: &lumbridge_mail_ai::ChatRequest) -> Result<String> {
    let user = req.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join("\n");
    Ok(match req.task {
        "triage" if user.contains("Dinner") => json!({"category": "important", "priority": 80, "summary": "Maya asks you to book a table for four", "needs_reply": true, "due": null}),
        "triage" => json!({"category": "personal", "priority": 50, "summary": "A note from a friend", "needs_reply": false, "due": null}),
        "summary" => json!({"summary": "Maya and Sam settle dinner on Saturday at 7:30; Maya asks for a table for four."}),
        "draft" => json!({"body": "Hi Maya,\n\nDone, I booked a table for four at 7:30.\n\nSam"}),
        "extract" => json!({"items": [{"text": "Book a table for four", "owner": "me", "due": null}], "dates": []}),
        "ask_plan" => json!({"queries": ["dinner"]}),
        "ask_answer" => json!({"answer": "Dinner is on Saturday at 7:30.", "sources": ["t1"]}),
        _ => json!({}),
    }
    .to_string())
}

impl Env {
    fn new() -> Env {
        Self::with_config(CONFIG)
    }

    fn with_config(config: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        // Resolved exactly as the binaries do with LUMBRIDGE_MAIL_HOME set.
        let home = dir.path().to_path_buf();
        let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home.clone().into_os_string()));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.config_file(), config).unwrap();
        let me = |name: &str, email: &str| Address::new(Some(name), email);
        let mut demos = HashMap::new();
        demos.insert("personal", Arc::new(DemoProvider::new(AccountId::new("personal"), DemoSet::Personal, me("Sam Example", "sam@example.com"))));
        demos.insert("work", Arc::new(DemoProvider::new(AccountId::new("work"), DemoSet::Work, me("Sam Example", "sam@work.example"))));
        demos.insert("anon", Arc::new(DemoProvider::new(AccountId::new("anon"), DemoSet::Anon, me("Otter", "otter@anon.example"))));
        demos.insert("ro", Arc::new(DemoProvider::new(AccountId::new("ro"), DemoSet::Personal, me("Sam Example", "sam@readonly.example"))));
        Env { _dir: dir, paths, demos, model: Arc::new(ScriptedModel::with(answer)) }
    }

    fn demo(&self, id: &str) -> &Arc<DemoProvider> {
        &self.demos[id]
    }

    async fn start(&self) -> Daemon {
        let mut o = Options::new(self.paths.clone());
        for (id, p) in &self.demos {
            o.providers.insert(AccountId::new(*id), p.clone() as Arc<dyn Provider>);
        }
        o.model = Some(self.model.clone());
        Daemon::start(o).await.unwrap()
    }

    async fn person(&self) -> Client {
        let token = Client::read_person_token(&self.paths.person_token()).expect("token file");
        Client::connect(&self.paths.socket(), Some(token), "test person").await.unwrap()
    }

    async fn agent(&self) -> Client {
        Client::connect(&self.paths.socket(), None, "test agent").await.unwrap()
    }

    /// Model requests so far, as one string per request.
    fn prompts(&self) -> Vec<String> {
        self.model.requests().iter().map(|r| r.messages.iter().map(|m| m.content.clone()).collect::<Vec<_>>().join("\n")).collect()
    }
}

/// Polls `cond` (an expression that may `.await`) until it holds, for up to 20 s.
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

async fn status(c: &mut Client, id: &str) -> AccountStatus {
    let st: Vec<AccountStatus> = c.call(method::STATUS, json!({})).await.unwrap();
    st.into_iter().find(|s| s.account.as_str() == id).unwrap()
}

async fn threads(c: &mut Client, q: Query) -> Vec<ThreadSummary> {
    let p: ThreadPage = c.call(method::THREADS, &q).await.unwrap();
    p.threads
}

fn inbox() -> Query {
    Query { role: Some(MailboxRole::Inbox), limit: 100, ..Query::default() }
}

fn only(a: &str) -> Query {
    Query { accounts: Some(vec![AccountId::new(a)]), limit: 100, ..Query::default() }
}

async fn synced(env: &Env) {
    let mut c = env.person().await;
    eventually!("every account synced", {
        let st: Vec<AccountStatus> = c.call(method::STATUS, json!({})).await.unwrap();
        st.len() == 4 && st.iter().all(|s| s.last_sync.is_some() && s.online)
    });
    // Each account's first full sync is two pages: wait for the second.
    for (id, n) in [("personal", 8), ("work", 5), ("anon", 3), ("ro", 8)] {
        eventually!(format!("{id} has its threads"), threads(&mut c, only(id)).await.len() >= n);
    }
}

fn find<'a>(ts: &'a [ThreadSummary], id: &str) -> &'a ThreadSummary {
    ts.iter().find(|t| t.id.as_str() == id).unwrap_or_else(|| panic!("no thread {id}"))
}

fn find_in<'a>(ts: &'a [ThreadSummary], account: &str, id: &str) -> &'a ThreadSummary {
    ts.iter().find(|t| t.id.as_str() == id && t.account.as_str() == account).unwrap_or_else(|| panic!("no thread {account}/{id}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_serves_mail_over_its_socket() {
    let env = Env::new();
    let d = env.start().await;
    // The socket and the token are private to this user.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(env.paths.socket()).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(env.paths.person_token()).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&env.paths.runtime_dir).unwrap().permissions().mode() & 0o777, 0o700);
    synced(&env).await;
    let mut c = env.person().await;
    let mut tail = env.agent().await;

    let hello: Hello = c.call(method::HELLO, json!({})).await.unwrap();
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    let accounts: Vec<Account> = c.call(method::ACCOUNTS, json!({})).await.unwrap();
    assert_eq!(accounts.len(), 4);
    let boxes: Vec<Mailbox> = c.call(method::MAILBOXES, json!({"account": "personal"})).await.unwrap();
    assert!(boxes.iter().any(|b| b.role == MailboxRole::Inbox && b.total > 0));

    // The unified inbox: never the anon account unless it is named.
    let unified = threads(&mut c, inbox()).await;
    assert!(unified.iter().all(|t| t.account.as_str() != "anon"));
    assert!(unified.iter().any(|t| t.account.as_str() == "work"));
    let anon = threads(&mut c, Query { accounts: Some(vec![AccountId::new("anon")]), ..inbox() }).await;
    assert!(anon.iter().any(|t| t.subject.contains("blue bike")));

    // A thread, oldest first, and a body fetched on demand.
    let dinner = ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t-dinner") };
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, &dinner).await.unwrap();
    assert_eq!(msgs.len(), 3);
    assert!(msgs.windows(2).all(|w| w[0].date <= w[1].date));
    let body: Body = c.call(method::BODY, MessageRef { account: dinner.account.clone(), message: msgs[2].id.clone() }).await.unwrap();
    assert!(body.text.unwrap().contains("table for four"));

    // An attachment lands in the cache directory under a safe name, private.
    let hike: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t-hike") }).await.unwrap();
    let saved: Value = c
        .call(method::ATTACHMENT, AttachmentRef { account: AccountId::new("personal"), message: hike[0].id.clone(), attachment: AttachmentId::new("a1") })
        .await
        .unwrap();
    let path = std::path::PathBuf::from(saved["path"].as_str().unwrap());
    assert!(path.starts_with(env.paths.attachments_dir()));
    assert_eq!(path.file_name().unwrap(), "hike-map.pdf");
    assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.4 made-up map\n");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

    // The same bytes in pieces over the protocol (`message.attachment.read`, for a client elsewhere).
    let read = |offset: u64, max: u32| AttachmentRead {
        account: AccountId::new("personal"),
        message: hike[0].id.clone(),
        attachment: AttachmentId::new("a1"),
        offset,
        max,
    };
    let first: AttachmentChunk = c.call(method::ATTACHMENT_READ, read(0, 9)).await.unwrap();
    assert_eq!((first.filename.as_str(), first.mime.as_str(), first.size, first.offset, first.eof), ("hike-map.pdf", "application/pdf", 21, 0, false));
    let rest: AttachmentChunk = c.call(method::ATTACHMENT_READ, read(9, 0)).await.unwrap();
    assert!(rest.eof);
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut whole = b64.decode(&first.data).unwrap();
    whole.extend(b64.decode(&rest.data).unwrap());
    assert_eq!(whole, b"%PDF-1.4 made-up map\n");
    let past: Result<AttachmentChunk> = c.call(method::ATTACHMENT_READ, read(22, 0)).await;
    assert!(matches!(past, Err(Error::Invalid(_))), "{past:?}");
    let missing: Result<AttachmentChunk> =
        c.call(method::ATTACHMENT_READ, AttachmentRead { attachment: AttachmentId::new("nope"), ..read(0, 0) }).await;
    assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");

    // Search, with operators; the anon wall holds for search too.
    // ("ro" serves the same made-up personal mail, so it matches too.)
    let hits = threads(&mut c, Query { text: Some("noodle".into()), ..Query::default() }).await;
    let mut got: Vec<String> = hits.iter().map(|t| format!("{}/{}", t.account, t.id)).collect();
    got.sort();
    assert_eq!(got, ["personal/t-dinner", "ro/t-dinner"]);
    let from = threads(&mut c, Query { text: Some("from:priya".into()), ..Query::default() }).await;
    assert_eq!(from.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["t-q4"]);
    assert!(threads(&mut c, Query { text: Some("bike".into()), ..Query::default() }).await.is_empty());
    let named = threads(&mut c, Query { accounts: Some(vec![AccountId::new("anon")]), text: Some("bike".into()), ..Query::default() }).await;
    assert!(!named.is_empty());
    // HTML-only mail is searchable by its text (through render) once its body is fetched.
    let garden: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t-garden") }).await.unwrap();
    let _: Body = c.call(method::BODY, MessageRef { account: AccountId::new("personal"), message: garden[0].id.clone() }).await.unwrap();
    assert!(threads(&mut c, Query { text: Some("frost".into()), ..only("personal") }).await.iter().any(|t| t.id.as_str() == "t-garden"));

    // An action: applied locally at once, then on the server.
    let rent = find_in(&unified, "personal", "t-rent").clone();
    let rent_msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: rent.account.clone(), thread: rent.id.clone() }).await.unwrap();
    let q: Value = c
        .call(method::ACT, Act { account: rent.account.clone(), action: Action::Archive { ids: vec![rent_msgs[0].id.clone()] }, caller: Caller::Person })
        .await
        .unwrap();
    assert!(q["queued"].as_i64().is_some());
    assert!(!threads(&mut c, inbox()).await.iter().any(|t| t.id == rent.id && t.account == rent.account), "gone from the inbox at once");
    let demo = env.demo("personal").clone();
    let rid = rent_msgs[0].id.clone();
    eventually!("the archive reached the server", demo.message(&rid).is_some_and(|m| m.mailboxes == vec![MailboxId::new("ARCHIVE")]));

    // New mail: synced by itself (wait_for_change wakes), announced to every connection.
    let _: Value = c.call(method::SYNC, json!({"account": "personal"})).await.unwrap();
    demo.deliver(DemoMail::new("kite", "Bo Okafor", "bo@friends.example", "Kite festival on Sunday?", "Want to fly kites on Sunday?"));
    demo.deliver(DemoMail::new("bank", "Acme Bank", "alerts@acmebank.example", "Your login code", "Your code is 771204."));
    let mut new_mail = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while new_mail.len() < 2 && Instant::now() < deadline {
        if let Some(Event::NewMail { subject, from, .. }) = tail.next_event(Duration::from_secs(1)).await {
            new_mail.push((from, subject));
        }
    }
    assert!(new_mail.iter().any(|(f, s)| f == "Bo Okafor" && s == "Kite festival on Sunday?"), "{new_mail:?}");
    assert!(new_mail.iter().any(|(_, s)| s == "A security code from Acme Bank"), "sensitive mail is one line: {new_mail:?}");
    assert!(new_mail.iter().all(|(_, s)| !s.contains("771204")));

    // Counts per AI category (triage runs in the background).
    eventually!("triage filled the counts", {
        let v: Vec<CategoryCount> = c.call(method::COUNTS, CountsRequest { accounts: None, role: Some(MailboxRole::Inbox) }).await.unwrap();
        v.iter().any(|x| x.category == Category::Newsletter) && v.iter().any(|x| x.category == Category::Sensitive)
    });

    // Bad requests get JSON-RPC errors with a core Error in `data`.
    let e = c.call::<_, Value>("no.such.method", json!({})).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    let e = c.call::<_, Value>(method::THREAD, json!({"account": 5})).await.unwrap_err();
    assert!(matches!(e, Error::Invalid(_)), "{e:?}");
    let e = c.call::<_, Value>(method::THREAD, json!({"account": "nobody", "thread": "t"})).await.unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e:?}");

    d.shutdown().await;
    assert!(!env.paths.socket().exists(), "the socket goes with the daemon");
    assert!(!env.paths.person_token().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_a_person_can_send_and_agents_see_sensitive_mail_as_one_line() {
    let env = Env::new();
    let d = env.start().await;
    synced(&env).await;
    let mut agent = env.agent().await;
    let mut person = env.person().await;

    let dinner: Vec<MessageMeta> = person.call(method::THREAD, ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t-dinner") }).await.unwrap();
    let draft = Draft {
        local_id: "reply-1".into(),
        to: vec![Address::new(Some("Maya Chen"), "maya@friends.example")],
        subject: "Re: Dinner Saturday?".into(),
        text: "Booked!".into(),
        reply_to: Some(dinner[2].id.clone()),
        ..Draft::default()
    };
    // An agent that claims to be a person is still an agent: the claim is not a capability.
    let send = Act { account: AccountId::new("personal"), action: Action::Send { draft: draft.clone() }, caller: Caller::Person };
    let e = agent.call::<_, Value>(method::ACT, &send).await.unwrap_err();
    assert_eq!(e, Error::Refused(safety::SEND_NEEDS_A_PERSON.into()));
    let as_agent = Act { caller: Caller::Agent, ..send.clone() };
    assert!(matches!(person.call::<_, Value>(method::ACT, &as_agent).await, Err(Error::Refused(_))));
    // A wrong token is refused outright.
    let e = Client::connect(&env.paths.socket(), Some("0".repeat(64)), "liar").await.err().unwrap();
    assert!(matches!(e, Error::Refused(_)), "{e:?}");
    // Agents may save drafts.
    let _: Value = agent.call(method::ACT, Act { account: AccountId::new("personal"), action: Action::SaveDraft { draft: draft.clone() }, caller: Caller::Agent }).await.unwrap();
    // The person sends.
    let _: Value = person.call(method::ACT, &send).await.unwrap();
    let demo = env.demo("personal").clone();
    eventually!("the send went out once", demo.sent().len() == 1);
    // The save's draft id reached the store, so the send used it and the server has no stray draft.
    eventually!("no drafts left on the server", demo.messages().iter().all(|m| !m.flags.draft));

    // Sensitive mail: an agent sees one line, no snippet, no body.
    let rows = threads(&mut agent, only("personal")).await;
    let code = find(&rows, "t-acme-code");
    assert_eq!(code.subject, "A security code from Acme");
    assert!(code.snippet.is_empty());
    let msgs: Vec<MessageMeta> = agent.call(method::THREAD, ThreadRef { account: AccountId::new("personal"), thread: code.id.clone() }).await.unwrap();
    assert!(msgs.iter().all(|m| !m.snippet.contains("482913") && !m.subject.contains("verification")));
    let e = agent.call::<_, Body>(method::BODY, MessageRef { account: AccountId::new("personal"), message: msgs[0].id.clone() }).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)));
    let hits = threads(&mut agent, Query { text: Some("482913".into()), ..Query::default() }).await;
    assert!(hits.iter().all(|t| !t.snippet.contains("482913")));
    // A person reads it.
    let b: Body = person.call(method::BODY, MessageRef { account: AccountId::new("personal"), message: msgs[0].id.clone() }).await.unwrap();
    assert!(b.text.unwrap().contains("482913"));
    // And no model ever saw it.
    assert!(env.prompts().iter().all(|p| !p.contains("482913")));
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_accounts_change_nothing_on_the_server() {
    let env = Env::new();
    let d = env.start().await;
    synced(&env).await;
    let mut c = env.person().await;
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: AccountId::new("ro"), thread: ThreadId::new("t-rent") }).await.unwrap();
    for action in [
        Action::Archive { ids: vec![msgs[0].id.clone()] },
        Action::SetSeen { ids: vec![msgs[0].id.clone()], seen: true },
        Action::SaveDraft { draft: Draft { local_id: "x".into(), ..Draft::default() } },
        Action::Send { draft: Draft { local_id: "y".into(), to: vec![Address::new(None, "lena@friends.example")], ..Draft::default() } },
    ] {
        let e = c.call::<_, Value>(method::ACT, Act { account: AccountId::new("ro"), action, caller: Caller::Person }).await.unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e:?}");
    }
    // Reading still works, and the body is fetched.
    let _: Body = c.call(method::BODY, MessageRef { account: AccountId::new("ro"), message: msgs[0].id.clone() }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(env.demo("ro").applied().is_empty());
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_offline_provider_is_retried_and_recovers() {
    let env = Env::new();
    let d = env.start().await;
    synced(&env).await;
    let mut c = env.person().await;
    let demo = env.demo("personal").clone();
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t-sale") }).await.unwrap();
    demo.set_offline(true);
    let _: Value = c.call(method::ACT, Act { account: AccountId::new("personal"), action: Action::Trash { ids: vec![msgs[0].id.clone()] }, caller: Caller::Person }).await.unwrap();
    eventually!("status says offline with the action queued", {
        let p = status(&mut c, "personal").await;
        !p.online && p.queued == 1 && matches!(p.error, Some(Error::Offline(_)))
    });
    // Still shown as trashed locally while offline.
    assert!(!threads(&mut c, only("personal")).await.iter().any(|t| t.id.as_str() == "t-sale" && t.mailboxes.contains(&MailboxId::new("INBOX"))));
    demo.set_offline(false);
    let id = msgs[0].id.clone();
    eventually!("the retry reached the server", demo.message(&id).is_some_and(|m| m.mailboxes == vec![MailboxId::new("TRASH")]));
    let _: Value = c.call(method::SYNC, json!({})).await.unwrap();
    eventually!("back online, queue empty", {
        let p = status(&mut c, "personal").await;
        p.online && p.queued == 0 && p.error.is_none()
    });
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_errors_wait_for_the_user_and_bad_cursors_resync() {
    let env = Env::new();
    let d = env.start().await;
    synced(&env).await;
    let mut c = env.person().await;
    let demo = env.demo("work").clone();
    demo.fail_next_sync(Error::Auth("token expired (made up)".into()));
    let _: Value = c.call(method::SYNC, json!({"account": "work"})).await.unwrap();
    eventually!("auth error shown", matches!(status(&mut c, "work").await.error, Some(Error::Auth(_))));
    // Not retried on its own.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(matches!(status(&mut c, "work").await.error, Some(Error::Auth(_))));
    // The user acts (sync.now): it tries again and recovers.
    let _: Value = c.call(method::SYNC, json!({"account": "work"})).await.unwrap();
    eventually!("auth error cleared", status(&mut c, "work").await.error.is_none());

    // The server forgets its history and a message vanished meanwhile: ResyncNeeded, then a full
    // (reset) sync drops it locally.
    let lunch = demo.find("Team lunch Friday").unwrap();
    demo.vanish(&lunch.id);
    demo.expire_history();
    demo.deliver(DemoMail::new("standup", "Tom Weller", "tom@work.example", "Standup moved to 10", "See you at 10."));
    let _: Value = c.call(method::SYNC, json!({"account": "work"})).await.unwrap();
    eventually!("the vanished thread is gone after the resync", {
        let ts = threads(&mut c, only("work")).await;
        !ts.iter().any(|t| t.id.as_str() == "t-lunch") && ts.iter().any(|t| t.id.as_str() == "t-standup")
    });
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_and_the_queue_survive_a_restart() {
    let env = Env::new();
    let d = env.start().await;
    synced(&env).await;
    // A second daemon on the same home is refused.
    let mut again = Options::new(env.paths.clone());
    again.providers.insert(AccountId::new("personal"), env.demo("personal").clone() as Arc<dyn Provider>);
    assert!(matches!(Daemon::start(again).await.err().unwrap(), Error::Refused(_)));

    let mut c = env.person().await;
    let demo = env.demo("personal").clone();
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t-hike") }).await.unwrap();
    demo.set_offline(true);
    let _: Value = c.call(method::ACT, Act { account: AccountId::new("personal"), action: Action::SetFlagged { ids: vec![msgs[0].id.clone()], flagged: true }, caller: Caller::Person }).await.unwrap();
    drop(c);
    d.shutdown().await;
    // A crash leaves a socket file behind; the next start must not trip over it.
    std::fs::write(env.paths.socket(), b"stale").unwrap();

    let d = env.start().await;
    let mut c = env.person().await;
    // Straight from the store, before any sync: the mail and the local change are there.
    let rows = threads(&mut c, only("personal")).await;
    assert!(rows.len() >= 8);
    assert!(find(&rows, "t-hike").flagged);
    let st: Vec<AccountStatus> = c.call(method::STATUS, json!({})).await.unwrap();
    assert_eq!(st.iter().find(|s| s.account.as_str() == "personal").unwrap().queued, 1);
    demo.set_offline(false);
    let id = msgs[0].id.clone();
    eventually!("the queued star reached the server after the restart", demo.message(&id).is_some_and(|m| m.flags.flagged));
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_triage_and_the_ai_methods() {
    let env = Env::new();
    let d = env.start().await;
    synced(&env).await;
    let mut c = env.person().await;
    eventually!("personal mail triaged", threads(&mut c, only("personal")).await.iter().all(|t| t.triage.is_some()));
    let rows = threads(&mut c, only("personal")).await;
    let t = |id: &str| find(&rows, id).triage.clone().unwrap();
    // Rules first: sensitive, newsletter, receipt, promotion never reach the model.
    assert_eq!((t("t-acme-code").category, t("t-acme-code").by.as_str()), (Category::Sensitive, "rules"));
    assert_eq!(t("t-acme-code").summary, "A security code from Acme");
    assert_eq!(t("t-garden").category, Category::Newsletter);
    assert_eq!(t("t-receipt").category, Category::Transactional);
    assert_eq!(t("t-sale").category, Category::Promotion);
    // People go to the (scripted) model.
    assert_eq!((t("t-dinner").category, t("t-dinner").by.as_str()), (Category::Important, "fake"));
    assert!(t("t-dinner").needs_reply);
    // The injection attempt is flagged, not obeyed, and cannot make itself urgent.
    let favour = t("t-favour");
    assert!(favour.suspicious, "{favour:?}");
    assert!(favour.priority <= 40);
    assert!(!favour.summary.to_lowercase().contains("suspicious"));

    // AI off for work: rules or a plain row, never a model.
    eventually!("work mail triaged", threads(&mut c, only("work")).await.iter().all(|t| t.triage.is_some()));
    let prompts = env.prompts();
    assert!(!prompts.is_empty());
    assert!(prompts.iter().all(|p| !p.contains("Q4 planning") && !p.contains("Team lunch")), "work mail reached a model");
    assert!(prompts.iter().all(|p| !p.contains("482913")), "a security code reached a model");
    let ci = find(&threads(&mut c, only("work")).await, "t-ci").triage.clone().unwrap();
    assert_eq!(ci.category, Category::Notification);
    let by_category = threads(&mut c, Query { category: Some(Category::Important), ..only("personal") }).await;
    assert!(by_category.iter().any(|t| t.id.as_str() == "t-dinner"));

    let dinner = ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t-dinner") };
    let s: Value = c.call(method::AI_SUMMARISE, &dinner).await.unwrap();
    assert!(s["summary"].as_str().unwrap().contains("7:30"));
    let code = ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t-acme-code") };
    let s: Value = c.call(method::AI_SUMMARISE, &code).await.unwrap();
    assert_eq!(s["summary"], "A security code from Acme");
    let x: Extracted = c.call(method::AI_EXTRACT, &dinner).await.unwrap();
    assert_eq!(x.items[0].text, "Book a table for four");
    assert!(matches!(c.call::<_, Extracted>(method::AI_EXTRACT, &code).await, Err(Error::Refused(_))));
    let req = DraftRequest { account: dinner.account.clone(), thread: Some(dinner.thread.clone()), instruction: "say it's booked".into(), reply_all: false, extra_cc: vec![] };
    let draft: Draft = c.call(method::AI_DRAFT, &req).await.unwrap();
    assert_eq!(draft.to[0].email, "maya@friends.example");
    assert!(draft.subject.starts_with("Re: Dinner"));
    assert!(draft.text.contains("booked"));
    assert!(env.demo("personal").sent().is_empty(), "a draft is never sent");
    let work = DraftRequest { account: AccountId::new("work"), thread: Some(ThreadId::new("t-q4")), instruction: "ok".into(), reply_all: false, extra_cc: vec![] };
    assert!(matches!(c.call::<_, Draft>(method::AI_DRAFT, &work).await, Err(Error::Refused(_))), "AI is off for work");

    // Ask: only non-anon accounts unless named.
    let before = env.model.calls();
    let a: Answer = c.call(method::AI_ASK, json!({"question": "when is dinner?"})).await.unwrap();
    assert!(a.text.contains("7:30"));
    assert_eq!(a.sources.len(), 1);
    let asked: Vec<String> = env.prompts().into_iter().skip(before).collect();
    assert!(asked.iter().all(|p| !p.contains("blue bike")));
    let t: Triage = c.call(method::AI_TRIAGE, &dinner).await.unwrap();
    assert_eq!(t.category, Category::Important);
    d.shutdown().await;
}

/// A stand-in keyring: what `accounts.add` stored (never a real keyring in a test).
#[derive(Default)]
struct FakeKeyring(std::sync::Mutex<Vec<(String, String)>>);

impl lumbridge_maild::secrets::SecretStore for FakeKeyring {
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.0.lock().unwrap().push((account.into(), secret.into()));
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap().retain(|(a, _)| a != account);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_adds_accounts_from_nothing() {
    // First run: no config at all.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home.clone().into_os_string()));
    let keyring = Arc::new(FakeKeyring::default());
    let mut o = Options::new(paths.clone());
    o.secrets = Some(keyring.clone());
    // The gog and IMAP accounts below are served by made-up mail: no gog, no server, no real keyring read.
    for (id, email) in [("sam-gmail", "sam@gmail.example"), ("box", "sam@box.example")] {
        o.providers.insert(AccountId::new(id), Arc::new(DemoProvider::new(AccountId::new(id), DemoSet::Personal, Address::new(Some("Sam"), email))) as Arc<dyn Provider>);
    }
    let d = Daemon::start(o).await.unwrap();
    let token = Client::read_person_token(&paths.person_token()).unwrap();
    let mut c = Client::connect(&paths.socket(), Some(token), "test person").await.unwrap();
    let hello: Hello = c.call(method::HELLO, json!({})).await.unwrap();
    assert!(hello.person, "the token was accepted");
    let none: Vec<Account> = c.call(method::ACCOUNTS, json!({})).await.unwrap();
    assert!(none.is_empty());

    let found: Discovered = c.call(method::ACCOUNTS_DISCOVER, json!({"email": "sam@fastmail.com"})).await.unwrap();
    assert_eq!(found.imap_host, "imap.fastmail.com");

    // "Try with demo mail".
    for n in lumbridge_maild::setup::demo_accounts() {
        let a: Account = c.call(method::ACCOUNTS_ADD, &n).await.unwrap();
        assert_eq!(a.provider, ProviderKind::Demo);
    }
    let all: Vec<Account> = c.call(method::ACCOUNTS, json!({})).await.unwrap();
    assert_eq!(all.len(), 3);
    eventually!("demo mail syncs", !threads(&mut c, only("demo-work")).await.is_empty());

    // A gog account, read-only, and an IMAP one whose password goes to the keyring only.
    let gog = NewAccount { id: None, persona: Persona::Personal, label: String::new(), email: "sam@gmail.example".into(), name: "Sam".into(),
                           readonly: true, provider: NewProvider::Gog { binary: Some("/nonexistent/gog".into()), ssh_host: None }, password: None };
    let a: Account = c.call(method::ACCOUNTS_ADD, &gog).await.unwrap();
    assert!(a.readonly);
    let imap = NewAccount { id: Some("box".into()), persona: Persona::Work, label: "Box".into(), email: "sam@box.example".into(), name: "Sam".into(),
                            readonly: false, provider: NewProvider::Imap { imap_host: "127.0.0.1".into(), imap_port: Some(1), imap_security: Some("plain".into()),
                            smtp_host: None, smtp_port: None, smtp_security: None, username: None }, password: Some("made-up-secret".into()) };
    let _: Account = c.call(method::ACCOUNTS_ADD, &imap).await.unwrap();
    assert_eq!(keyring.0.lock().unwrap().as_slice(), &[("box".to_string(), "made-up-secret".to_string())]);
    let text = std::fs::read_to_string(paths.config_file()).unwrap();
    assert!(!text.contains("made-up-secret"), "never in the config");
    assert!(text.contains("readonly = true"));
    // The same id again is refused; so is an agent.
    let again = c.call::<_, Account>(method::ACCOUNTS_ADD, &imap).await.unwrap_err();
    assert!(matches!(again, Error::Invalid(_)), "{again:?}");
    let mut agent = Client::connect(&paths.socket(), None, "test agent").await.unwrap();
    let e = agent.call::<_, Account>(method::ACCOUNTS_ADD, &gog).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e:?}");
    let hello: Hello = agent.call(method::HELLO, json!({})).await.unwrap();
    assert!(!hello.person);
    let all: Vec<Account> = c.call(method::ACCOUNTS, json!({})).await.unwrap();
    assert_eq!(all.len(), 5);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deep_home_still_gets_a_socket_that_fits() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("x".repeat(90));
    std::fs::create_dir_all(&home).unwrap();
    let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home.clone().into_os_string()));
    assert!(paths.runtime_dir.join("maild.sock").as_os_str().len() > lumbridge_mail_core::paths::MAX_SOCKET_PATH);
    let d = Daemon::start(Options::new(paths.clone())).await.unwrap();
    assert!(paths.socket().starts_with("/tmp"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(paths.socket_dir()).unwrap().permissions().mode() & 0o777, 0o700);
    let token = Client::read_person_token(&paths.person_token()).unwrap();
    let mut c = Client::connect(&paths.socket(), Some(token), "test").await.unwrap();
    let hello: Hello = c.call(method::HELLO, json!({})).await.unwrap();
    assert!(hello.person);
    let dir_of_socket = paths.socket_dir();
    d.shutdown().await;
    let _ = std::fs::remove_dir_all(dir_of_socket);
}
