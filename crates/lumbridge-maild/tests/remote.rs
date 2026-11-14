//! The remote daemon over ssh, with a fake `ssh` (tests/fake-ssh.sh): it logs its arguments, skips
//! the options, and runs the remote command here with the "server's" LUMBRIDGE_MAIL_HOME, exactly as
//! sshd would hand it to the login shell. The daemon runs in-process on that home with made-up mail;
//! the client side is the real `lbmail` binary and `remote::connect`. Made-up mail only.

use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::remote::Remote;
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_maild::{Client, Daemon, DemoProvider, DemoSet, Options};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LBMAIL: &str = env!("CARGO_BIN_EXE_lbmail");

const SERVER_CONFIG: &str = r##"
[daemon]
sync_interval_s = 30

# The tests make things happen through agent connections (stars).
[agents]
actions = true

[ai]
enabled = false

[[accounts]]
id = "personal"
persona = "personal"
label = "Personal"
email = "sam@example.com"
name = "Sam Example"
[accounts.provider]
kind = "demo"
"##;

/// A "server" (a daemon home) and a "client" (another home whose config names the server), with a
/// fake ssh between them.
struct Net {
    _dir: tempfile::TempDir,
    server: Paths,
    client_home: PathBuf,
    ssh: PathBuf,
    log: PathBuf,
    demo: Arc<DemoProvider>,
}

impl Net {
    fn new() -> Net {
        let dir = tempfile::tempdir().unwrap();
        let server_home = dir.path().join("server");
        let server = Paths::resolve(|k| (k == HOME_VAR).then(|| server_home.clone().into_os_string()));
        std::fs::create_dir_all(&server.config_dir).unwrap();
        std::fs::write(server.config_file(), SERVER_CONFIG).unwrap();
        let client_home = dir.path().join("client");
        std::fs::create_dir_all(client_home.join("config")).unwrap();
        // The fake ssh, with this server's home and log baked in (so tests need no shared environment).
        let log = dir.path().join("ssh.log");
        let fake = dir.path().join("fake-ssh.sh");
        std::fs::write(&fake, include_str!("fake-ssh.sh")).unwrap();
        let ssh = dir.path().join("ssh");
        std::fs::write(&ssh, format!("#!/bin/sh\nFAKE_SSH_SERVER_HOME='{}' FAKE_SSH_LOG='{}' exec /bin/sh '{}' \"$@\"\n",
                                     server.runtime_dir.display(), log.display(), fake.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The client's config names the server; it has no accounts of its own.
        std::fs::write(
            client_home.join("config/config.toml"),
            format!("[daemon]\nremote = \"fakehost\"\nssh = \"{}\"\nremote_lbmail = \"{LBMAIL}\"\n", ssh.display()),
        )
        .unwrap();
        let demo = Arc::new(DemoProvider::new(AccountId::new("personal"), DemoSet::Personal, Address::new(Some("Sam Example"), "sam@example.com")));
        Net { _dir: dir, server, client_home, ssh, log, demo }
    }

    async fn start(&self) -> Daemon {
        let mut o = Options::new(self.server.clone());
        o.providers.insert(AccountId::new("personal"), self.demo.clone() as Arc<dyn Provider>);
        Daemon::start(o).await.unwrap()
    }

    /// Waits for the first sync and its events to be over, so a test sees only the events it causes.
    async fn settled(&self) {
        let mut c = Client::connect(&self.server.socket(), None, "settle").await.unwrap();
        let start = Instant::now();
        loop {
            let st: Vec<AccountStatus> = c.call(method::STATUS, json!({})).await.unwrap();
            if st.iter().all(|s| s.last_sync.is_some()) || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        while c.next_event(Duration::from_millis(700)).await.is_some() {}
    }

    fn remote(&self) -> Remote {
        let mut r = Remote::new("fakehost").unwrap();
        r.ssh = self.ssh.display().to_string();
        r.lbmail = LBMAIL.into();
        r
    }

    /// The real `lbmail` as a client on the client machine (no terminal: an agent).
    fn lbmail(&self, args: &[&str]) -> Command {
        let mut c = Command::new(LBMAIL);
        c.args(args).env(HOME_VAR, &self.client_home).env_remove("LBMAIL_REMOTE").env_remove("LBMAIL_AGENT");
        c.stdin(Stdio::null());
        c
    }

    fn ssh_log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// `ssh fakehost lbmail pipe` by hand, the way an agent could run it.
    fn raw_pipe(&self) -> std::process::Child {
        let mut c = Command::new(&self.ssh);
        c.args(["-T", "fakehost", &format!("{LBMAIL} pipe")]);
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }
}

/// Makes something happen on the server: (un)stars the newest thread, so every client gets an event.
async fn poke(c: &mut Client, on: bool) {
    let page: ThreadPage = c.call(method::THREADS, Query { limit: 1, ..Query::default() }).await.unwrap();
    let Some(t) = page.threads.first() else { return };
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: t.account.clone(), thread: t.id.clone() }).await.unwrap();
    let act = Act { account: t.account.clone(), action: Action::SetFlagged { ids: msgs.iter().map(|m| m.id.clone()).collect(), flagged: on }, caller: Caller::Agent };
    let _: Value = c.call(method::ACT, &act).await.unwrap();
}

async fn output(mut c: Command) -> std::process::Output {
    tokio::task::spawn_blocking(move || c.output().unwrap()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_and_answers_go_through_ssh() {
    let net = Net::new();
    let d = net.start().await;

    let mut c = lumbridge_maild::remote::connect(&net.remote(), None, false, "test").await.unwrap();
    assert_eq!(c.via(), "remote:fakehost");
    let accounts: Vec<Account> = c.call(method::ACCOUNTS, json!({})).await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].email, "sam@example.com");
    let err = c.call::<_, Value>("no.such.method", json!({})).await.unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "errors keep their kind across ssh: {err:?}");

    // The real lbmail, pointed at the server by its config.
    let out = output(net.lbmail(&["status"])).await;
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text} {}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("remote:fakehost (as agent)"), "{text}");
    assert!(text.contains("personal"), "{text}");
    let out = output(net.lbmail(&["--json", "status"])).await;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["via"], "remote:fakehost");
    assert_eq!(v["person"], false);
    // `--remote local` (and an explicit --socket) mean this machine: nothing runs there.
    let out = output(net.lbmail(&["--remote", "local", "status"])).await;
    assert_eq!(out.status.code(), Some(3), "offline: no local daemon on the client");
    let log = net.ssh_log();
    assert!(log.contains("-T") && log.contains("BatchMode=yes") && log.contains("fakehost"), "{log}");
    assert!(log.contains("ControlMaster=auto"), "lbmail shares one connection: {log}");
    // Accounts live on the server: lbmail says where to run it instead of editing this machine's file.
    let out = output(net.lbmail(&["account", "list"])).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ssh -t fakehost"), "{}", String::from_utf8_lossy(&out.stderr));
    drop(c);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_stream_over_ssh() {
    let net = Net::new();
    let d = net.start().await;
    let mut remote = lumbridge_maild::remote::connect(&net.remote(), None, false, "test").await.unwrap();
    // Something happens on the server: a local person stars a thread.
    let token = Client::read_person_token(&net.server.person_token()).unwrap();
    let mut local = Client::connect(&net.server.socket(), Some(token), "local").await.unwrap();
    let start = Instant::now();
    let mut got = None;
    let mut on = true;
    while got.is_none() && start.elapsed() < Duration::from_secs(20) {
        poke(&mut local, on).await;
        on = !on;
        got = remote.next_event(Duration::from_millis(500)).await;
    }
    assert!(got.is_some(), "an event reached the remote client");
    assert!(!remote.is_closed());

    // And through the real `lbmail tail`.
    let child = net.lbmail(&["--json", "tail", "--count", "1", "--timeout", "20"]).stdout(Stdio::piped()).spawn().unwrap();
    let mut s = lumbridge_maild::remote::connect(&net.remote(), None, false, "trigger").await.unwrap();
    let waiter = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap());
    let trig = tokio::spawn(async move {
        let mut on = true;
        loop {
            poke(&mut s, on).await;
            on = !on;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });
    let out = waiter.await.unwrap();
    trig.abort();
    let line = String::from_utf8_lossy(&out.stdout);
    let ev: Value = serde_json::from_str(line.lines().next().unwrap_or("null")).unwrap();
    assert!(ev["type"].is_string(), "{line}");
    drop((remote, local));
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tail_reconnects_after_the_pipe_dies() {
    let net = Net::new();
    let d = net.start().await;
    net.settled().await;
    let cut = net.client_home.join("cut-once");
    // The first pipe is cut after a second, like a dropped ssh connection.
    let child = net.lbmail(&["--json", "tail", "--count", "1", "--timeout", "30"]).env("FAKE_SSH_CUT", &cut).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let waiter = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap());
    let start = Instant::now();
    while !cut.exists() && start.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(cut.exists(), "the first pipe started");
    // Only after the cut (and the reconnect) does anything happen.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let mut s = lumbridge_maild::remote::connect(&net.remote(), None, false, "trigger").await.unwrap();
    let trig = tokio::spawn(async move {
        let mut on = true;
        loop {
            poke(&mut s, on).await;
            on = !on;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });
    let out = waiter.await.unwrap();
    trig.abort();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout} {}", String::from_utf8_lossy(&out.stderr));
    let ev: Value = serde_json::from_str(stdout.lines().next().unwrap_or("null")).unwrap();
    assert!(ev["type"].is_string(), "an event after the reconnect: {stdout}");
    let pipes = net.ssh_log().lines().filter(|l| l.ends_with(" pipe")).count();
    assert!(pipes >= 3, "tail dialled again (plus the trigger's pipe): {}", net.ssh_log());
    d.shutdown().await;
}

/// The person rule over ssh: `lbmail pipe` adds nothing, so whoever can run it is an agent unless it
/// shows the token itself; lbmail without a terminal never fetches it; agents never get it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssh_never_makes_an_agent_a_person() {
    let net = Net::new();
    let d = net.start().await;

    // An agent with ssh access runs the pipe by hand and claims to be a person.
    let mut p = net.raw_pipe();
    let mut w = p.stdin.take().unwrap();
    let mut r = BufReader::new(p.stdout.take().unwrap());
    let mut ask = |line: Value| -> Value {
        writeln!(w, "{line}").unwrap();
        w.flush().unwrap();
        loop {
            let mut l = String::new();
            assert!(r.read_line(&mut l).unwrap() > 0, "pipe closed");
            let v: Value = serde_json::from_str(&l).unwrap();
            if v.get("id") == line.get("id") {
                return v;
            }
        }
    };
    let h = ask(json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": {"client": "agent"}}));
    assert_eq!(h["result"]["person"], false, "the pipe added no token: {h}");
    let draft = Draft { local_id: "d1".into(), subject: "Hi".into(), to: vec![Address::new(None, "maya@postbox.example")], ..Default::default() };
    let act = Act { account: AccountId::new("personal"), action: Action::Send { draft }, caller: Caller::Person };
    let sent = ask(json!({"jsonrpc": "2.0", "id": 2, "method": "act", "params": act}));
    assert_eq!(sent["error"]["data"]["kind"], "refused", "no send from an agent over ssh: {sent}");
    let guess = ask(json!({"jsonrpc": "2.0", "id": 3, "method": "hello", "params": {"person_token": "0123abcd"}}));
    assert_eq!(guess["error"]["data"]["kind"], "refused", "a wrong token is refused: {guess}");
    drop(w);
    let _ = p.wait();

    // lbmail without a terminal (an agent's tool) never asks for the token.
    let before = net.ssh_log();
    assert!(!before.contains("person-token"), "{before}");
    let out = output(net.lbmail(&["status"])).await;
    assert!(String::from_utf8_lossy(&out.stdout).contains("(as agent)"));
    assert!(!net.ssh_log().contains("person-token"), "{}", net.ssh_log());

    // The server end refuses the token to an agent, by flag or by environment.
    for (extra, env) in [(&["--agent", "person-token"][..], "0"), (&["person-token"], "1")] {
        let mut c = Command::new(LBMAIL);
        c.args(extra).env(HOME_VAR, &net.server.runtime_dir).env("LBMAIL_AGENT", env).stdin(Stdio::null());
        let out = output(c).await;
        assert_eq!(out.status.code(), Some(4), "{extra:?}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty());
    }
    // ... and gives it, into a pipe, to the app or lbmail-at-a-terminal on another machine.
    let token = Client::read_person_token(&net.server.person_token()).unwrap();
    let fetched = lumbridge_maild::remote::fetch_token(&net.remote(), None).await.unwrap();
    assert_eq!(fetched, token);
    let mut person = lumbridge_maild::remote::connect(&net.remote(), None, true, "person").await.unwrap();
    assert!(person.is_person());
    let h: Hello = person.call(method::HELLO, json!({})).await.unwrap();
    assert!(h.person);
    d.shutdown().await;

    // A restarted daemon has a new token: the person client fetches it again.
    let d = net.start().await;
    let again = lumbridge_maild::remote::connect(&net.remote(), None, true, "person").await.unwrap();
    assert!(again.is_person());
    assert_ne!(Client::read_person_token(&net.server.person_token()).unwrap(), token);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_daemon_or_no_host_is_offline_with_the_reason() {
    let net = Net::new();
    // No daemon on the server: the pipe says so at once.
    let err = lumbridge_maild::remote::connect(&net.remote(), None, false, "t").await.err().unwrap();
    assert!(matches!(&err, Error::Offline(m) if m.contains("remote:fakehost") && m.contains("no daemon")), "{err:?}");
    let err = lumbridge_maild::remote::connect(&net.remote(), None, true, "t").await.err().unwrap();
    assert!(matches!(&err, Error::Offline(m) if m.contains("no daemon")), "{err:?}");
    // A host ssh cannot reach.
    let mut r = net.remote();
    r.host = "unreachable".into();
    let err = lumbridge_maild::remote::connect(&r, None, false, "t").await.err().unwrap();
    assert!(matches!(&err, Error::Offline(m) if m.contains("Could not resolve")), "{err:?}");
    let out = output(net.lbmail(&["--remote", "unreachable", "ls"])).await;
    assert_eq!(out.status.code(), Some(3));
    // lbmail quit against a remote never writes a marker on this machine.
    let out = output(net.lbmail(&["quit"])).await;
    assert_eq!(out.status.code(), Some(3));
    let client_paths = Paths::under(&net.client_home);
    assert!(!client_paths.stopped_marker().exists());
    assert!(!Path::new(&client_paths.socket()).exists(), "nothing started a daemon on the client");
}
