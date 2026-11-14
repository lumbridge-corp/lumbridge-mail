//! The app's real `Rpc` on a daemon "on another machine": through a fake `ssh` (the maild crate's
//! tests/fake-ssh.sh, which runs the remote command here in the server's home) and the real
//! `lbmail pipe` / `lbmail person-token`. Covers: answers and events, the person token fetched over
//! ssh, an agent link staying an agent, reconnecting after the pipe is cut and after the daemon
//! restarts (new token), a host ssh cannot reach, and that nothing ever starts a daemon here.
//!
//! Needs the `lbmail` binary built next to this test (`cargo test --workspace` builds it); without it
//! the test says so and stops.

use futures::executor::block_on;
use lumbridge_mail_app::backend::rpc::Rpc;
use lumbridge_mail_app::backend::{Backend, LinkState, Problem};
use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::remote::Remote;
use lumbridge_mail_core::rpc::{Act, AttachmentRef, Caller};
use lumbridge_mail_core::{AccountId, Action, Query};
use lumbridge_mail_e2e::{wait_real, MailEnv};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

fn wait_state(rpc: &Rpc, what: &str, want: impl Fn(&LinkState) -> bool) {
    wait_real(what, Duration::from_secs(30), || want(&rpc.state()));
}

/// Star or unstar the newest thread of the writable `personal` account through `rpc`, so the daemon
/// sends an event. Not the unified newest: `ro` (read-only) carries the same mail as `work`, so the
/// newest thread is a tie between the two, broken by which account synced last, and a star on `ro`
/// is refused.
fn poke(rpc: &Rpc, on: bool) {
    let page = block_on(rpc.threads(Query { accounts: Some(vec![AccountId::new("personal")]), limit: 1, ..Query::default() })).unwrap();
    let t = &page.threads[0];
    let msgs = block_on(rpc.thread(lumbridge_mail_core::rpc::ThreadRef { account: t.account.clone(), thread: t.id.clone() })).unwrap();
    let act = Act { account: t.account.clone(), action: Action::SetFlagged { ids: msgs.into_iter().map(|m| m.id).collect(), flagged: on }, caller: Caller::Person };
    block_on(rpc.act(act)).unwrap();
}

fn main() {
    let exe = std::env::current_exe().unwrap();
    let lbmail = exe.parent().and_then(|d| d.parent()).map(|d| d.join("lbmail")).unwrap();
    if !lbmail.is_file() {
        println!("remote_app: SKIPPED, {} is not built (run cargo test --workspace)", lbmail.display());
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // This process is the client machine: its own home, with no daemon and no accounts.
    let client_home = dir.path().join("client");
    std::fs::create_dir_all(&client_home).unwrap();
    // SAFETY: first thing in a single-threaded test binary, before any thread exists.
    unsafe { std::env::set_var(HOME_VAR, &client_home) };
    let client = Paths::from_env();

    let mut env = MailEnv::new();
    env.start();
    env.wait_synced(4);

    let log = dir.path().join("ssh.log");
    let fake = dir.path().join("fake-ssh.sh");
    std::fs::write(&fake, include_str!("../../lumbridge-maild/tests/fake-ssh.sh")).unwrap();
    let cut = dir.path().join("cut-once");
    let ssh = dir.path().join("ssh");
    std::fs::write(&ssh, format!(
        "#!/bin/sh\nFAKE_SSH_SERVER_HOME='{}' FAKE_SSH_LOG='{}' FAKE_SSH_CUT=\"${{CUT:-}}\" exec /bin/sh '{}' \"$@\"\n",
        env.paths.runtime_dir.display(), log.display(), fake.display())).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut remote = Remote::new("fakehost").unwrap();
    remote.ssh = ssh.display().to_string();
    remote.lbmail = lbmail.display().to_string();

    // 1. Answers, the person token over ssh, events.
    let rpc = Rpc::remote(remote.clone());
    let link = rpc.link().unwrap();
    let events = rpc.events();
    assert_eq!(rpc.name(), "maild on fakehost");
    let hello = block_on(rpc.hello()).expect("hello over ssh");
    assert!(hello.person && rpc.is_person(), "the app fetched the token over ssh and the daemon took it");
    assert_eq!(block_on(rpc.accounts()).unwrap().len(), 4);
    poke(&rpc, true);
    wait_real("an event over ssh", Duration::from_secs(10), || events.try_recv().is_ok());
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(log_text.contains("person-token") && log_text.contains(" pipe"), "{log_text}");
    assert!(log_text.contains("ControlMaster=auto") && log_text.contains("-T"), "{log_text}");
    let token = std::fs::read_to_string(env.paths.person_token()).unwrap();
    assert!(!log_text.contains(token.trim()), "the token never goes on a command line: {log_text}");
    println!("ok  answers, events and a person over ssh");

    // 1b. An attachment comes over the same ssh, in pieces, into this machine's private cache (the
    // client's own home), never a path on the server.
    let hike = block_on(rpc.thread(lumbridge_mail_core::rpc::ThreadRef { account: AccountId::new("personal"), thread: lumbridge_mail_core::ThreadId::new("t-hike") })).unwrap();
    let aref = AttachmentRef { account: AccountId::new("personal"), message: hike[0].id.clone(), attachment: lumbridge_mail_core::AttachmentId::new("a1") };
    let steps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let progress: lumbridge_mail_app::backend::Progress = { let s = steps.clone(); std::sync::Arc::new(move |d, t| s.lock().unwrap().push((d, t))) };
    let path = block_on(rpc.attachment_progress(aref.clone(), progress)).expect("attachment over ssh");
    assert!(path.starts_with(client.attachments_dir()), "{} is in the client's cache", path.display());
    assert!(!path.starts_with(&env.paths.cache_dir), "not the server's cache");
    assert_eq!(path.file_name().unwrap(), "hike-map.pdf");
    assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.4 made-up map\n");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(steps.lock().unwrap().last(), Some(&(21, 21)));
    assert_eq!(block_on(rpc.attachment(aref)).unwrap(), path, "a second open reuses the copy");
    println!("ok  attachments are copied over ssh into the client's cache");

    // The app never stops a daemon on another machine: no Quit, and the call is refused here.
    assert!(!rpc.can_quit_daemon());
    assert!(matches!(block_on(rpc.quit_daemon()), Err(lumbridge_mail_core::Error::Refused(_))));
    let info = block_on(rpc.set_paused(true)).unwrap();
    assert!(info.paused, "pause works over ssh");
    assert!(!block_on(rpc.set_paused(false)).unwrap().paused);
    assert!(env.paths.socket().exists(), "the remote daemon is still there");
    println!("ok  pause/resume over ssh, never quit");

    // 2. An agent link over the same ssh stays an agent: no token fetched, no send.
    let agent = Rpc::remote_agent(remote.clone());
    assert!(!block_on(agent.hello()).unwrap().person);
    let draft = lumbridge_mail_core::Draft { local_id: "d1".into(), subject: "Hi".into(),
                                             to: vec![lumbridge_mail_core::Address::new(None, "maya@postbox.example")], ..Default::default() };
    let sent = block_on(agent.act(Act { account: lumbridge_mail_core::AccountId::new("personal"), action: Action::Send { draft }, caller: Caller::Person }));
    assert!(matches!(sent, Err(lumbridge_mail_core::Error::Refused(_))), "{sent:?}");
    drop(agent);
    println!("ok  an agent over ssh cannot send");

    // 3. The ssh connection drops: the link says so and comes back by itself.
    // SAFETY: the only threads are ours and none reads the environment concurrently with this write.
    unsafe { std::env::set_var("CUT", &cut) };
    drop(rpc);
    let rpc = Rpc::remote(remote.clone());
    let link = { drop(link); rpc.link().unwrap() };
    assert!(block_on(rpc.hello()).unwrap().person);
    wait_real("the first pipe was cut", Duration::from_secs(10), || cut.exists());
    wait_state(&rpc, "the link to notice", |s| matches!(s, LinkState::Down { .. }));
    let down = rpc.state();
    assert!(matches!(&down, LinkState::Down { problem: Problem::Remote { host, .. }, .. } if host == "fakehost"), "{down:?}");
    wait_state(&rpc, "the link to come back", |s| *s == LinkState::Connected);
    assert!(block_on(rpc.hello()).unwrap().person, "a person again on the new connection");
    assert_eq!(block_on(rpc.accounts()).unwrap().len(), 4);
    println!("ok  reconnects after the pipe is cut ({})", match down { LinkState::Down { problem, .. } => problem.message(), _ => String::new() });

    // 4. The daemon restarts (a new person token): the link fetches the new one.
    env.kill();
    wait_state(&rpc, "the link to see the daemon gone", |s| matches!(s, LinkState::Down { .. }));
    let err = block_on(rpc.accounts()).unwrap_err();
    assert!(err.is_retryable(), "offline, not hanging: {err:?}");
    env.start();
    wait_state(&rpc, "the link to find the restarted daemon", |s| *s == LinkState::Connected);
    wait_real("hello on the new connection", Duration::from_secs(10), || rpc.last_hello().is_some());
    assert!(rpc.is_person(), "the new token was fetched: {:?}", rpc.last_hello());
    poke(&rpc, false);
    println!("ok  a restarted daemon is found again, with its new token");

    // 5. Nothing ever started (or tried to start) a daemon on this machine.
    let mut seen = Vec::new();
    while let Ok(s) = link.try_recv() {
        seen.push(s);
    }
    assert!(!seen.iter().any(|s| matches!(s, LinkState::Starting { .. })), "{seen:?}");
    assert!(!Path::new(&client.socket()).exists(), "no daemon socket on the client");
    assert!(!client.data_dir.join("maild.log").exists(), "no daemon was spawned here");
    drop(rpc);

    // 6. A host ssh cannot reach: offline with ssh's reason, never a hang.
    let mut bad = remote.clone();
    bad.host = "unreachable".into();
    let rpc = Rpc::remote(bad);
    let err = block_on(rpc.accounts()).unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
    assert!(matches!(rpc.state(), LinkState::Down { problem: Problem::Remote { ref detail, .. }, .. } if detail.contains("Could not resolve")), "{:?}", rpc.state());
    println!("ok  an unreachable host is offline with the reason");
    drop(rpc);
    env.kill();
    println!("remote_app: ok");
}
