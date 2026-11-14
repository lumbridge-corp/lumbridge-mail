//! `GogProvider` against recorded, made-up gog output (tests/fixtures, written by gen_fixtures.py from
//! gogcli v0.29.0's output structs): bodies, attachments, actions, drafts, sends. No real account, no
//! real gog. Sync is in tests/sync.rs.

use lumbridge_mail_core::*;
use lumbridge_mail_gog::fake::FakeRunner;
use lumbridge_mail_gog::{Arg, ComposeMode, GogCall, GogConfig, GogOutput, GogProvider, mime};
use std::sync::Arc;

const M3: &str = "18f0a1b2c3d4e503";
const M2: &str = "18f0a1b2c3d4e502";
const M1: &str = "18f0a1b2c3d4e501";
const M0: &str = "18f0a1b2c3d4e500";
const M4: &str = "18f0a1b2c3d4e504";
const M9: &str = "18f0a1b2c3d4e509";
const T2: &str = "18f0a1b2c3d4e502";
const EMAIL: &str = "test.user@example.com";
const NOT_FOUND: &str = "Google API error (404 notFound): Requested entity was not found.";

fn fx(name: &str) -> serde_json::Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap()
}

/// A fake that knows the label list and the INBOX counts, plus every message's metadata.
fn fake() -> Arc<FakeRunner> {
    let f = Arc::new(FakeRunner::new());
    f.on_json("gmail.labels.list", &[], fx("labels_list.json"));
    f.on_json("gmail.labels.get", &["INBOX"], fx("label_get_inbox.json"));
    for (id, file) in [(M3, "m3_metadata.json"), (M2, "m2_metadata.json"), (M1, "m1_metadata.json"), (M0, "m0_metadata.json"), (M4, "m4_metadata.json")] {
        f.on_json("gmail.raw", &[id, "metadata"], fx(file));
    }
    f.on("gmail.raw", &[M9], GogOutput::fail(5, NOT_FOUND));
    f
}

fn provider(f: &Arc<FakeRunner>, mode: ComposeMode) -> GogProvider {
    let mut cfg = GogConfig::new(AccountId::new("personal-gmail"), EMAIL);
    cfg.name = Some("Test User".into());
    cfg.compose = mode;
    GogProvider::new(cfg, f.clone())
}

fn lit(call: &GogCall) -> Vec<String> {
    call.display_args()
}

fn has_seq(args: &[String], seq: &[&str]) -> bool {
    args.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
}

/// Every call carries the guards, and only `gmail.send` lacks `--gmail-no-send`.
fn assert_guarded(f: &FakeRunner) {
    for c in f.calls() {
        let a = lit(&c);
        let cmd = c.command().expect("every call names its one enabled command").to_owned();
        assert!(has_seq(&a, &["--enable-commands-exact", &cmd]), "{a:?}");
        assert!(a.contains(&"--no-input".to_owned()) && a.contains(&"--json".to_owned()), "{a:?}");
        assert!(has_seq(&a, &["--color", "never"]) && has_seq(&a, &["--account", EMAIL]), "{a:?}");
        assert!(a.contains(&"--wrap-untrusted=false".to_owned()), "{a:?}");
        let no_send = a.contains(&"--gmail-no-send".to_owned());
        assert_eq!(no_send, cmd != "gmail.send", "{cmd}: {a:?}");
        // The dotted command is also the subcommand path.
        let path: Vec<&str> = cmd.split('.').collect();
        assert!(has_seq(&a, &path), "{a:?}");
    }
}


// The sync paths (listings, windows, backfill, bundles) are in tests/sync.rs.

#[tokio::test]
async fn bodies_walk_the_mime_tree() {
    let f = fake();
    f.on_json("gmail.raw", &[M3, "full"], fx("m3_full.json"));
    let p = provider(&f, ComposeMode::Raw);
    let b = p.fetch_body(&MessageId::new(M3)).await.unwrap();
    assert_eq!(b.text.as_deref(), Some("Can we move the garden meeting to Friday?\r\nI'll bring the seeds.\r\n\nGrüße aus dem Garten"));
    assert_eq!(b.html.as_deref(), Some("<p>Can we move the garden meeting to <b>Friday</b>?</p>"));
    let atts: Vec<(&str, &str, &str, u64, Option<&str>)> =
        b.attachments.iter().map(|a| (a.id.as_str(), a.filename.as_str(), a.mime.as_str(), a.size, a.content_id.as_deref())).collect();
    assert_eq!(atts, vec![
        ("2", "garden-plan.pdf", "application/pdf", 40960, None),
        ("3", "attachment", "image/png", 11, Some("seed-photo@example.com")),
    ]);
    assert!(b.headers.iter().any(|(k, v)| k == "List-Unsubscribe" && v == "<mailto:unsub@example.com>"));
    assert!(!b.headers.iter().any(|(k, _)| k == "Subject"), "only the kept headers");
    assert!(has_seq(&lit(&f.calls()[0]), &["gmail", "raw", M3, "--format", "full"]));
}

#[tokio::test]
async fn raw_source_and_attachments() {
    let f = fake();
    f.on_json("gmail.raw", &[M3, "raw"], fx("m3_raw.json"));
    f.on_json("gmail.raw", &[M3, "full"], fx("m3_full.json"));
    f.on("gmail.attachment", &[M3, "ANGjdJ8-made-up-attachment-id"], GogOutput {
        code: 0,
        stdout: serde_json::to_vec(&fx("attachment_download.json")).unwrap(),
        stderr: String::new(),
        file: Some(b"%PDF-".to_vec()),
    });
    let p = provider(&f, ComposeMode::Raw);

    let raw = p.fetch_raw(&MessageId::new(M3)).await.unwrap();
    assert!(String::from_utf8(raw).unwrap().starts_with("From: \"Lovelace, Ada\" <ada@example.com>\r\n"));

    // A big attachment: looked up by partId, then downloaded through an --out file.
    assert_eq!(p.fetch_attachment(&MessageId::new(M3), &AttachmentId::new("2")).await.unwrap(), b"%PDF-");
    let dl = f.calls_to("gmail.attachment").pop().unwrap();
    assert!(dl.args.ends_with(&[Arg::from("--out"), Arg::Out]), "{:?}", dl.args);
    // A small inline part comes straight from the payload, no download.
    let png = p.fetch_attachment(&MessageId::new(M3), &AttachmentId::new("3")).await.unwrap();
    assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    assert_eq!(f.calls_to("gmail.attachment").len(), 1);
    assert!(matches!(p.fetch_attachment(&MessageId::new(M3), &AttachmentId::new("9")).await, Err(Error::NotFound(_))));
    assert_guarded(&f);
}

#[tokio::test]
async fn label_actions_are_batch_modifies() {
    let f = fake();
    f.on_json("gmail.batch.modify", &[], fx("batch_modify.json"));
    let p = provider(&f, ComposeMode::Raw);
    let ids = vec![MessageId::new(M3), MessageId::new(M2)];
    let cases: Vec<(Action, &[&str])> = vec![
        (Action::SetSeen { ids: ids.clone(), seen: true }, &["--remove", "UNREAD"]),
        (Action::SetSeen { ids: ids.clone(), seen: false }, &["--add", "UNREAD"]),
        (Action::SetFlagged { ids: ids.clone(), flagged: true }, &["--add", "STARRED"]),
        (Action::SetFlagged { ids: ids.clone(), flagged: false }, &["--remove", "STARRED"]),
        (Action::Archive { ids: ids.clone() }, &["--remove", "INBOX"]),
        (Action::Trash { ids: ids.clone() }, &["--add", "TRASH", "--remove", "INBOX"]),
        (Action::Move { ids: ids.clone(), from: Some(MailboxId::new("INBOX")), to: MailboxId::new("Label_11") }, &["--add", "Label_11", "--remove", "INBOX"]),
        (Action::Move { ids: ids.clone(), from: Some(MailboxId::new("Label_11")), to: MailboxId::new("SPAM") }, &["--add", "SPAM", "--remove", "Label_11,INBOX"]),
        (Action::Move { ids: ids.clone(), from: Some(MailboxId::new("TRASH")), to: MailboxId::new("ALL") }, &["--remove", "TRASH"]),
    ];
    for (action, want) in cases {
        f.clear_calls();
        p.apply(&action).await.unwrap();
        // Applying twice is harmless: the same label change again.
        p.apply(&action).await.unwrap();
        let calls = f.calls_to("gmail.batch.modify");
        assert_eq!(calls.len(), 2, "{action:?}");
        let a = lit(&calls[0]);
        assert!(has_seq(&a, &["gmail", "batch", "modify", M3, M2]), "{a:?}");
        assert!(has_seq(&a, want), "{action:?}: {a:?}");
        assert!(a.ends_with(&want.iter().map(|s| s.to_string()).collect::<Vec<_>>()), "{action:?}: {a:?}");
    }
    // Nothing to do: no call.
    f.clear_calls();
    p.apply(&Action::Move { ids: ids.clone(), from: None, to: MailboxId::new("ALL") }).await.unwrap();
    p.apply(&Action::Archive { ids: vec![] }).await.unwrap();
    assert!(f.calls().is_empty());
    // Big batches split at Gmail's 1000-id limit.
    let many: Vec<MessageId> = (0..2500).map(|i| MessageId::new(format!("18f0{i:012x}"))).collect();
    p.apply(&Action::Archive { ids: many }).await.unwrap();
    assert_eq!(f.calls_to("gmail.batch.modify").len(), 3);
    // An id that would read as a flag is refused before anything runs.
    f.clear_calls();
    let bad = Action::Archive { ids: vec![MessageId::new("--enable-commands=all")] };
    assert!(matches!(p.apply(&bad).await, Err(Error::Invalid(_))));
    assert!(f.calls().is_empty());
    // A message deleted meanwhile is not an error.
    f.on("gmail.batch.modify", &[M9], GogOutput::fail(5, NOT_FOUND));
    p.apply(&Action::Archive { ids: vec![MessageId::new(M9)] }).await.unwrap();
    assert_guarded(&f);
}

fn reply_draft() -> Draft {
    Draft {
        local_id: "draft-local-7".into(),
        to: vec![Address::new(Some("Ada Lovelace"), "ada@example.com")],
        subject: "Re: Garden meeting".into(),
        text: "Friday works. I'll bring the plan.".into(),
        reply_to: Some(MessageId::new(M3)),
        ..Default::default()
    }
}

#[tokio::test]
async fn send_builds_threaded_mime_and_is_the_only_unguarded_call() {
    let f = fake();
    let draft = reply_draft();
    let mid = mime::message_id_for(&draft.local_id, EMAIL).unwrap();
    f.on_json("gmail.messages.search", &[&format!("in:sent rfc822msgid:{mid}")], fx("search_empty.json"));
    f.on_json("gmail.messages.search", &[&format!("in:drafts rfc822msgid:{mid}")], fx("search_empty.json"));
    f.on_json("gmail.send", &[], fx("send.json"));
    let p = provider(&f, ComposeMode::Raw);
    let sent = p.apply(&Action::Send { draft }).await.unwrap();
    assert_eq!(sent.server_id.as_deref(), Some("18f0a1b2c3d4e5e1"));

    let sends = f.calls_to("gmail.send");
    assert_eq!(sends.len(), 1);
    let a = lit(&sends[0]);
    assert!(has_seq(&a, &["gmail", "send", "--raw-file", "-", "--thread-id", T2]), "{a:?}");
    let mime = String::from_utf8(sends[0].stdin.clone().unwrap()).unwrap();
    // Unfold long header lines before looking at them.
    let head = mime.split("\r\n\r\n").next().unwrap().replace("\r\n ", " ").replace("\r\n\t", " ");
    assert!(head.contains(&format!("Message-ID: <{mid}>")), "{head}");
    assert!(head.contains("From: \"Test User\" <test.user@example.com>"), "{head}");
    assert!(head.contains("To: \"Ada Lovelace\" <ada@example.com>"), "{head}");
    assert!(head.contains("In-Reply-To: <CAF-ada-3@mail.example.com>"), "{head}");
    assert!(head.contains("References: <CAF-ada-1@mail.example.com> <CAF-test-2@mail.example.com>"), "{head}");
    assert!(head.contains("<CAF-ada-3@mail.example.com>\r\n"), "parent id ends References: {head}");
    assert!(mime.contains("Friday works."));
    // Checked for an earlier delivery first, then sent, then looked for the draft to clean up.
    assert_eq!(f.commands(), vec!["gmail.raw", "gmail.messages.search", "gmail.send", "gmail.messages.search"]);
    assert_guarded(&f);
}

#[tokio::test]
async fn a_send_that_already_went_out_is_not_sent_again() {
    let f = fake();
    let draft = reply_draft();
    let mid = mime::message_id_for(&draft.local_id, EMAIL).unwrap();
    f.on_json("gmail.messages.search", &[&format!("in:sent rfc822msgid:{mid}")], serde_json::json!({
        "messages": [{"id": "18f0a1b2c3d4e5e1", "threadId": T2, "labels": ["SENT"]}], "nextPageToken": ""}));
    f.on_json("gmail.messages.search", &[&format!("in:drafts rfc822msgid:{mid}")], fx("search_empty.json"));
    let p = provider(&f, ComposeMode::Raw);
    p.apply(&Action::Send { draft }).await.unwrap();
    assert!(f.calls_to("gmail.send").is_empty());
}

#[tokio::test]
async fn sends_need_recipients_and_failures_keep_their_kind() {
    let f = fake();
    let p = provider(&f, ComposeMode::Raw);
    let mut d = reply_draft();
    d.to.clear();
    assert!(matches!(p.apply(&Action::Send { draft: d }).await, Err(Error::Invalid(_))));

    let draft = reply_draft();
    let mid = mime::message_id_for(&draft.local_id, EMAIL).unwrap();
    f.on_json("gmail.messages.search", &[&format!("in:sent rfc822msgid:{mid}")], fx("search_empty.json"));
    f.on("gmail.send", &[], GogOutput::fail(2, "Gmail sending is blocked by config gmail_no_send"));
    assert!(matches!(p.apply(&Action::Send { draft: draft.clone() }).await, Err(Error::Refused(_))));
    f.on_err("gmail.send", &[], Error::Offline("gmail.send: timed out after 180s".into()));
    let e = p.apply(&Action::Send { draft }).await.unwrap_err();
    assert!(e.is_retryable(), "{e:?}");
}

#[tokio::test]
async fn flags_mode_sends_through_gogs_own_composer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plan v2.txt");
    std::fs::write(&path, b"rows of beans").unwrap();
    let f = fake();
    f.on_json("gmail.messages.search", &["in:sent newer_than:1d subject:\"Re: Garden meeting\""], fx("search_empty.json"));
    f.on_json("gmail.send", &[], fx("send.json"));
    let p = provider(&f, ComposeMode::Flags);
    let mut d = reply_draft();
    d.cc = vec![Address::new(Some("Bob, Jr."), "bob@example.org")];
    d.html = Some("<p>Friday works.</p>".into());
    d.attachments = vec![DraftAttachment { path: path.to_string_lossy().into(), filename: "plan v2.txt".into(), mime: "text/plain".into() }];
    p.apply(&Action::Send { draft: d }).await.unwrap();

    let send = f.calls_to("gmail.send").pop().unwrap();
    let a = lit(&send);
    assert!(has_seq(&a, &["--to", "Ada Lovelace <ada@example.com>", "--cc", "Bob Jr. <bob@example.org>"]), "{a:?}");
    assert!(has_seq(&a, &["--subject", "Re: Garden meeting", "--body-file", "-"]), "{a:?}");
    assert!(has_seq(&a, &["--body-html-file", "<in:body.html>"]), "{a:?}");
    assert!(has_seq(&a, &["--reply-to-message-id", M3]), "{a:?}");
    assert!(has_seq(&a, &["--attach", "<in:a0/plan v2.txt>"]), "{a:?}");
    assert_eq!(send.stdin.as_deref(), Some(&b"Friday works. I'll bring the plan."[..]));
    assert_eq!(send.inputs, vec![
        ("body.html".to_owned(), b"<p>Friday works.</p>".to_vec()),
        ("a0/plan v2.txt".to_owned(), b"rows of beans".to_vec()),
    ]);
    assert!(!a.contains(&"--gmail-no-send".to_owned()));
    assert_guarded(&f);
}

#[tokio::test]
async fn flags_mode_does_not_resend_a_matching_recent_message() {
    let f = fake();
    let mut sent = fx("m2_metadata.json");
    sent["internalDate"] = serde_json::json!(format!("{}", (now() - 60) * 1000));
    sent["snippet"] = serde_json::json!("Friday works. I&#39;ll bring the plan.");
    sent["payload"]["headers"] = serde_json::json!([
        {"name": "To", "value": "Ada Lovelace <ADA@example.com>"}, {"name": "Subject", "value": "Re: Garden meeting"}]);
    f.on_json("gmail.raw", &[M2, "metadata"], sent);
    f.on_json("gmail.messages.search", &["in:sent newer_than:1d subject:\"Re: Garden meeting\""], serde_json::json!({
        "messages": [{"id": M2, "threadId": T2, "labels": ["SENT"]}], "nextPageToken": ""}));
    let p = provider(&f, ComposeMode::Flags);
    p.apply(&Action::Send { draft: reply_draft() }).await.unwrap();
    assert!(f.calls_to("gmail.send").is_empty());

    // Different text: a new message, so it is sent.
    f.on_json("gmail.send", &[], fx("send.json"));
    let mut other = reply_draft();
    other.text = "Actually, Saturday?".into();
    p.apply(&Action::Send { draft: other }).await.unwrap();
    assert_eq!(f.calls_to("gmail.send").len(), 1);
}

#[tokio::test]
async fn drafts_save_update_and_delete_by_local_id() {
    let f = fake();
    let draft = Draft { reply_to: None, ..reply_draft() };
    let mid = mime::message_id_for(&draft.local_id, EMAIL).unwrap();
    f.on_json("gmail.messages.search", &[&format!("in:drafts rfc822msgid:{mid}")], fx("search_empty.json"));
    f.on_json("gmail.drafts.create", &[], fx("draft_create.json"));
    f.on_json("gmail.drafts.update", &["r-2222222222222222222"], fx("draft_create.json"));
    f.on_json("gmail.drafts.delete", &["r-2222222222222222222", "--force"], fx("deleted.json"));
    let p = provider(&f, ComposeMode::Raw);

    let saved = p.apply(&Action::SaveDraft { draft: draft.clone() }).await.unwrap();
    assert_eq!(saved.server_id.as_deref(), Some("r-2222222222222222222"));
    let create = f.calls_to("gmail.drafts.create").pop().unwrap();
    assert!(has_seq(&lit(&create), &["gmail", "drafts", "create", "--raw-file", "-"]));
    assert!(String::from_utf8(create.stdin.unwrap()).unwrap().contains(&format!("Message-ID: <{mid}>")));

    // Saved again: replaces the same Gmail draft, found from memory (no search this time).
    f.clear_calls();
    p.apply(&Action::SaveDraft { draft: draft.clone() }).await.unwrap();
    assert_eq!(f.commands(), vec!["gmail.drafts.update"]);
    assert!(has_seq(&lit(&f.calls()[0]), &["gmail", "drafts", "update", "r-2222222222222222222", "--raw-file", "-"]));

    p.apply(&Action::DeleteDraft { local_id: draft.local_id.clone(), server_id: None }).await.unwrap();
    assert_eq!(f.calls_to("gmail.drafts.delete").len(), 1);
    // Deleting again is a no-op (nothing found any more).
    f.clear_calls();
    p.apply(&Action::DeleteDraft { local_id: draft.local_id.clone(), server_id: None }).await.unwrap();
    assert!(f.calls_to("gmail.drafts.delete").is_empty());
    assert_guarded(&f);
}

#[tokio::test]
async fn after_a_restart_raw_drafts_are_found_by_message_id() {
    let f = fake();
    let draft = Draft { reply_to: None, ..reply_draft() };
    let mid = mime::message_id_for(&draft.local_id, EMAIL).unwrap();
    f.on_json("gmail.messages.search", &[&format!("in:drafts rfc822msgid:{mid}")], serde_json::json!({
        "messages": [{"id": "18f0a1b2c3d4e5d2", "threadId": T2, "labels": ["DRAFT"]}], "nextPageToken": ""}));
    f.on_json("gmail.drafts.list", &["--all"], fx("drafts_list.json"));
    f.on_json("gmail.drafts.update", &["r-2222222222222222222"], fx("draft_create.json"));
    let p = provider(&f, ComposeMode::Raw);
    p.apply(&Action::SaveDraft { draft }).await.unwrap();
    assert_eq!(f.commands(), vec!["gmail.messages.search", "gmail.drafts.list", "gmail.drafts.update"]);

    // Deleted on the web meanwhile: the update 404s and a new draft is created.
    let f2 = fake();
    f2.on_json("gmail.messages.search", &[&format!("in:drafts rfc822msgid:{mid}")], serde_json::json!({
        "messages": [{"id": "18f0a1b2c3d4e5d2", "threadId": T2, "labels": ["DRAFT"]}], "nextPageToken": ""}));
    f2.on_json("gmail.drafts.list", &["--all"], fx("drafts_list.json"));
    f2.on("gmail.drafts.update", &[], GogOutput::fail(5, NOT_FOUND));
    f2.on_json("gmail.drafts.create", &[], fx("draft_create.json"));
    let p2 = provider(&f2, ComposeMode::Raw);
    p2.apply(&Action::SaveDraft { draft: Draft { reply_to: None, ..reply_draft() } }).await.unwrap();
    assert_eq!(f2.calls_to("gmail.drafts.create").len(), 1);
}

#[tokio::test]
async fn auto_mode_picks_raw_mime_only_on_new_enough_gog() {
    for (fixture, raw) in [("version_0_29.json", false), ("version_0_48.json", true)] {
        let f = fake();
        f.on_json("version", &[], fx(fixture));
        f.on_json("gmail.messages.search", &[], fx("search_empty.json"));
        f.on_json("gmail.drafts.create", &[], fx("draft_create.json"));
        let p = provider(&f, ComposeMode::Auto);
        let d = Draft { reply_to: None, ..reply_draft() };
        p.apply(&Action::SaveDraft { draft: d.clone() }).await.unwrap();
        p.apply(&Action::SaveDraft { draft: Draft { local_id: "other".into(), ..d } }).await.unwrap();
        assert_eq!(f.calls_to("version").len(), 1, "asked once");
        let a = lit(&f.calls_to("gmail.drafts.create")[0]);
        assert_eq!(a.contains(&"--raw-file".to_owned()), raw, "{fixture}: {a:?}");
        assert_eq!(a.contains(&"--body-file".to_owned()), !raw, "{fixture}: {a:?}");
        assert_guarded(&f);
    }
}

#[tokio::test]
async fn flags_mode_drafts_stay_in_memory_and_empty_ones_wait() {
    let f = fake();
    f.on_json("gmail.drafts.create", &[], fx("draft_create.json"));
    f.on_json("gmail.drafts.update", &["r-2222222222222222222"], fx("draft_create.json"));
    let p = provider(&f, ComposeMode::Flags);
    p.apply(&Action::SaveDraft { draft: Draft { local_id: "blank".into(), ..Default::default() } }).await.unwrap();
    assert!(f.calls().is_empty(), "nothing to store yet");

    let d = Draft { reply_to: None, ..reply_draft() };
    p.apply(&Action::SaveDraft { draft: d.clone() }).await.unwrap();
    p.apply(&Action::SaveDraft { draft: d.clone() }).await.unwrap();
    let up = lit(&f.calls_to("gmail.drafts.update")[0]);
    assert!(has_seq(&up, &["update", "r-2222222222222222222", "--to"]), "{up:?}");
    assert!(up.contains(&"--clear-attachments".to_owned()), "no attachments now: {up:?}");
    // Not in memory (another process saved it): no guessing, nothing deleted.
    let p2 = provider(&f, ComposeMode::Flags);
    f.clear_calls();
    p2.apply(&Action::DeleteDraft { local_id: d.local_id, server_id: None }).await.unwrap();
    assert!(f.calls().is_empty());
}

#[tokio::test]
async fn errors_map_to_core_kinds() {
    let f = Arc::new(FakeRunner::new());
    f.on("gmail.labels.list", &[], GogOutput::fail(4, "auth required for test.user@example.com; run: gog auth add"));
    let p = provider(&f, ComposeMode::Raw);
    assert!(matches!(p.sync(None).await, Err(Error::Auth(_))));

    f.on_err("gmail.labels.list", &[], Error::Offline("ssh gog-host: Connection timed out".into()));
    assert!(p.sync(None).await.unwrap_err().is_retryable());

    f.on("gmail.raw", &[M9], GogOutput::fail(5, NOT_FOUND));
    assert!(matches!(p.fetch_body(&MessageId::new(M9)).await, Err(Error::NotFound(_))));
    f.on("gmail.raw", &[M3], GogOutput { code: 0, stdout: b"not json".to_vec(), ..Default::default() });
    assert!(matches!(p.fetch_body(&MessageId::new(M3)).await, Err(Error::Other(_))));
}

#[tokio::test]
async fn wait_for_change_just_sleeps() {
    let f = Arc::new(FakeRunner::new());
    let p = provider(&f, ComposeMode::Raw);
    let t = std::time::Instant::now();
    p.wait_for_change(1).await.unwrap();
    assert!(t.elapsed() >= std::time::Duration::from_millis(900));
    assert!(f.calls().is_empty());
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

#[tokio::test]
async fn the_stored_draft_id_is_used_after_a_restart_in_flags_mode() {
    // gog 0.29 (flags mode) cannot find a draft by Message-ID; the store's server_id is what finds it.
    let f = fake();
    f.on_json("gmail.drafts.update", &["r-2222222222222222222"], fx("draft_create.json"));
    f.on_json("gmail.drafts.delete", &["r-2222222222222222222", "--force"], fx("deleted.json"));
    let p = provider(&f, ComposeMode::Flags);
    let draft = Draft { reply_to: None, server_id: Some("r-2222222222222222222".into()), ..reply_draft() };
    let out = p.apply(&Action::SaveDraft { draft: draft.clone() }).await.unwrap();
    assert_eq!(out.server_id.as_deref(), Some("r-2222222222222222222"));
    assert_eq!(f.commands(), vec!["gmail.drafts.update"]);

    let fresh = provider(&f, ComposeMode::Flags);
    f.clear_calls();
    fresh
        .apply(&Action::DeleteDraft { local_id: draft.local_id.clone(), server_id: draft.server_id.clone() })
        .await
        .unwrap();
    assert_eq!(f.commands(), vec!["gmail.drafts.delete"]);
    assert_guarded(&f);
}

#[tokio::test]
async fn a_cursor_from_somewhere_else_asks_for_a_resync() {
    let f = fake();
    let p = provider(&f, ComposeMode::Raw);
    let e = p.sync(Some(&SyncCursor("imap1:{}".into()))).await.unwrap_err();
    assert!(matches!(e, Error::ResyncNeeded(_)), "{e:?}");
}
