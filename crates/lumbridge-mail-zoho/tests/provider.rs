//! `ZohoProvider` against the in-process fake Zoho MCP server. Made-up mail, made-up tokens and
//! `example.test` style addresses only; nothing reaches Zoho.

use lumbridge_mail_core::*;
use lumbridge_mail_zoho::fake::{
    FAKE_CLIENT_ID, FakeAccount, FakeAttachment, FakeMail, FakeState, FakeZoho, ReplyMode, now_ms,
};
use lumbridge_mail_zoho::{
    ARCHIVE, Connection, Limits, ProbeLine, Retry, ZohoConfig, ZohoProvider,
};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

const A1: &str = "1001";
const A2: &str = "2002";
const ME1: &str = "sam@one.example.test";
const ME2: &str = "sam@two.example.test";

fn accounts() -> Vec<FakeAccount> {
    vec![
        FakeAccount::standard(A1, ME1),
        FakeAccount::standard(A2, ME2),
    ]
}

fn inbox(a: &str) -> String {
    format!("{a}01")
}
fn sent(a: &str) -> String {
    format!("{a}03")
}
fn trash(a: &str) -> String {
    format!("{a}05")
}
fn projects(a: &str) -> String {
    format!("{a}07")
}

fn mail(account: &str, folder: &str, mins_ago: i64, subject: &str) -> FakeMail {
    FakeMail {
        account: account.into(),
        folder: folder.into(),
        subject: subject.into(),
        from: "pat@example.com".into(),
        sender: "Pat Example".into(),
        to: vec![if account == A1 {
            ME1.into()
        } else {
            ME2.into()
        }],
        received_ms: now_ms() - mins_ago * 60_000,
        summary: format!("About {subject}"),
        ..FakeMail::default()
    }
}

fn connection(fake: &FakeZoho, dir: &std::path::Path, expires_in: u64) -> Connection {
    Connection {
        mcp_url: fake.mcp_url(),
        token_url: fake.token_url(),
        client_id: FAKE_CLIENT_ID.into(),
        token_file: fake.write_token_file(dir, expires_in),
    }
}

fn config(conn: &Connection, id: &str, email: &str) -> ZohoConfig {
    let mut c = ZohoConfig::new(AccountId::new(id), email, conn.clone());
    c.limits = Limits {
        max_in_flight: 4,
        max_calls_per_min: 60_000,
        burst: 1000,
        backoff_base: Duration::from_millis(5),
        backoff_max: Duration::from_millis(50),
        ..Limits::default()
    };
    c.poll_interval = Duration::from_millis(20);
    c.backfill_every = Duration::ZERO;
    // Off by default (live Zoho refuses `includeArchived`); the fake supports it, so these tests cover it.
    c.sync_archive = true;
    c
}

async fn sync_all(p: &ZohoProvider, mut cursor: Option<SyncCursor>) -> (Vec<Delta>, SyncCursor) {
    let mut out = Vec::new();
    for _ in 0..200 {
        let d = p.sync(cursor.as_ref()).await.unwrap();
        cursor = d.cursor.clone();
        let more = d.more;
        out.push(d);
        if !more {
            return (out, cursor.unwrap());
        }
    }
    panic!("sync never finished");
}

fn ids(ds: &[Delta]) -> Vec<String> {
    ds.iter()
        .flat_map(|d| d.upserted.iter().map(|m| m.id.to_string()))
        .collect()
}

fn find<'a>(ds: &'a [Delta], id: &str) -> Option<&'a MessageMeta> {
    ds.iter()
        .flat_map(|d| &d.upserted)
        .rfind(|m| m.id.as_str() == id)
}

#[tokio::test]
async fn full_sync_then_cheap_incremental_rounds() {
    let mut st = FakeState::new(accounts());
    let i0 = st.add(mail(A1, &inbox(A1), 50, "Lunch plans"));
    let i1 = st.add(FakeMail {
        read: true,
        flag: "important".into(),
        thread: Some("777".into()),
        ..mail(A1, &inbox(A1), 40, "Re: Lunch")
    });
    let s0 = st.add(FakeMail {
        read: true,
        ..mail(A1, &sent(A1), 30, "Notes")
    });
    let p0 = st.add(mail(A1, &projects(A1), 20, "Project kickoff"));
    let ar = st.add(FakeMail {
        archived: true,
        read: true,
        ..mail(A1, &inbox(A1), 60, "Old receipt")
    });
    let other = st.add(mail(A2, &inbox(A2), 10, "Not this account"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let conn = connection(&fake, dir.path(), 3600);
    let mut cfg = config(&conn, "zoho-one", ME1);
    cfg.page_size = 2;
    let p = ZohoProvider::new(cfg).unwrap();

    let (pages, cursor) = sync_all(&p, None).await;
    assert!(
        pages.iter().all(|d| d.reset),
        "every page of a full sync is reset"
    );
    assert!(
        pages.len() >= 5,
        "one listing page per call: {}",
        pages.len()
    );
    let got = ids(&pages);
    for id in [&i0, &i1, &s0, &p0, &ar] {
        assert!(got.contains(id), "{id} missing from {got:?}");
    }
    assert!(!got.contains(&other), "another account's mail");
    let mb = pages[0].mailboxes.as_ref().unwrap();
    let roles: Vec<(String, MailboxRole)> = mb.iter().map(|m| (m.name.clone(), m.role)).collect();
    assert!(
        roles.contains(&("Inbox".into(), MailboxRole::Inbox))
            && roles.contains(&("Sent".into(), MailboxRole::Sent))
    );
    assert!(
        roles.contains(&("Projects".into(), MailboxRole::Custom))
            && roles.contains(&("Archive".into(), MailboxRole::Archive))
    );
    assert!(
        !roles.iter().any(|(n, _)| n == "Outbox"),
        "Outbox is not synced"
    );
    let m0 = find(&pages, &i0).unwrap();
    assert_eq!(
        (
            m0.subject.as_str(),
            m0.snippet.as_str(),
            m0.flags.seen,
            m0.flags.flagged
        ),
        ("Lunch plans", "About Lunch plans", false, false)
    );
    assert_eq!(
        m0.from,
        vec![Address::new(Some("Pat Example"), "pat@example.com")]
    );
    assert_eq!(m0.to, vec![Address::new(None, ME1)]);
    assert_eq!(m0.mailboxes, vec![MailboxId::new(inbox(A1))]);
    assert!(m0.partial);
    let m1 = find(&pages, &i1).unwrap();
    assert!(m1.flags.seen && m1.flags.flagged);
    // Zoho's rows carry no threadId: each message is its own thread.
    assert_eq!(m1.thread.as_str(), i1.as_str());
    assert_eq!(
        find(&pages, &ar).unwrap().mailboxes,
        vec![MailboxId::new(ARCHIVE)]
    );
    let last = pages.last().unwrap();
    let floor = last.window_floor.expect("a 90-day window");
    assert!((floor - (now_ms() / 1000 - 90 * 86_400)).abs() < 120);

    // Nothing changed: one listing per hot folder (Inbox, Drafts, Sent), nothing reported.
    let before = fake.lock().calls.len();
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert!(
        d.upserted.is_empty() && d.removed.is_empty() && !d.more && !d.reset,
        "{:?}",
        d.upserted
    );
    assert_eq!(fake.lock().calls.len() - before, 3);
    let cursor = d.cursor.unwrap();

    // New mail, read elsewhere, moved to a folder not polled this round, deleted.
    let (new, moved, gone) = {
        let mut s = fake.lock();
        let new = s.add(mail(A1, &inbox(A1), 1, "Brand new"));
        s.mail(&i1).unwrap().read = false;
        s.mail(&i0).unwrap().folder = projects(A1);
        s.mails.retain(|m| m.id != s0);
        (new, i0.clone(), s0.clone())
    };
    let d = p.sync(Some(&cursor)).await.unwrap();
    let up: Vec<&str> = d.upserted.iter().map(|m| m.id.as_str()).collect();
    assert!(
        up.contains(&new.as_str()) && up.contains(&i1.as_str()),
        "{up:?}"
    );
    assert!(!find(std::slice::from_ref(&d), &i1).unwrap().flags.seen);
    assert_eq!(
        find(std::slice::from_ref(&d), &moved).unwrap().mailboxes,
        vec![MailboxId::new(projects(A1))],
        "looked up"
    );
    assert_eq!(d.removed, vec![MessageId::new(gone)]);
    assert!(fake.lock().count("ZohoMail_get_mail") >= 2);
    let cursor = d.cursor.unwrap();
    let pages = lumbridge_mail_zoho::cursor_page_ids(&cursor);
    assert!(pages[&inbox(A1)].contains(&new));

    // A cursor from another provider means a full resync.
    assert!(matches!(
        p.sync(Some(&SyncCursor("rs1:{}".into()))).await,
        Err(Error::ResyncNeeded(_))
    ));
}

#[tokio::test]
async fn sse_answers_and_a_server_that_wants_a_session() {
    let mut st = FakeState::new(accounts());
    st.mode = ReplyMode::Sse;
    st.require_session = true;
    let id = st.add(mail(A1, &inbox(A1), 5, "Over SSE"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    let (pages, _) = sync_all(&p, None).await;
    assert!(ids(&pages).contains(&id));
    let body = p.fetch_body(&MessageId::new(&id)).await.unwrap();
    assert_eq!(body.html.as_deref(), Some("<div>About Over SSE</div>"));
}

#[tokio::test]
async fn an_expired_token_is_refreshed_once_and_written_back() {
    let mut st = FakeState::new(accounts());
    st.rotate_refresh = true;
    st.add(mail(A1, &inbox(A1), 5, "Hello"));
    st.add(mail(A2, &inbox(A2), 5, "Hello two"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    // Expired on disk; the fake only takes a refreshed token.
    let conn = connection(&fake, dir.path(), 0);
    {
        let mut s = fake.lock();
        s.access_token = "fake-access-not-yet".into();
    }
    let p1 = Arc::new(ZohoProvider::new(config(&conn, "zoho-one", ME1)).unwrap());
    let p2 = Arc::new(ZohoProvider::new(config(&conn, "zoho-two", ME2)).unwrap());
    assert!(
        Arc::ptr_eq(p1.connection(), p2.connection()),
        "one sign-in, one connection"
    );
    let (a, b) = tokio::join!(sync_all(&p1, None), sync_all(&p2, None));
    assert_eq!(ids(&a.0).len(), 1);
    assert_eq!(ids(&b.0).len(), 1);
    let s = fake.lock();
    assert_eq!(s.token_requests.len(), 1, "two accounts, one refresh");
    let req = &s.token_requests[0];
    assert_eq!(req["grant_type"], "refresh_token");
    assert_eq!(req["refresh_token"], "fake-refresh-0");
    assert_eq!(req["client_id"], FAKE_CLIENT_ID);
    assert_eq!(req["resource"], fake.mcp_url());
    assert!(
        s.calls.iter().any(|c| c.account.as_deref() == Some(A1))
            && s.calls.iter().any(|c| c.account.as_deref() == Some(A2))
    );
    drop(s);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&conn.token_file).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "fake-access-1");
    assert_eq!(
        saved["refresh_token"], "fake-refresh-1",
        "a rotated refresh token is kept"
    );
    assert_eq!(saved["scope"], "made-up.scope", "other keys kept");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&conn.token_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(p1.connection().refreshes(), 1);
}

#[tokio::test]
async fn a_401_refreshes_but_a_token_someone_else_refreshed_is_used_as_is() {
    let mut st = FakeState::new(accounts());
    st.add(mail(A1, &inbox(A1), 5, "Hello"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let conn = connection(&fake, dir.path(), 3600);
    let p = ZohoProvider::new(config(&conn, "zoho-one", ME1)).unwrap();
    let (_, cursor) = sync_all(&p, None).await;
    assert_eq!(fake.lock().token_requests.len(), 0);

    // The server stops taking the token: one refresh, then the call goes through.
    fake.lock().unauthorized_next = 1;
    p.sync(Some(&cursor)).await.unwrap();
    assert_eq!(fake.lock().token_requests.len(), 1);

    // Another program refreshed and wrote the file: our token is now refused, the file's is used.
    {
        let mut s = fake.lock();
        s.access_token = "fake-access-from-elsewhere".into();
    }
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&conn.token_file).unwrap()).unwrap();
    v["access_token"] = json!("fake-access-from-elsewhere");
    std::fs::write(&conn.token_file, v.to_string()).unwrap();
    p.sync(Some(&cursor)).await.unwrap();
    assert_eq!(fake.lock().token_requests.len(), 1, "no second refresh");

    // The token endpoint refuses: the user must sign in again.
    {
        let mut s = fake.lock();
        s.access_token = "fake-access-other".into();
        s.refuse_refresh = true;
    }
    assert!(matches!(p.sync(Some(&cursor)).await, Err(Error::Auth(_))));
}

#[tokio::test]
async fn a_bad_token_reported_inside_the_tool_result_also_refreshes() {
    let mut st = FakeState::new(accounts());
    st.auth_error_in_body = true;
    st.unauthorized_next = 1;
    st.add(mail(A1, &inbox(A1), 5, "Hello"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    let (pages, _) = sync_all(&p, None).await;
    assert_eq!(ids(&pages).len(), 1);
    assert_eq!(fake.lock().token_requests.len(), 1);
}

#[tokio::test]
async fn rate_limits_are_waited_out_and_failures_open_the_breaker() {
    let mut st = FakeState::new(accounts());
    st.add(mail(A1, &inbox(A1), 5, "Hello"));
    st.rate_limit_next = 1;
    st.retry_after = "1".into();
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let conn = connection(&fake, dir.path(), 3600);
    let mut cfg = config(&conn, "zoho-one", ME1);
    cfg.limits.backoff_max = Duration::from_secs(5);
    cfg.limits.retries = 1;
    cfg.limits.breaker_failures = 3;
    let p = ZohoProvider::new(cfg).unwrap();
    let t = Instant::now();
    let (pages, cursor) = sync_all(&p, None).await;
    assert!(
        t.elapsed() >= Duration::from_millis(900),
        "waited for Retry-After: {:?}",
        t.elapsed()
    );
    assert_eq!(ids(&pages).len(), 1);

    fake.lock().fail_next = 100;
    assert!(matches!(
        p.sync(Some(&cursor)).await,
        Err(Error::Offline(_))
    ));
    assert!(matches!(
        p.sync(Some(&cursor)).await,
        Err(Error::Offline(_))
    ));
    let load = p.load().unwrap();
    assert!(load.breaker_open_until.is_some(), "{load:?}");
    let n = fake.lock().calls.len();
    assert!(matches!(
        p.sync(Some(&cursor)).await,
        Err(Error::Offline(_))
    ));
    assert_eq!(fake.lock().calls.len(), n, "the open breaker sends nothing");
    assert!(load.scope.starts_with("zoho:") && load.max_in_flight == 4);
}

#[tokio::test]
async fn null_lists_and_missing_fields_read_as_empty() {
    let mut st = FakeState::new(accounts());
    st.null_lists = true;
    let id = st.add(FakeMail {
        to: Vec::new(),
        summary: String::new(),
        ..mail(A1, &inbox(A1), 5, "Bare")
    });
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    let (pages, cursor) = sync_all(&p, None).await;
    let m = find(&pages, &id).unwrap();
    assert!(m.to.is_empty() && m.snippet.is_empty());
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert!(d.upserted.is_empty());
    let body = p.fetch_body(&MessageId::new(&id)).await.unwrap();
    assert!(body.attachments.is_empty());
}

#[tokio::test]
async fn admin_tools_are_refused_before_anything_is_sent() {
    let fake = FakeZoho::start(FakeState::new(accounts())).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    for tool in [
        "ZohoMail_deleteUser",
        "ZohoMail_resetUserPassword",
        "ZohoMail_deleteDomain",
        "ZohoMail_empty_folder",
        "tools/list",
    ] {
        let e = p
            .connection()
            .call(
                tool,
                json!({ "path_variables": { "zuid": "1" } }),
                Retry::Safe,
            )
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{tool}: {e}");
    }
    let s = fake.lock();
    assert!(
        s.calls.is_empty() && s.token_requests.is_empty(),
        "nothing reached the server"
    );
}

#[tokio::test]
async fn actions_reach_zoho_with_the_right_tools() {
    let mut st = FakeState::new(accounts());
    let a = st.add(mail(A1, &inbox(A1), 5, "One"));
    let b = st.add(mail(A1, &inbox(A1), 4, "Two"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    let ids2 = vec![MessageId::new(&a), MessageId::new(&b)];

    p.apply(&Action::SetSeen {
        ids: ids2.clone(),
        seen: true,
    })
    .await
    .unwrap();
    assert!(
        fake.lock()
            .mails
            .iter()
            .filter(|m| m.account == A1)
            .all(|m| m.read)
    );
    p.apply(&Action::SetFlagged {
        ids: vec![MessageId::new(&a)],
        flagged: true,
    })
    .await
    .unwrap();
    assert_eq!(fake.lock().mail(&a).unwrap().flag, "important");
    assert!(matches!(
        p.apply(&Action::SetFlagged {
            ids: vec![MessageId::new(&a)],
            flagged: false
        })
        .await,
        Err(Error::Unsupported(_))
    ));
    p.apply(&Action::Move {
        ids: vec![MessageId::new(&a)],
        from: Some(MailboxId::new(inbox(A1))),
        to: MailboxId::new(projects(A1)),
    })
    .await
    .unwrap();
    assert_eq!(fake.lock().mail(&a).unwrap().folder, projects(A1));
    p.apply(&Action::Archive {
        ids: vec![MessageId::new(&a)],
    })
    .await
    .unwrap();
    assert!(fake.lock().mail(&a).unwrap().archived);
    p.apply(&Action::Move {
        ids: vec![MessageId::new(&a)],
        from: Some(MailboxId::new(ARCHIVE)),
        to: MailboxId::new(inbox(A1)),
    })
    .await
    .unwrap();
    {
        let mut s = fake.lock();
        let m = s.mail(&a).unwrap();
        assert!(!m.archived && m.folder == inbox(A1));
    }
    p.apply(&Action::Trash {
        ids: vec![MessageId::new(&b)],
    })
    .await
    .unwrap();
    assert_eq!(fake.lock().mail(&b).unwrap().folder, trash(A1));
    // Drafts are local: nothing is sent.
    let n = fake.lock().calls.len();
    p.apply(&Action::SaveDraft {
        draft: Draft {
            local_id: "d1".into(),
            subject: "x".into(),
            ..Draft::default()
        },
    })
    .await
    .unwrap();
    assert_eq!(fake.lock().calls.len(), n);
    assert!(matches!(
        p.apply(&Action::Trash {
            ids: vec![MessageId::new("../x")]
        })
        .await,
        Err(Error::NotFound(_))
    ));
    let caps = p.capabilities();
    assert_eq!(
        (caps.drafts, caps.seen, caps.moves),
        (Support::Local, Support::Server, Support::Server)
    );
    assert!(caps.send && caps.raw);
}

#[tokio::test]
async fn send_reply_and_no_duplicate_after_a_lost_answer() {
    let mut st = FakeState::new(accounts());
    let parent = st.add(mail(A1, &inbox(A1), 30, "Question"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&connection(&fake, dir.path(), 3600), "zoho-one", ME1);
    cfg.name = Some("Sam".into());
    let p = ZohoProvider::new(cfg).unwrap();
    let draft = Draft {
        local_id: "local-1".into(),
        to: vec![Address::new(Some("Kim"), "kim@example.org")],
        cc: vec![Address::new(None, "lee@example.org")],
        subject: "Hello there".into(),
        text: "Hi Kim, see you soon.".into(),
        ..Draft::default()
    };
    let out = p
        .apply(&Action::Send {
            draft: draft.clone(),
        })
        .await
        .unwrap();
    assert!(out.server_id.is_some());
    {
        let s = fake.lock();
        assert_eq!(s.sends.len(), 1);
        let b = &s.sends[0]["body"];
        assert_eq!(s.sends[0]["tool"], "ZohoMail_send_mail");
        assert_eq!(
            (
                b["fromAddress"].as_str(),
                b["toAddress"].as_str(),
                b["ccAddress"].as_str()
            ),
            (Some(ME1), Some("kim@example.org"), Some("lee@example.org"))
        );
        assert_eq!(
            (b["contentType"].as_str(), b["content"].as_str()),
            (Some("plaintext"), Some("Hi Kim, see you soon."))
        );
        assert!(b.get("bccAddress").is_none());
    }
    // The same draft again (a retry after a lost answer, or after a restart with a new local id): not sent twice.
    p.apply(&Action::Send {
        draft: draft.clone(),
    })
    .await
    .unwrap();
    let fresh = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    fresh
        .apply(&Action::Send {
            draft: Draft {
                local_id: "local-2".into(),
                ..draft.clone()
            },
        })
        .await
        .unwrap();
    assert_eq!(fake.lock().sends.len(), 1);

    // A reply goes through reply_to_mail on the parent.
    let reply = Draft {
        local_id: "local-3".into(),
        to: vec![Address::new(None, "pat@example.com")],
        subject: "Re: Question".into(),
        text: "Answer.".into(),
        html: Some("<p>Answer.</p>".into()),
        reply_to: Some(MessageId::new(&parent)),
        ..Draft::default()
    };
    p.apply(&Action::Send { draft: reply }).await.unwrap();
    {
        let s = fake.lock();
        assert_eq!(s.sends.len(), 2);
        assert_eq!(s.sends[1]["tool"], "ZohoMail_reply_to_mail");
        assert_eq!(s.sends[1]["parent"], parent.as_str());
        assert_eq!(s.sends[1]["body"]["contentType"], "html");
        assert_eq!(s.sends[1]["body"]["includeOriginalContent"], false);
    }
    // A send that fails on transport is not retried by the connection (the queue retries, after the check).
    {
        let mut s = fake.lock();
        s.fail_next = 1;
        s.fail_only = Some("ZohoMail_send_mail".into());
    }
    let d4 = Draft {
        local_id: "local-4".into(),
        subject: "Another".into(),
        ..draft.clone()
    };
    assert!(matches!(
        p.apply(&Action::Send { draft: d4 }).await,
        Err(Error::Offline(_))
    ));
    assert_eq!(
        fake.lock().count("ZohoMail_send_mail"),
        2,
        "one try, one failure"
    );
    let with_file = Draft {
        local_id: "local-5".into(),
        attachments: vec![DraftAttachment {
            filename: "a.txt".into(),
            mime: "text/plain".into(),
            path: "/nonexistent".into(),
        }],
        ..draft
    };
    assert!(matches!(
        p.apply(&Action::Send { draft: with_file }).await,
        Err(Error::Unsupported(_))
    ));
}

#[tokio::test]
async fn bodies_headers_raw_and_attachments() {
    let mut st = FakeState::new(accounts());
    let id = st.add(FakeMail {
        headers: vec![
            "List-Id: <news.example.com>".into(),
            "List-Unsubscribe: <mailto:unsub@example.com>".into(),
            "X-Other: 1".into(),
        ],
        html: "<p>Weekly news</p><img src=\"cid:logo1\">".into(),
        attachments: vec![
            FakeAttachment {
                name: "report.pdf".into(),
                bytes: b"%PDF-made-up".to_vec(),
                ..FakeAttachment::default()
            },
            FakeAttachment {
                name: "logo.png".into(),
                bytes: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
                cid: Some("logo1".into()),
                ..FakeAttachment::default()
            },
        ],
        ..mail(A1, &inbox(A1), 5, "Newsletter")
    });
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    let mid = MessageId::new(&id);
    let body = p.fetch_body(&mid).await.unwrap();
    assert!(body.html.as_deref().unwrap().contains("Weekly news"));
    assert_eq!(body.header("List-Id"), Some("<news.example.com>"));
    assert_eq!(
        body.header("List-Unsubscribe"),
        Some("<mailto:unsub@example.com>")
    );
    assert!(body.header("X-Other").is_none());
    assert_eq!(body.attachments.len(), 2);
    let pdf = body
        .attachments
        .iter()
        .find(|a| a.filename == "report.pdf")
        .unwrap();
    assert_eq!((pdf.mime.as_str(), pdf.size), ("application/pdf", 12));
    let logo = body
        .attachments
        .iter()
        .find(|a| a.content_id.is_some())
        .unwrap();
    assert_eq!(
        p.fetch_attachment(&mid, &pdf.id).await.unwrap(),
        b"%PDF-made-up"
    );
    assert_eq!(
        p.fetch_attachment(&mid, &logo.id).await.unwrap(),
        vec![0x89, b'P', b'N', b'G', 1, 2, 3]
    );
    assert!(matches!(
        p.fetch_attachment(&mid, &AttachmentId::new("999")).await,
        Err(Error::NotFound(_))
    ));
    let raw = p.fetch_raw(&mid).await.unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("Subject: Newsletter"));
    assert!(matches!(
        p.fetch_body(&MessageId::new("123456")).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn the_window_the_count_limit_and_backfill() {
    let mut st = FakeState::new(accounts());
    let mut inbox_ids = Vec::new();
    for i in 0..6 {
        inbox_ids.push(st.add(mail(A1, &inbox(A1), 60 * (i + 1), &format!("Mail {i}"))));
    }
    let ancient = st.add(mail(A1, &sent(A1), 60 * 24 * 200, "From long ago"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&connection(&fake, dir.path(), 3600), "zoho-one", ME1);
    cfg.window_max = 4;
    cfg.page_size = 3;
    let p = ZohoProvider::new(cfg).unwrap();
    let (pages, cursor) = sync_all(&p, None).await;
    let got = ids(&pages);
    assert_eq!(
        got.iter().filter(|i| inbox_ids.contains(i)).count(),
        4,
        "{got:?}"
    );
    assert!(!got.contains(&ancient), "outside the 90 days");
    let floor = pages.last().unwrap().window_floor.unwrap();
    let oldest_listed = find(&pages, &inbox_ids[3]).unwrap().date;
    assert_eq!(
        floor,
        oldest_listed + 1,
        "raised to what the count limit covered"
    );

    p.request_backfill(None).await.unwrap();
    // One backfill page per round at most.
    let mut pages = Vec::new();
    let mut cursor = Some(cursor);
    for _ in 0..10 {
        let (more, c) = sync_all(&p, cursor).await;
        pages.extend(more);
        cursor = Some(c);
    }
    let filled: Vec<String> = pages
        .iter()
        .filter(|d| d.backfill)
        .flat_map(|d| d.upserted.iter().map(|m| m.id.to_string()))
        .collect();
    assert!(
        filled.contains(&inbox_ids[4])
            && filled.contains(&inbox_ids[5])
            && filled.contains(&ancient),
        "{filled:?}"
    );
    assert!(
        pages
            .iter()
            .filter(|d| d.backfill)
            .all(|d| d.upserted.iter().all(|m| m.date < floor))
    );
}

#[tokio::test]
async fn a_burst_of_new_mail_is_followed_page_by_page() {
    let mut st = FakeState::new(accounts());
    st.add(mail(A1, &inbox(A1), 100, "Old one"));
    st.add(mail(A1, &inbox(A1), 90, "Old two"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&connection(&fake, dir.path(), 3600), "zoho-one", ME1);
    cfg.poll_size = 2;
    let p = ZohoProvider::new(cfg).unwrap();
    let (_, cursor) = sync_all(&p, None).await;
    let new: Vec<String> = {
        let mut s = fake.lock();
        (0..5)
            .map(|i| s.add(mail(A1, &inbox(A1), 10 - i, &format!("Burst {i}"))))
            .collect()
    };
    let (pages, _) = sync_all(&p, Some(cursor)).await;
    let got = ids(&pages);
    for id in &new {
        assert!(got.contains(id), "{id} missing: {got:?}");
    }
    assert!(pages.len() >= 3, "followed in extra calls: {}", pages.len());
    assert!(
        pages.iter().all(|d| d.removed.is_empty()),
        "pushed off the page is not gone"
    );
}

#[tokio::test]
async fn pages_follow_zohos_page_token_and_no_fields_are_asked() {
    let mut st = FakeState::new(accounts());
    for i in 0..5 {
        st.add(mail(A1, &inbox(A1), 10 + i, &format!("Mail {i}")));
    }
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&connection(&fake, dir.path(), 3600), "zoho-one", ME1);
    cfg.page_size = 2;
    let p = ZohoProvider::new(cfg).unwrap();
    let (pages, _) = sync_all(&p, None).await;
    assert_eq!(ids(&pages).len(), 5);
    let s = fake.lock();
    let lists: Vec<&serde_json::Value> = s
        .calls
        .iter()
        .filter(|c| c.tool == "ZohoMail_list_mails")
        .map(|c| &c.args)
        .collect();
    assert!(
        lists
            .iter()
            .any(|a| a.pointer("/query_params/pageToken").is_some()),
        "the next pages use the token"
    );
    assert!(
        lists
            .iter()
            .all(|a| a.pointer("/query_params/fields").is_none()),
        "no fields: the default row has what sync needs"
    );
    assert!(
        s.calls
            .iter()
            .all(|c| c.args.pointer("/query_params/fields").is_none())
    );
}

#[tokio::test]
async fn a_single_wrapped_answer_is_read_too() {
    let mut st = FakeState::new(accounts());
    st.single_wrap = true;
    let id = st.add(mail(A1, &inbox(A1), 5, "Hello"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    let (pages, _) = sync_all(&p, None).await;
    assert_eq!(ids(&pages), vec![id.clone()]);
    assert!(p.fetch_raw(&MessageId::new(&id)).await.is_ok());
}

#[tokio::test]
async fn plain_text_answers_are_errors_and_undeclared_keys_never_leave() {
    let mut st = FakeState::new(accounts());
    st.add(mail(A1, &inbox(A1), 5, "Hello"));
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    let (_, cursor) = sync_all(&p, None).await;
    fake.lock().plain_error_next = Some("Invalid keys found in path variable. Kindly check allowed keys in tool schema and try again.".into());
    match p.sync(Some(&cursor)).await {
        Err(Error::Other(m)) => assert!(m.contains("Invalid keys found in path variable"), "{m}"),
        other => panic!("{other:?}"),
    }
    // A key the tool does not declare is refused here, before anything is sent.
    let n = fake.lock().calls.len();
    let e = p
        .connection()
        .call(
            "ZohoMail_get_mail",
            json!({ "path_variables": { "accountId": A1, "messageId": "1", "folderId": "2" } }),
            Retry::Safe,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::Invalid(_)) && e.to_string().contains("path_variables.folderId"),
        "{e}"
    );
    assert_eq!(fake.lock().calls.len(), n);
    // A folder id Zoho does not know is an empty list, not an error.
    let v = p
        .connection()
        .call(
            "ZohoMail_list_mails",
            json!({ "path_variables": { "accountId": A1 }, "query_params": { "folderId": "999" } }),
            Retry::Safe,
        )
        .await
        .unwrap();
    assert!(
        lumbridge_mail_zoho::parse::page(&v)
            .unwrap()
            .rows
            .is_empty()
    );
}

#[tokio::test]
async fn the_probe_reports_shapes_and_never_mail() {
    let mut st = FakeState::new(vec![FakeAccount {
        aliases: vec!["sam@two.example.test".into()],
        ..FakeAccount::standard(A1, ME1)
    }]);
    st.add(FakeMail {
        headers: vec!["List-Id: <news.example.com>".into()],
        attachments: vec![FakeAttachment {
            name: "secret-report.pdf".into(),
            bytes: b"%PDF-made-up".to_vec(),
            ..FakeAttachment::default()
        }],
        summary: "Confidential summary text".into(),
        ..mail(A1, &inbox(A1), 5, "Very private subject")
    });
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let p = ZohoProvider::new(config(
        &connection(&fake, dir.path(), 3600),
        "zoho-one",
        ME1,
    ))
    .unwrap();
    let lines: Vec<ProbeLine> = p.probe().await;
    let tools: Vec<&str> = lines.iter().map(|l| l.tool).collect();
    assert_eq!(
        tools,
        vec![
            "ZohoMail_list_accounts",
            "ZohoMail_list_folders",
            "ZohoMail_list_mails",
            "ZohoMail_get_mail",
            "ZohoMail_get_mail_html",
            "ZohoMail_get_mail_headers",
            "ZohoMail_list_mail_attachments",
            "ZohoMail_get_mail_original"
        ]
    );
    for l in &lines {
        assert!(l.ok && matches!(l.parser, Some(Ok(_))), "{l}");
        assert_eq!(l.status, Some(200), "{l}");
    }
    assert!(lines[1].folder_types.contains(&"Inbox".to_string()));
    let printed: String = lines.iter().map(|l| l.to_string()).collect();
    let id = fake.lock().mails[0].id.clone();
    for secret in [
        "Very private",
        "Confidential",
        "pat@example.com",
        "Pat Example",
        "sam@one",
        "sam@two",
        "secret-report",
        "news.example",
        "PDF-made-up",
        id.as_str(),
        A1,
    ] {
        assert!(!printed.contains(secret), "{secret} printed:\n{printed}");
    }
    assert!(
        printed.contains("messages: [1 x {") && printed.contains("folderType: str"),
        "{printed}"
    );
    // Only read tools were called.
    {
        let s = fake.lock();
        assert!(
            s.calls.iter().all(
                |c| c.tool.starts_with("ZohoMail_list_") || c.tool.starts_with("ZohoMail_get_")
            ),
            "{:?}",
            s.calls.iter().map(|c| &c.tool).collect::<Vec<_>>()
        );
    }
    // A failing call shows the server's words.
    fake.lock().plain_error_next = Some("Invalid keys found in path variable. Kindly check allowed keys in tool schema and try again.".into());
    let lines = p.probe().await;
    assert!(
        !lines[0].ok
            && lines[0]
                .error
                .as_deref()
                .unwrap()
                .contains("Invalid keys found"),
        "{}",
        lines[0]
    );
}

#[tokio::test]
async fn the_archive_is_off_by_default_and_a_cursor_that_names_it_still_syncs() {
    let mut st = FakeState::new(accounts());
    let i0 = st.add(mail(A1, &inbox(A1), 50, "Lunch plans"));
    let ar = st.add(FakeMail {
        archived: true,
        read: true,
        ..mail(A1, &inbox(A1), 60, "Old receipt")
    });
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let conn = connection(&fake, dir.path(), 3600);
    // A cursor saved while the archive was on...
    let on = ZohoProvider::new(config(&conn, "zoho-one", ME1)).unwrap();
    let first = on.sync(None).await.unwrap();
    assert!(first.more, "the full sync has more pages");
    // ...is finished by a provider with the default (off): no archive page, no error.
    let mut cfg = config(&conn, "zoho-one", ME1);
    cfg.sync_archive = ZohoConfig::new(AccountId::new("x"), ME1, conn.clone()).sync_archive;
    assert!(!cfg.sync_archive, "off by default");
    let off = ZohoProvider::new(cfg).unwrap();
    let (pages, _) = sync_all(&off, first.cursor.clone()).await;
    let got: Vec<_> = ids(&[vec![first], pages].concat());
    assert!(got.contains(&i0));
    assert!(!got.contains(&ar), "archived mail is not listed while the archive is off");
}

/// Live Zoho refuses `includeArchived` with a 400 naming it. The refusal says why (errorCode, location,
/// locationType), the archive view is off for the session (asked once, never again), and the rest of the
/// sync goes on. The same for `fields`: listings go on without it.
#[tokio::test]
async fn a_parameter_zoho_refuses_is_dropped_for_the_session_and_the_sync_goes_on() {
    let mut st = FakeState::new(accounts());
    let i0 = st.add(mail(A1, &inbox(A1), 50, "Lunch plans"));
    let p0 = st.add(mail(A1, &projects(A1), 20, "Project kickoff"));
    st.add(FakeMail { archived: true, ..mail(A1, &inbox(A1), 60, "Old receipt") });
    st.refuse_params = vec!["includeArchived".into(), "fields".into()];
    let fake = FakeZoho::start(st).await;
    let dir = tempfile::tempdir().unwrap();
    let conn = connection(&fake, dir.path(), 3600);
    let mut cfg = config(&conn, "zoho-one", ME1);
    cfg.cold_every = 1;
    cfg.list_fields = "messageId,threadId,folderId,subject".into();
    let p = ZohoProvider::new(cfg).unwrap();

    let (pages, mut cursor) = sync_all(&p, None).await;
    let got = ids(&pages);
    assert!(got.contains(&i0) && got.contains(&p0), "the folders synced: {got:?}");
    let asked = |k: &str| fake.lock().calls.iter().filter(|c| c.tool == "ZohoMail_list_mails" && c.args.pointer(&format!("/query_params/{k}")).is_some()).count();
    assert_eq!((asked("includeArchived"), asked("fields")), (1, 1), "each refused shape was sent once");
    // Rounds that look at every folder (and the archive) do not ask again.
    for _ in 0..3 {
        cursor = p.sync(Some(&cursor)).await.unwrap().cursor.unwrap();
    }
    assert_eq!((asked("includeArchived"), asked("fields")), (1, 1));
    assert!(p.refused("ZohoMail_list_mails", Some(&json!({"includeArchived": true, "limit": 1, "sortBy": "time", "sortOrder": false, "start": 1}))));
}
