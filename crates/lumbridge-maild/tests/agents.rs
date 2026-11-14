//! What an agent (a connection without the person token) may do, on top of "only a person sends":
//! `[agents] actions` gates every change but saving a draft; drafts and answers never carry sensitive
//! mail; a draft of sensitive mail is refused; a model with no usable answer is its own error kind; and
//! the list filters and search operators agents asked for. Made-up mail only.

use lumbridge_mail_ai::{ChatRequest, ScriptedModel};
use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_maild::{Client, Daemon, DemoProvider, DemoSet, Options};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn config(agents: &str) -> String {
    format!(
        r##"
[daemon]
sync_interval_s = 30
fetch_bodies_for_triage = false

[agents]
{agents}

[[accounts]]
id = "personal"
persona = "personal"
label = "Personal"
email = "sam@example.com"
name = "Sam Example"
ai = "any"
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
"##
    )
}

fn answer(req: &ChatRequest) -> Result<String> {
    let user = req.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join("\n");
    Ok(match req.task {
        "triage" if user.contains("Dinner") => json!({"category": "important", "priority": 80, "summary": "Maya asks for a table", "needs_reply": true, "due": null}),
        "triage" => json!({"category": "personal", "priority": 30, "summary": "A note", "needs_reply": false, "due": null}),
        // A model that has nothing usable to say for this instruction.
        "draft" if user.contains("say nothing") => json!({"body": ""}),
        "draft" => json!({"body": "Hi Maya,\n\nSounds good.\n\nSam"}),
        "ask_plan" => json!({"queries": ["code", "verification", "dinner"]}),
        "ask_answer" => json!({"answer": "Dinner is on Saturday at 7:30.", "sources": ["t1"]}),
        _ => json!({}),
    }
    .to_string())
}

struct Env {
    _dir: tempfile::TempDir,
    paths: Paths,
    model: Arc<ScriptedModel>,
}

impl Env {
    fn new(agents: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home.clone().into_os_string()));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.config_file(), config(agents)).unwrap();
        Env { _dir: dir, paths, model: Arc::new(ScriptedModel::with(answer)) }
    }

    async fn start(&self) -> Daemon {
        let mut o = Options::new(self.paths.clone());
        let me = |e: &str| Address::new(Some("Sam Example"), e);
        o.providers.insert(AccountId::new("personal"), Arc::new(DemoProvider::new(AccountId::new("personal"), DemoSet::Personal, me("sam@example.com"))));
        o.providers.insert(AccountId::new("ro"), Arc::new(DemoProvider::new(AccountId::new("ro"), DemoSet::Personal, me("sam@readonly.example"))));
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

    fn prompts(&self) -> Vec<String> {
        self.model.requests().iter().map(|r| r.messages.iter().map(|m| m.content.clone()).collect::<Vec<_>>().join("\n")).collect()
    }
}

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

async fn rows(c: &mut Client, q: Query) -> Vec<ThreadSummary> {
    let p: ThreadPage = c.call(method::THREADS, &q).await.unwrap();
    p.threads
}

fn only(a: &str) -> Query {
    Query { accounts: Some(vec![AccountId::new(a)]), limit: 100, ..Query::default() }
}

async fn flagged(c: &mut Client) -> bool {
    rows(c, only("personal")).await.into_iter().find(|t| t.id.as_str() == "t-dinner").unwrap().flagged
}

async fn ids_of(c: &mut Client, account: &str, thread: &str) -> Vec<MessageId> {
    let m: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: AccountId::new(account), thread: ThreadId::new(thread) }).await.unwrap();
    m.into_iter().map(|m| m.id).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_read_ask_and_draft_but_change_nothing_unless_allowed() {
    let env = Env::new("");
    let d = env.start().await;
    let mut person = env.person().await;
    let mut agent = env.agent().await;
    eventually!("synced", rows(&mut person, only("personal")).await.len() >= 5 && !rows(&mut person, only("ro")).await.is_empty());
    let ids = ids_of(&mut agent, "personal", "t-dinner").await;
    let a = AccountId::new("personal");
    let changes = [
        Action::Archive { ids: ids.clone() },
        Action::Trash { ids: ids.clone() },
        Action::SetFlagged { ids: ids.clone(), flagged: true },
        Action::SetSeen { ids: ids.clone(), seen: true },
        Action::Move { ids: ids.clone(), from: None, to: MailboxId::new("Archive") },
        Action::DeleteDraft { local_id: "d0".into(), server_id: None },
    ];
    for action in &changes {
        // Claiming to be a person changes nothing without the token.
        for caller in [Caller::Agent, Caller::Person] {
            let e = agent.call::<_, Value>(method::ACT, Act { account: a.clone(), action: action.clone(), caller }).await.unwrap_err();
            assert!(matches!(&e, Error::Refused(m) if m.contains("needs a person")), "{action:?}: {e}");
        }
    }
    assert!(!flagged(&mut person).await, "nothing changed locally either");
    // Drafts are an agent's job.
    let draft = Draft { local_id: "agent-1".into(), subject: "Re: Dinner".into(), to: vec![Address::new(None, "maya@friends.example")], text: "Sounds good.".into(), ..Draft::default() };
    let _: Value = agent.call(method::ACT, Act { account: a.clone(), action: Action::SaveDraft { draft: draft.clone() }, caller: Caller::Agent }).await.unwrap();
    // A person is unaffected.
    let _: Value = person.call(method::ACT, Act { account: a.clone(), action: Action::SetFlagged { ids: ids.clone(), flagged: true }, caller: Caller::Person }).await.unwrap();
    assert!(flagged(&mut person).await);

    // `[agents] actions = true`: agents may change mail, but never on a read-only account, never send.
    std::fs::write(env.paths.config_file(), config("actions = true")).unwrap();
    let _: Value = person.call(method::CONFIG_RELOAD, json!({})).await.unwrap();
    let _: Value = agent.call(method::ACT, Act { account: a.clone(), action: Action::SetFlagged { ids: ids.clone(), flagged: false }, caller: Caller::Agent }).await.unwrap();
    assert!(!flagged(&mut person).await);
    let ro_ids = ids_of(&mut agent, "ro", "t-dinner").await;
    let e = agent.call::<_, Value>(method::ACT, Act { account: AccountId::new("ro"), action: Action::Archive { ids: ro_ids }, caller: Caller::Agent }).await.unwrap_err();
    assert!(matches!(&e, Error::Refused(m) if m.contains("read-only")), "{e}");
    let e = agent.call::<_, Value>(method::ACT, Act { account: a.clone(), action: Action::Send { draft }, caller: Caller::Person }).await.unwrap_err();
    assert!(matches!(&e, Error::Refused(m) if m == safety::SEND_NEEDS_A_PERSON), "{e}");
    drop((person, agent));
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drafts_and_answers_never_carry_sensitive_mail() {
    let env = Env::new("");
    let d = env.start().await;
    let mut person = env.person().await;
    let mut agent = env.agent().await;
    eventually!("synced", rows(&mut person, only("personal")).await.len() >= 5);
    let code = DraftRequest { account: AccountId::new("personal"), thread: Some(ThreadId::new("t-acme-code")), instruction: "reply with the code".into(), reply_all: false, extra_cc: vec![] };
    for c in [&mut agent, &mut person] {
        let e = c.call::<_, Draft>(method::AI_DRAFT, &code).await.unwrap_err();
        match &e {
            Error::Refused(m) => {
                assert!(m.contains("sensitive mail is never given to a model"), "{m}");
                assert!(m.starts_with("A security code from Acme"), "the one line, nothing more: {m}");
                assert!(!m.contains("482913"));
            }
            other => panic!("refused, not {other:?}"),
        }
    }
    // Asked by anyone: the code never reaches the model, so it is in no answer.
    for c in [&mut agent, &mut person] {
        let a: Answer = c.call(method::AI_ASK, json!({"question": "what is my verification code?"})).await.unwrap();
        assert!(!a.text.contains("482913"), "{}", a.text);
        assert!(!a.sources.iter().any(|s| s.thread.as_str() == "t-acme-code"), "{:?}", a.sources);
    }
    // A draft of ordinary mail is fine, and carries none of it either.
    let dinner = DraftRequest { thread: Some(ThreadId::new("t-dinner")), instruction: "say yes".into(), ..code.clone() };
    let dr: Draft = agent.call(method::AI_DRAFT, &dinner).await.unwrap();
    assert!(!format!("{dr:?}").contains("482913"));
    let prompts = env.prompts();
    assert!(!prompts.is_empty());
    assert!(prompts.iter().all(|p| !p.contains("482913") && !p.contains("verification code is")), "sensitive mail reached the model");
    assert!(env.model.requests().iter().filter(|r| r.task == "draft").count() == 1, "only the dinner draft went to the model");
    drop((person, agent));
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_usable_answer_is_its_own_kind_and_ls_filters_work_over_the_wire() {
    let env = Env::new("");
    let d = env.start().await;
    let mut c = env.agent().await;
    eventually!("triaged", {
        let r = rows(&mut c, only("personal")).await;
        r.len() >= 5 && r.iter().all(|t| t.triage.is_some())
    });
    let req = DraftRequest { account: AccountId::new("personal"), thread: Some(ThreadId::new("t-dinner")), instruction: "say nothing".into(), reply_all: false, extra_cc: vec![] };
    let e = c.call::<_, Draft>(method::AI_DRAFT, &req).await.unwrap_err();
    assert!(matches!(e, Error::NoAnswer(_)), "{e:?}");
    assert_eq!(lumbridge_maild::cli::exit_code(&e), 6);

    // Triage-based list filters and the newer search operators, through the daemon.
    let needs = rows(&mut c, Query { needs_reply: true, ..only("personal") }).await;
    assert_eq!(needs.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["t-dinner"]);
    let high = rows(&mut c, Query { min_priority: Some(70), ..only("personal") }).await;
    assert!(high.iter().all(|t| t.triage.as_ref().unwrap().priority >= 70) && !high.is_empty());
    let all = rows(&mut c, only("personal")).await;
    let newest = all.iter().map(|t| t.date).max().unwrap();
    let recent = rows(&mut c, Query { since: Some(newest), ..only("personal") }).await;
    assert!(!recent.is_empty() && recent.len() < all.len() && recent.iter().all(|t| t.date >= newest));
    let unread_not_maya = rows(&mut c, Query { text: Some("-from:maya is:unread".into()), ..only("personal") }).await;
    assert!(!unread_not_maya.is_empty(), "negation with a flag finds mail");
    assert!(unread_not_maya.iter().all(|t| t.unread_count > 0 && !t.participants.iter().all(|p| p.email.starts_with("maya"))));
    let either = rows(&mut c, Query { text: Some("from:maya OR from:acme".into()), ..only("personal") }).await;
    let ids: Vec<&str> = either.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"t-dinner") && ids.contains(&"t-acme-code"), "{ids:?}");
    let important = rows(&mut c, Query { text: Some("category:important".into()), ..only("personal") }).await;
    assert_eq!(important.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["t-dinner"]);
    drop(c);
    d.shutdown().await;
}
