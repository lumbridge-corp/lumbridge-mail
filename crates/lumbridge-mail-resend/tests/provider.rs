//! `ResendProvider` against the in-process fake Resend server. Made-up mail and a made-up key only.

use lumbridge_mail_core::*;
use lumbridge_mail_resend::fake::{FAKE_KEY, FakeAttachment, FakeReceived, FakeResend, FakeSent, FakeState};
use lumbridge_mail_resend::{ApiKey, ResendConfig, ResendProvider};
use std::time::{Duration, Instant};

const T0: i64 = 1_794_211_200_000; // 2026-11-09 08:00 UTC

fn config(fake: &FakeResend) -> ResendConfig {
    let mut c = ResendConfig::new(AccountId::new("shop"), "hello@shop.example", ApiKey::new(FAKE_KEY));
    c.name = Some("Shop".into());
    c.base_url = fake.base_url();
    c.requests_per_second = 200.0;
    c.poll_interval = Duration::from_millis(50);
    c
}

fn sent(n: i64, to: &str, subject: &str, event: &str) -> FakeSent {
    FakeSent {
        from: "Shop <hello@shop.example>".into(),
        to: vec![to.into()],
        subject: subject.into(),
        text: Some(format!("Text of {subject}")),
        html: Some(format!("<p>{subject}</p>")),
        created_ms: T0 + n * 60_000,
        last_event: event.into(),
        ..FakeSent::default()
    }
}

fn received(n: i64, from: &str, subject: &str) -> FakeReceived {
    FakeReceived {
        from: from.into(),
        to: vec!["hello@shop.example".into()],
        subject: subject.into(),
        text: Some(format!("Hello, this is {subject}.")),
        html: Some(format!("<p>Hello, this is {subject}.</p>")),
        created_ms: T0 + n * 60_000,
        ..FakeReceived::default()
    }
}

async fn sync_all(p: &ResendProvider, mut cursor: Option<SyncCursor>) -> (Vec<Delta>, SyncCursor) {
    let mut out = Vec::new();
    for _ in 0..100 {
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
    ds.iter().flat_map(|d| d.upserted.iter().map(|m| m.id.to_string())).collect()
}

#[tokio::test]
async fn full_sync_pages_then_incremental_is_cheap() {
    let mut st = FakeState::new();
    let mut sent_ids = Vec::new();
    for i in 0..5 {
        sent_ids.push(st.add_sent(sent(i, &format!("c{i}@example.com"), &format!("Order {i}"), "delivered")));
    }
    // Another website of the same team: not this account's mail.
    st.add_sent(FakeSent { from: "noreply@blog.example".into(), ..sent(9, "x@example.com", "Blog digest", "delivered") });
    let mut recv_ids = Vec::new();
    for i in 0..3 {
        recv_ids.push(st.add_received(received(i, &format!("Customer {i} <c{i}@example.com>"), &format!("Question {i}"))));
    }
    st.add_received(FakeReceived { to: vec!["team@blog.example".into()], ..received(8, "z@example.com", "Not ours") });
    let fake = FakeResend::start(st).await;
    let mut cfg = config(&fake);
    cfg.page_size = 2;
    let p = ResendProvider::new(cfg).unwrap();

    let (pages, cursor) = sync_all(&p, None).await;
    assert!(pages.len() >= 3, "paged: {}", pages.len());
    assert!(pages.iter().all(|d| d.reset), "every page of a full sync is reset");
    let got = ids(&pages);
    assert_eq!(got.len(), 8, "{got:?}");
    for id in &sent_ids {
        assert!(got.contains(&format!("s:{id}")));
    }
    for id in &recv_ids {
        assert!(got.contains(&format!("r:{id}")));
    }
    let mb = pages[0].mailboxes.as_ref().unwrap();
    let roles: Vec<_> = mb.iter().map(|m| (m.id.as_str(), m.role)).collect();
    assert!(roles.contains(&("inbox", MailboxRole::Inbox)) && roles.contains(&("sent", MailboxRole::Sent)));
    assert!(roles.contains(&("scheduled", MailboxRole::Custom)) && roles.contains(&("bounced", MailboxRole::Custom)));
    let first = pages.iter().flat_map(|d| &d.upserted).find(|m| m.id.as_str() == format!("r:{}", recv_ids[0])).unwrap();
    assert_eq!(first.snippet, "Hello, this is Question 0.", "new received mail is read for its snippet");
    assert_eq!(first.from[0].email, "c0@example.com");
    assert_eq!(first.mailboxes, vec![MailboxId::new("inbox")]);
    assert!(!first.flags.seen);
    let s0 = pages.iter().flat_map(|d| &d.upserted).find(|m| m.id.as_str() == format!("s:{}", sent_ids[0])).unwrap();
    assert_eq!((s0.snippet.as_str(), s0.flags.seen), ("Delivered", true));
    assert_eq!(s0.date, (T0 / 1000), "created_at in Resend's sent format");

    // Nothing new: one request per list, nothing reported.
    let before = fake.lock().api_calls();
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert!(d.upserted.is_empty() && d.removed.is_empty() && !d.more && !d.reset, "{:?}", d.upserted);
    assert_eq!(fake.lock().api_calls() - before, 2);

    // New mail on both sides: only that is reported.
    let (new_s, new_r) = {
        let mut s = fake.lock();
        (s.add_sent(sent(20, "new@example.com", "Order 20", "sent")), s.add_received(received(21, "new@example.com", "Hi again")))
    };
    let (pages, cursor) = sync_all(&p, d.cursor.clone()).await;
    assert_eq!(ids(&pages), vec![format!("s:{new_s}"), format!("r:{new_r}")]);
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert!(d.upserted.is_empty());
    // A cursor from another provider means a full resync.
    assert!(matches!(p.sync(Some(&SyncCursor("imap1:{}".into()))).await, Err(Error::ResyncNeeded(_))));
}

#[tokio::test]
async fn a_429_is_waited_out_and_too_many_go_offline() {
    let mut st = FakeState::new();
    st.add_sent(sent(0, "a@example.com", "One", "delivered"));
    st.rate_limit_next = 1;
    st.retry_after = "1".into();
    let fake = FakeResend::start(st).await;
    let p = ResendProvider::new(config(&fake)).unwrap();
    let t = Instant::now();
    let (pages, _) = sync_all(&p, None).await;
    assert!(t.elapsed() >= Duration::from_millis(900), "waited for Retry-After: {:?}", t.elapsed());
    assert_eq!(ids(&pages).len(), 1);

    fake.lock().rate_limit_next = 100;
    fake.lock().retry_after = "0".into();
    let mut cfg = config(&fake);
    cfg.max_rate_retries = 2;
    let p = ResendProvider::new(cfg).unwrap();
    let before = fake.lock().api_calls();
    assert!(matches!(p.sync(None).await, Err(Error::Offline(_))));
    assert_eq!(fake.lock().api_calls() - before, 3, "one try and two retries");
}

fn draft(local_id: &str) -> Draft {
    Draft {
        local_id: local_id.into(),
        to: vec![Address::new(Some("Ada Customer"), "ada@example.com")],
        subject: "Your order".into(),
        text: "It ships tomorrow.".into(),
        html: Some("<p>It ships tomorrow.</p>".into()),
        ..Draft::default()
    }
}

#[tokio::test]
async fn a_send_retried_after_a_lost_answer_goes_out_once() {
    let mut st = FakeState::new();
    st.drop_next_send = true;
    let fake = FakeResend::start(st).await;
    let mut cfg = config(&fake);
    cfg.reply_to = Some("support@shop.example".into());
    let p = ResendProvider::new(cfg).unwrap();
    let dir = std::env::temp_dir().join(format!("lbmail-resend-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("invoice.txt");
    std::fs::write(&file, b"made-up invoice").unwrap();
    let mut d = draft("draft-1");
    d.attachments = vec![DraftAttachment { path: file.to_string_lossy().into(), filename: "invoice.txt".into(), mime: "text/plain".into() }];
    let send = Action::Send { draft: d.clone() };

    let first = p.apply(&send).await;
    assert!(matches!(first, Err(Error::Offline(_))), "the answer was lost: {first:?}");
    let out = p.apply(&send).await.unwrap();
    {
        let s = fake.lock();
        assert_eq!(s.sent.len(), 1, "sent once");
        assert_eq!(out.server_id, Some(format!("s:{}", s.sent[0].id)));
        let keys: Vec<_> = s.requests.iter().filter(|r| r.method == "POST").map(|r| r.idempotency_key.clone().unwrap()).collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1], "the same key both times");
        let e = &s.sent[0];
        assert_eq!(e.from, "\"Shop\" <hello@shop.example>");
        assert_eq!(e.to, vec!["\"Ada Customer\" <ada@example.com>"]);
        assert_eq!(e.reply_to, vec!["support@shop.example"]);
        assert_eq!(e.attachments[0].bytes, b"made-up invoice");
    }

    // The same draft id with other content: Resend refuses, and so do we (never retried).
    let mut changed = d.clone();
    changed.text = "Something else".into();
    assert!(matches!(p.apply(&Action::Send { draft: changed }).await, Err(Error::Refused(_))));
    assert_eq!(fake.lock().sent.len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn replies_carry_threading_headers_and_join_the_thread() {
    let mut st = FakeState::new();
    let rid = st.add_received(FakeReceived {
        message_id: "<q1@example.com>".into(),
        headers: vec![("References".into(), "<root@example.com>".into())],
        ..received(0, "Ada Customer <ada@example.com>", "Shelf sizes")
    });
    let fake = FakeResend::start(st).await;
    let p = ResendProvider::new(config(&fake)).unwrap();
    let (pages, cursor) = sync_all(&p, None).await;
    let q = pages[0].upserted.iter().find(|m| m.id.as_str() == format!("r:{rid}")).unwrap().clone();
    assert_eq!(q.references, vec!["root@example.com"]);

    let mut d = draft("reply-1");
    d.subject = "Re: Shelf sizes".into();
    d.reply_to = Some(q.id.clone());
    p.apply(&Action::Send { draft: d }).await.unwrap();
    let h = fake.lock().sent[0].headers.clone();
    assert_eq!(h["In-Reply-To"], "<q1@example.com>");
    assert_eq!(h["References"], "<root@example.com> <q1@example.com>");
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert_eq!(d.upserted.len(), 1);
    assert_eq!(d.upserted[0].thread, q.thread, "the reply and the question are one thread");
}

#[tokio::test]
async fn sending_checks_the_from_domain() {
    let mut st = FakeState::new();
    st.domains[0].status = "pending".into();
    let fake = FakeResend::start(st).await;
    let p = ResendProvider::new(config(&fake)).unwrap();
    assert!(matches!(p.apply(&Action::Send { draft: draft("d") }).await, Err(Error::Refused(_))));
    let mut cfg = config(&fake);
    cfg.email = "hello@elsewhere.example".into();
    cfg.addresses = vec![lumbridge_mail_resend::AddressPattern::parse("@shop.example").unwrap()];
    let p = ResendProvider::new(cfg).unwrap();
    assert!(matches!(p.apply(&Action::Send { draft: draft("d") }).await, Err(Error::Refused(_))), "not one of the account's addresses");
    assert!(fake.lock().sent.is_empty());
    assert!(fake.lock().requests.iter().all(|r| r.method == "GET"));
}

#[tokio::test]
async fn trash_cancels_a_scheduled_email() {
    let mut st = FakeState::new();
    let later = FakeSent { scheduled_ms: Some(T0 + 400 * 86_400_000), ..sent(0, "ada@example.com", "Reminder", "scheduled") };
    let sid = st.add_sent(later);
    let done = st.add_sent(sent(1, "bo@example.com", "Receipt", "delivered"));
    let fake = FakeResend::start(st).await;
    let p = ResendProvider::new(config(&fake)).unwrap();
    let (pages, cursor) = sync_all(&p, None).await;
    let m = pages.iter().flat_map(|d| &d.upserted).find(|m| m.id.as_str() == format!("s:{sid}")).unwrap();
    assert_eq!(m.mailboxes, vec![MailboxId::new("scheduled")]);
    assert!(m.snippet.starts_with("Scheduled for 2027-"), "{}", m.snippet);

    p.apply(&Action::Trash { ids: vec![m.id.clone(), MessageId::new(format!("s:{done}"))] }).await.unwrap();
    {
        let s = fake.lock();
        assert_eq!(s.sent.iter().find(|e| e.id == sid).unwrap().last_event, "canceled");
        assert_eq!(s.sent.iter().find(|e| e.id == done).unwrap().last_event, "delivered", "only scheduled mail is canceled");
        assert_eq!(s.requests.iter().filter(|r| r.path.ends_with("/cancel")).count(), 1);
    }
    let d = p.sync(Some(&cursor)).await.unwrap();
    let m = d.upserted.iter().find(|m| m.id.as_str() == format!("s:{sid}")).expect("the canceled email is reported");
    assert_eq!(m.mailboxes, vec![MailboxId::new("trash")]);
    assert_eq!(m.snippet, "Canceled");
    // Trashing it again is fine (idempotent) and cancels nothing more.
    p.apply(&Action::Trash { ids: vec![m.id.clone()] }).await.unwrap();
    assert_eq!(fake.lock().requests.iter().filter(|r| r.path.ends_with("/cancel")).count(), 1);
}

#[tokio::test]
async fn due_scheduled_mail_is_seen_going_out() {
    let mut st = FakeState::new();
    let sid = st.add_sent(FakeSent { scheduled_ms: Some(T0 - 1000), ..sent(0, "ada@example.com", "Soon", "scheduled") });
    for i in 1..4 {
        st.add_sent(sent(i, "x@example.com", &format!("Newer {i}"), "delivered"));
    }
    let fake = FakeResend::start(st).await;
    let mut cfg = config(&fake);
    cfg.page_size = 2;
    let p = ResendProvider::new(cfg).unwrap();
    let (_, cursor) = sync_all(&p, None).await;
    fake.lock().sent_by_id(&sid).unwrap().last_event = "delivered".into();
    let d = p.sync(Some(&cursor)).await.unwrap();
    let m = d.upserted.iter().find(|m| m.id.as_str() == format!("s:{sid}")).expect("re-read when due, though off the newest page");
    assert_eq!(m.mailboxes, vec![MailboxId::new("sent")]);
    let d = p.sync(d.cursor.as_ref()).await.unwrap();
    assert!(d.upserted.is_empty(), "no longer tracked");
}

#[tokio::test]
async fn delivery_state_changes_are_reported() {
    let mut st = FakeState::new();
    let sid = st.add_sent(sent(0, "ada@example.com", "Welcome", "delivered"));
    st.add_sent(sent(1, "bo@example.com", "Welcome", "opened"));
    let fake = FakeResend::start(st).await;
    let p = ResendProvider::new(config(&fake)).unwrap();
    let (_, cursor) = sync_all(&p, None).await;
    fake.lock().sent_by_id(&sid).unwrap().last_event = "bounced".into();
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert_eq!(d.upserted.len(), 1, "only the one that changed");
    let m = &d.upserted[0];
    assert_eq!(m.mailboxes, vec![MailboxId::new("sent"), MailboxId::new("bounced")]);
    assert_eq!(m.snippet, "Bounced");
    let b = p.fetch_body(&m.id).await.unwrap();
    assert_eq!(b.delivery(), Some(DeliveryState::Bounced));
    assert_eq!(b.text.as_deref(), Some("Text of Welcome"));
    let d = p.sync(d.cursor.as_ref()).await.unwrap();
    assert!(d.upserted.is_empty());
}

#[tokio::test]
async fn inbound_mail_with_attachments() {
    let mut st = FakeState::new();
    let rid = st.add_received(FakeReceived {
        headers: vec![
            ("List-Unsubscribe".into(), "<mailto:unsub@sender.example>".into()),
            ("Received".into(), "from a".into()),
            ("Received".into(), "from b".into()),
            ("In-Reply-To".into(), "<earlier@shop.example>".into()),
        ],
        html: Some("<p>Photo: <img src=\"cid:img1\"></p>".into()),
        attachments: vec![
            FakeAttachment { filename: "plan.pdf".into(), content_type: "application/pdf".into(), bytes: b"%PDF-made-up".to_vec(), ..FakeAttachment::default() },
            FakeAttachment { filename: "photo.png".into(), content_type: "image/png".into(), content_id: Some("img1".into()), inline: true, bytes: vec![1, 2, 3], ..FakeAttachment::default() },
        ],
        raw: Some(b"From: c@example.com\r\nSubject: Plans\r\n\r\nmade up\r\n".to_vec()),
        ..received(0, "Cy <cy@example.com>", "Plans")
    });
    let fake = FakeResend::start(st).await;
    let mut cfg = config(&fake);
    cfg.max_download_bytes = 1024;
    let p = ResendProvider::new(cfg).unwrap();
    let (pages, _) = sync_all(&p, None).await;
    let m = pages[0].upserted.iter().find(|m| m.id.as_str() == format!("r:{rid}")).unwrap();
    assert!(m.has_attachments);
    assert_eq!(m.in_reply_to.as_deref(), Some("earlier@shop.example"));
    let b = p.fetch_body(&m.id).await.unwrap();
    assert_eq!(b.header("List-Unsubscribe"), Some("<mailto:unsub@sender.example>"));
    assert_eq!(b.header("Authentication-Results"), Some("resend.com; spf=pass; dkim=pass; dmarc=pass"));
    assert_eq!(b.attachments.len(), 2);
    let pdf = b.attachments.iter().find(|a| a.filename == "plan.pdf").unwrap();
    let img = b.attachments.iter().find(|a| a.filename == "photo.png").unwrap();
    assert_eq!(img.content_id.as_deref(), Some("img1"));
    assert_eq!(p.fetch_attachment(&m.id, &pdf.id).await.unwrap(), b"%PDF-made-up");
    assert_eq!(p.fetch_attachment(&m.id, &img.id).await.unwrap(), vec![1, 2, 3]);
    assert!(p.fetch_raw(&m.id).await.unwrap().starts_with(b"From: c@example.com"));
    {
        let s = fake.lock();
        let downloads: Vec<_> = s.requests.iter().filter(|r| r.path.starts_with("/_download/")).collect();
        assert_eq!(downloads.len(), 3);
        assert!(downloads.iter().all(|r| !r.authorization), "the key never goes to a download link");
        assert!(s.requests.iter().any(|r| r.path.contains("html_format=cid")));
    }
    // Over the size cap: refused, not truncated.
    fake.lock().received[0].attachments[0].bytes = vec![0; 4096];
    assert!(matches!(p.fetch_attachment(&m.id, &pdf.id).await, Err(Error::Refused(_))));
    assert!(matches!(p.fetch_attachment(&m.id, &AttachmentId::new("../x")).await, Err(Error::NotFound(_))));
    assert!(matches!(p.fetch_raw(&MessageId::new("s:00000001-0000-4000-8000-000000000001")).await, Err(Error::Unsupported(_))));
}

#[tokio::test]
async fn a_bad_key_is_an_auth_error_without_the_key() {
    let fake = FakeResend::start(FakeState::new()).await;
    let mut cfg = config(&fake);
    cfg.key = ApiKey::new("re_wrong_madeup_key_123456");
    let p = ResendProvider::new(cfg).unwrap();
    let e = p.sync(None).await.unwrap_err();
    assert!(matches!(e, Error::Auth(_)), "{e:?}");
    let shown = format!("{e} {e:?}");
    assert!(!shown.contains("wrong_madeup") && !shown.contains(FAKE_KEY), "{shown}");
    let e = p.apply(&Action::Send { draft: draft("x") }).await.unwrap_err();
    assert!(matches!(e, Error::Auth(_)), "{e:?}");
    assert!(fake.lock().sent.is_empty());
    let mut cfg = config(&fake);
    cfg.base_url = "http://api.resend.example".into();
    assert!(matches!(ResendProvider::new(cfg), Err(Error::Refused(_))), "never the key over plain http");
}

#[tokio::test]
async fn a_sending_only_key_syncs_nothing_and_still_sends() {
    let mut st = FakeState::new();
    st.sending_only = true;
    st.add_sent(sent(0, "a@example.com", "x", "delivered"));
    let fake = FakeResend::start(st).await;
    let p = ResendProvider::new(config(&fake)).unwrap();
    let d = p.sync(None).await.unwrap();
    assert!(d.upserted.is_empty() && !d.more);
    assert!(p.is_send_only());
    let before = fake.lock().api_calls();
    p.sync(d.cursor.as_ref()).await.unwrap();
    assert_eq!(fake.lock().api_calls(), before, "no more listing");
    p.apply(&Action::Send { draft: draft("s1") }).await.unwrap();
    assert_eq!(fake.lock().sent.len(), 2);
}

#[tokio::test]
async fn local_only_actions_touch_nothing_and_say_so() {
    let fake = FakeResend::start(FakeState::new()).await;
    let p = ResendProvider::new(config(&fake)).unwrap();
    let c = p.capabilities();
    assert_eq!((c.seen, c.flagged, c.drafts, c.archive), (Support::Local, Support::Local, Support::Local, Support::Local));
    assert!(c.send && c.note.is_some());
    let id = MessageId::new("r:00000001-0000-4000-8000-000000000001");
    for a in [
        Action::SetSeen { ids: vec![id.clone()], seen: true },
        Action::SetFlagged { ids: vec![id.clone()], flagged: true },
        Action::Archive { ids: vec![id.clone()] },
        Action::Trash { ids: vec![id.clone()] },
        Action::Move { ids: vec![id.clone()], from: Some(MailboxId::new("inbox")), to: MailboxId::new("archive") },
        Action::SaveDraft { draft: draft("d") },
        Action::DeleteDraft { local_id: "d".into(), server_id: None },
    ] {
        assert_eq!(p.apply(&a).await.unwrap(), ApplyOutcome::default(), "{a:?}");
    }
    assert!(matches!(
        p.apply(&Action::Move { ids: vec![id], from: None, to: MailboxId::new("bounced") }).await,
        Err(Error::Unsupported(_))
    ));
    assert_eq!(fake.lock().api_calls(), 0, "nothing reached Resend");
    let t = Instant::now();
    p.wait_for_change(3600).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "waits the poll interval, not the hour");
}

#[tokio::test]
async fn domains_and_key_names() {
    let fake = FakeResend::start(FakeState::new()).await;
    let p = ResendProvider::new(config(&fake)).unwrap();
    let d = p.domains().await.unwrap();
    assert_eq!(d[0].name, "shop.example");
    assert!(d[0].can_send() && d[0].receives());
    let k = p.api_keys().await.unwrap();
    assert_eq!(k[0].name, "shop website");
    assert!(!serde_json::to_string(&k).unwrap().contains("re_"), "names and dates only");
}
