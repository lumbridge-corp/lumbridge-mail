//! The daemon against the imap crate's scripted IMAP and SMTP servers (reused from
//! crates/lumbridge-mail-imap/tests/common): a real ImapProvider built from config.toml, with the
//! password from `password_env`. Made-up mail and a made-up password only.

#[path = "../../lumbridge-mail-imap/tests/common/mod.rs"]
mod imap_common;

use imap_common::fake_imap;
use imap_common::fake_smtp::{self, SmtpState};
use lumbridge_mail_core::paths::Paths;
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use lumbridge_maild::{Client, Daemon, Options};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

async fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_drives_a_real_imap_provider() {
    let imap = fake_imap::serve(imap_common::state(5, imap_common::BASIC));
    let smtp = fake_smtp::serve(SmtpState::default());
    // SAFETY: this test binary has one test, and nothing else reads this variable.
    unsafe { std::env::set_var("LBMAIL_IMAP_TEST_PASSWORD", "hunter2") };
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(
        paths.config_file(),
        format!(
            r#"
[daemon]
sync_interval_s = 5

[ai]
enabled = false

[[accounts]]
id = "mailbox"
persona = "work"
email = "jason@example.com"
name = "Jason Test"
[accounts.provider]
kind = "imap"
imap_host = "127.0.0.1"
imap_port = {}
imap_security = "plain"
smtp_host = "127.0.0.1"
smtp_port = {}
smtp_security = "plain"
password_env = "LBMAIL_IMAP_TEST_PASSWORD"
server_saves_sent = false
"#,
            imap.addr.port(),
            smtp.addr.port()
        ),
    )
    .unwrap();
    let d = Daemon::start(Options::new(paths.clone())).await.unwrap();
    let token = Client::read_person_token(&paths.person_token()).unwrap();
    let mut c = Client::connect(&paths.socket(), Some(token), "test").await.unwrap();

    let start = Instant::now();
    let rows = loop {
        let p: ThreadPage = c.call(method::THREADS, Query { accounts: Some(vec![AccountId::new("mailbox")]), ..Query::default() }).await.unwrap();
        if p.threads.len() == 5 {
            break p.threads;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "sync never finished: {} threads", p.threads.len());
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let st: Vec<AccountStatus> = c.call(method::STATUS, json!({})).await.unwrap();
    assert!(st[0].online && st[0].error.is_none(), "{st:?}");

    // A body over IMAP, then an action back to the server.
    let t1 = rows.iter().find(|t| t.subject == "Message 1").unwrap();
    let msgs: Vec<MessageMeta> = c.call(method::THREAD, ThreadRef { account: t1.account.clone(), thread: t1.id.clone() }).await.unwrap();
    assert_eq!(msgs[0].id.as_str(), "INBOX:100:1");
    let body: Body = c.call(method::BODY, MessageRef { account: t1.account.clone(), message: msgs[0].id.clone() }).await.unwrap();
    assert!(body.text.unwrap().contains("Body number 1."));
    let _: Value = c
        .call(method::ACT, Act { account: t1.account.clone(), action: Action::SetSeen { ids: vec![msgs[0].id.clone()], seen: true }, caller: Caller::Person })
        .await
        .unwrap();
    wait("\\Seen on the server", || imap.lock().folder("INBOX").messages[&1].has_flag("\\Seen")).await;

    // A person's send goes out over SMTP once, with a copy in Sent.
    let draft = Draft {
        local_id: "daemon-send-1".into(),
        to: vec![Address::new(Some("Ada Example"), "ada@example.com")],
        subject: "Re: Message 1".into(),
        text: "Thanks, got it.".into(),
        reply_to: Some(msgs[0].id.clone()),
        ..Draft::default()
    };
    let _: Value = c.call(method::ACT, Act { account: t1.account.clone(), action: Action::Send { draft }, caller: Caller::Person }).await.unwrap();
    wait("delivered over SMTP", || smtp.lock().delivered.len() == 1).await;
    wait("a copy in Sent", || imap.lock().folder("Sent").messages.len() == 1).await;
    assert!(smtp.lock().log.iter().all(|l| !l.contains("hunter2")));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(smtp.lock().delivered.len(), 1, "sent once");
    d.shutdown().await;
}
