//! `ImapProvider` against the in-process fake IMAP and SMTP servers. Made-up mail only.

mod common;

use common::fake_imap::{self, FakeFolder, Login};
use common::fake_smtp::{self, SmtpState};
use common::*;
use lumbridge_mail_core::{
    Action, Address, AttachmentId, Delta, Draft, Error, MailboxId, MailboxRole, MessageId, Provider, SyncCursor,
};
use lumbridge_mail_imap::{Credentials, ImapProvider, Security, Server};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ids(d: &Delta) -> Vec<String> {
    d.upserted.iter().map(|m| m.id.to_string()).collect()
}

/// Syncs until `more` is false; returns every delta.
async fn sync_all(p: &ImapProvider, mut cursor: Option<SyncCursor>) -> (Vec<Delta>, SyncCursor) {
    let mut out = Vec::new();
    for _ in 0..50 {
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

#[tokio::test]
async fn first_sync_is_paged_newest_first() {
    let server = fake_imap::serve(state(7, BASIC));
    let mut acct = account(server.addr, None);
    acct.options.page_size = 3;
    let p = ImapProvider::new(acct);

    let d1 = p.sync(None).await.unwrap();
    assert_eq!(ids(&d1), vec!["INBOX:100:7", "INBOX:100:6", "INBOX:100:5"]);
    assert!(d1.more);
    let inbox = d1.mailboxes.iter().flatten().find(|m| m.id.as_str() == "INBOX").unwrap();
    assert_eq!((inbox.role, inbox.name.as_str(), inbox.total, inbox.unread), (MailboxRole::Inbox, "Inbox", 7, 4));
    let roles: Vec<_> = d1.mailboxes.iter().flatten().map(|m| (m.id.to_string(), m.role)).collect();
    assert!(roles.contains(&("Sent".into(), MailboxRole::Sent)));
    assert!(roles.contains(&("Trash".into(), MailboxRole::Trash)));
    // Inbox first.
    assert_eq!(d1.mailboxes.as_ref().unwrap()[0].id.as_str(), "INBOX");

    let d2 = p.sync(d1.cursor.as_ref()).await.unwrap();
    assert_eq!(ids(&d2), vec!["INBOX:100:4", "INBOX:100:3", "INBOX:100:2"]);
    assert!(d2.more);
    let d3 = p.sync(d2.cursor.as_ref()).await.unwrap();
    assert_eq!(ids(&d3), vec!["INBOX:100:1"]);
    assert!(!d3.more);
    let m = &d3.upserted[0];
    assert_eq!(m.subject, "Message 1");
    assert_eq!(m.snippet, "Body number 1.");
    assert_eq!(m.from[0].email, "ada@example.com");
    assert_eq!(m.mailboxes, vec![MailboxId::new("INBOX")]);
    assert_eq!(m.rfc_message_id.as_deref(), Some("m1@example.com"));
    assert!(!m.flags.seen);
    assert!(d2.upserted.iter().find(|m| m.id.as_str() == "INBOX:100:2").unwrap().flags.seen);

    // Nothing changed: an empty delta.
    let d4 = p.sync(d3.cursor.as_ref()).await.unwrap();
    assert!(d4.upserted.is_empty() && d4.removed.is_empty() && !d4.more);
}

async fn changes_are_found(capabilities: &str) {
    let server = fake_imap::serve(state(5, capabilities));
    let p = ImapProvider::new(account(server.addr, None));
    let (first, cursor) = sync_all(&p, None).await;
    assert_eq!(first.iter().map(|d| d.upserted.len()).sum::<usize>(), 5);

    {
        let mut s = server.lock();
        let inbox = s.folder_mut("INBOX");
        inbox.add(msg(6, &[]));
        inbox.set_flags(1, &["\\Seen", "\\Flagged"]);
        inbox.remove(3);
    }
    let d = p.sync(Some(&cursor)).await.unwrap();
    let up: HashSet<String> = ids(&d).into_iter().collect();
    assert!(up.contains("INBOX:100:6"), "{capabilities}: new mail: {up:?}");
    assert!(up.contains("INBOX:100:1"), "{capabilities}: flag change: {up:?}");
    let one = d.upserted.iter().find(|m| m.id.as_str() == "INBOX:100:1").unwrap();
    assert!(one.flags.seen && one.flags.flagged);
    assert_eq!(d.removed, vec![MessageId::new("INBOX:100:3")], "{capabilities}");
    assert!(!up.contains("INBOX:100:2"), "{capabilities}: unchanged mail is not re-sent: {up:?}");

    // And quiet again afterwards.
    let again = p.sync(d.cursor.as_ref()).await.unwrap();
    assert!(again.upserted.is_empty() && again.removed.is_empty(), "{capabilities}: {:?} {:?}", ids(&again), again.removed);
}

#[tokio::test]
async fn changes_without_condstore_use_the_flag_window() {
    changes_are_found(BASIC).await;
}

#[tokio::test]
async fn changes_with_condstore() {
    changes_are_found(CONDSTORE).await;
}

#[tokio::test]
async fn changes_with_qresync() {
    changes_are_found(QRESYNC).await;
}

#[tokio::test]
async fn qresync_finds_expunges_without_listing_uids() {
    let server = fake_imap::serve(state(5, QRESYNC));
    let p = ImapProvider::new(account(server.addr, None));
    let (_, cursor) = sync_all(&p, None).await;
    server.lock().folder_mut("INBOX").remove(2);
    let before = server.lock().log.len();
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert_eq!(d.removed, vec![MessageId::new("INBOX:100:2")]);
    let log = server.lock().log[before..].to_vec();
    assert!(log.iter().any(|l| l.contains("VANISHED")), "{log:?}");
    assert!(!log.iter().any(|l| l.contains("SEARCH UID 1")), "no UID listing needed: {log:?}");
}

#[tokio::test]
async fn condstore_skips_quiet_folders() {
    let server = fake_imap::serve(state(3, CONDSTORE));
    let p = ImapProvider::new(account(server.addr, None));
    let (_, cursor) = sync_all(&p, None).await;
    let before = server.lock().log.len();
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert!(d.upserted.is_empty());
    let log = server.lock().log[before..].to_vec();
    assert!(!log.iter().any(|l| l.contains("EXAMINE") || l.contains("SELECT")), "STATUS was enough: {log:?}");
}

#[tokio::test]
async fn flag_changes_outside_the_window_are_not_seen() {
    let server = fake_imap::serve(state(6, BASIC));
    let mut acct = account(server.addr, None);
    acct.options.flag_window = 2;
    let p = ImapProvider::new(acct);
    let (_, cursor) = sync_all(&p, None).await;
    {
        let mut s = server.lock();
        s.folder_mut("INBOX").set_flags(6, &["\\Flagged"]);
        s.folder_mut("INBOX").set_flags(1, &["\\Flagged"]);
    }
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert_eq!(ids(&d), vec!["INBOX:100:6"], "only the newest 2 are compared");
}

#[tokio::test]
async fn uidvalidity_change_resyncs_the_folder() {
    let server = fake_imap::serve(state(3, BASIC));
    let p = ImapProvider::new(account(server.addr, None));
    let (_, cursor) = sync_all(&p, None).await;
    server.lock().folder_mut("INBOX").reset_uidvalidity(777);
    let (deltas, _) = sync_all(&p, Some(cursor)).await;
    let removed: HashSet<String> = deltas.iter().flat_map(|d| d.removed.iter().map(|r| r.to_string())).collect();
    let upserted: HashSet<String> = deltas.iter().flat_map(ids).collect();
    assert_eq!(removed, ["INBOX:100:1", "INBOX:100:2", "INBOX:100:3"].map(String::from).into());
    assert_eq!(upserted, ["INBOX:777:1", "INBOX:777:2", "INBOX:777:3"].map(String::from).into());
}

#[tokio::test]
async fn a_deleted_folder_removes_its_mail() {
    let mut st = state(1, BASIC);
    let mut work = FakeFolder::new("Work", &["\\HasNoChildren"], 900);
    work.add(msg(10, &[]));
    work.add(msg(11, &[]));
    st.folders.push(work);
    let server = fake_imap::serve(st);
    let p = ImapProvider::new(account(server.addr, None));
    let (_, cursor) = sync_all(&p, None).await;
    server.lock().folders.retain(|f| f.path != "Work");
    let d = p.sync(Some(&cursor)).await.unwrap();
    let mut removed: Vec<String> = d.removed.iter().map(|r| r.to_string()).collect();
    removed.sort();
    assert_eq!(removed, vec!["Work:900:1", "Work:900:2"]);
    assert!(!d.mailboxes.iter().flatten().any(|m| m.id.as_str() == "Work"));
}

#[tokio::test]
async fn gmail_layout_roles_and_skipped_folders() {
    let mut inbox = FakeFolder::new("INBOX", &["\\HasNoChildren"], 1);
    inbox.add(msg(1, &[]));
    let mut all = FakeFolder::new("[Gmail]/All Mail", &["\\HasNoChildren", "\\All"], 2);
    all.add(msg(1, &[]));
    let st = fake_imap::FakeState {
        capabilities: format!("{BASIC} X-GM-EXT-1"),
        folders: vec![
            inbox,
            FakeFolder::new("[Gmail]", &["\\HasChildren", "\\Noselect"], 3),
            all,
            FakeFolder::new("[Gmail]/Sent Mail", &["\\HasNoChildren", "\\Sent"], 4),
            FakeFolder::new("[Gmail]/Starred", &["\\HasNoChildren", "\\Flagged"], 5),
            FakeFolder::new("[Gmail]/Important", &["\\HasNoChildren", "\\Important"], 6),
            FakeFolder::new("[Gmail]/Spam", &["\\HasNoChildren", "\\Junk"], 7),
            FakeFolder::new("[Gmail]/Bin", &["\\HasNoChildren"], 8),
        ],
        ..Default::default()
    };
    let server = fake_imap::serve(st);
    let p = ImapProvider::new(account(server.addr, None));
    let (deltas, _) = sync_all(&p, None).await;
    let boxes: Vec<(String, String, MailboxRole)> =
        deltas[0].mailboxes.iter().flatten().map(|m| (m.id.to_string(), m.name.clone(), m.role)).collect();
    assert!(boxes.contains(&("[Gmail]/Sent Mail".into(), "Sent Mail".into(), MailboxRole::Sent)));
    assert!(boxes.contains(&("[Gmail]/Spam".into(), "Spam".into(), MailboxRole::Spam)));
    assert!(boxes.contains(&("[Gmail]/Bin".into(), "Bin".into(), MailboxRole::Trash)), "name fallback: {boxes:?}");
    for skipped in ["[Gmail]", "[Gmail]/All Mail", "[Gmail]/Starred", "[Gmail]/Important"] {
        assert!(!boxes.iter().any(|b| b.0 == skipped), "{skipped} is not synced: {boxes:?}");
    }
    // The message is reported once (from INBOX), not again from All Mail.
    let all_ids: Vec<String> = deltas.iter().flat_map(ids).collect();
    assert_eq!(all_ids, vec!["INBOX:1:1"]);
}

#[tokio::test]
async fn proton_bridge_layout_by_name() {
    let names = ["INBOX", "Drafts", "Sent", "Starred", "Archive", "Spam", "Trash", "All Mail", "Folders", "Folders/Receipts", "Labels", "Labels/Travel"];
    let st = fake_imap::FakeState {
        capabilities: OLD.into(),
        folders: names.iter().enumerate().map(|(i, n)| FakeFolder::new(n, &["\\HasNoChildren"], 10 + i as u32)).collect(),
        ..Default::default()
    };
    let server = fake_imap::serve(st);
    let p = ImapProvider::new(account(server.addr, None));
    let boxes = p.check().await.unwrap();
    let role = |id: &str| boxes.iter().find(|m| m.id.as_str() == id).map(|m| m.role);
    assert_eq!(role("Sent"), Some(MailboxRole::Sent));
    assert_eq!(role("Drafts"), Some(MailboxRole::Drafts));
    assert_eq!(role("Archive"), Some(MailboxRole::Archive));
    assert_eq!(role("Spam"), Some(MailboxRole::Spam));
    assert_eq!(role("All Mail"), Some(MailboxRole::All));
    assert_eq!(role("Starred"), Some(MailboxRole::Starred));
    assert_eq!(role("Folders/Receipts"), Some(MailboxRole::Custom));
    assert_eq!(boxes.iter().find(|m| m.id.as_str() == "Folders/Receipts").unwrap().name, "Receipts");
}

#[tokio::test]
async fn threads_come_from_references() {
    let mut st = state(0, BASIC);
    let inbox = st.folders.iter_mut().find(|f| f.path == "INBOX").unwrap();
    inbox.add(fake_imap::FakeMessage::new(&[], "10-Nov-2026 09:00:00 +0000", mail(1, "Plans", "root@example.com", None)));
    inbox.add(fake_imap::FakeMessage::new(&[], "10-Nov-2026 09:05:00 +0000", mail(2, "Re: Plans", "r1@example.com", Some("root@example.com"))));
    inbox.add(fake_imap::FakeMessage::new(&[], "10-Nov-2026 09:09:00 +0000", mail(3, "Other", "x@example.com", None)));
    let server = fake_imap::serve(st);
    let p = ImapProvider::new(account(server.addr, None));
    let (deltas, _) = sync_all(&p, None).await;
    let all: Vec<_> = deltas.iter().flat_map(|d| d.upserted.clone()).collect();
    let t = |uid: &str| all.iter().find(|m| m.id.as_str().ends_with(uid)).unwrap().thread.clone();
    assert_eq!(t(":1"), t(":2"));
    assert_ne!(t(":1"), t(":3"));
}

const WITH_ATTACHMENT: &[u8] = b"From: Ada Example <ada@example.com>\r\nTo: jason@example.com\r\nSubject: Report\r\n\
Message-ID: <att@example.com>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"zz\"\r\n\r\n\
--zz\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>See <b>attached</b>.</p>\r\n\
--zz\r\nContent-Type: text/csv; name=\"q4.csv\"\r\nContent-Disposition: attachment; filename=\"q4.csv\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\nYSxiCjEsMgo=\r\n--zz--\r\n";

#[tokio::test]
async fn bodies_raw_and_attachments() {
    let mut st = state(0, BASIC);
    st.folders[0].add(fake_imap::FakeMessage::new(&[], "10-Nov-2026 09:00:00 +0000", WITH_ATTACHMENT.to_vec()));
    let server = fake_imap::serve(st);
    let p = ImapProvider::new(account(server.addr, None));
    let d = p.sync(None).await.unwrap();
    let m = &d.upserted[0];
    assert!(m.has_attachments);
    let body = p.fetch_body(&m.id).await.unwrap();
    assert!(body.html.as_deref().unwrap().contains("<b>attached</b>"));
    assert_eq!(body.attachments.len(), 1);
    assert_eq!(body.attachments[0].filename, "q4.csv");
    assert_eq!(p.fetch_attachment(&m.id, &body.attachments[0].id).await.unwrap(), b"a,b\n1,2\n");
    assert_eq!(p.fetch_raw(&m.id).await.unwrap(), WITH_ATTACHMENT);
    assert!(matches!(p.fetch_attachment(&m.id, &AttachmentId::new("a9")).await, Err(Error::NotFound(_))));
    assert!(matches!(p.fetch_raw(&MessageId::new("INBOX:999:1")).await, Err(Error::NotFound(_))));
    // Reading does not mark anything seen.
    assert!(!server.lock().folder("INBOX").messages[&1].has_flag("\\Seen"));
}

#[tokio::test]
async fn flag_actions_are_idempotent() {
    let server = fake_imap::serve(state(3, BASIC));
    let p = ImapProvider::new(account(server.addr, None));
    let id = MessageId::new("INBOX:100:1");
    for _ in 0..2 {
        p.apply(&Action::SetSeen { ids: vec![id.clone()], seen: true }).await.unwrap();
        p.apply(&Action::SetFlagged { ids: vec![id.clone()], flagged: true }).await.unwrap();
    }
    {
        let s = server.lock();
        let m = &s.folder("INBOX").messages[&1];
        assert!(m.has_flag("\\Seen") && m.has_flag("\\Flagged"));
        assert_eq!(m.flags.len(), 2, "no duplicate flags: {:?}", m.flags);
    }
    p.apply(&Action::SetFlagged { ids: vec![id.clone()], flagged: false }).await.unwrap();
    assert!(!server.lock().folder("INBOX").messages[&1].has_flag("\\Flagged"));
    // A stale id (folder reset since) is a no-op, not an error.
    p.apply(&Action::SetSeen { ids: vec![MessageId::new("INBOX:1:1")], seen: true }).await.unwrap();
    assert!(matches!(p.apply(&Action::SetSeen { ids: vec![MessageId::new("bogus")], seen: true }).await, Err(Error::Invalid(_))));
}

async fn move_archive_trash(capabilities: &str) {
    let server = fake_imap::serve(state(4, capabilities));
    let p = ImapProvider::new(account(server.addr, None));
    let m = |uid: u32| MessageId::new(format!("INBOX:100:{uid}"));
    for _ in 0..2 {
        p.apply(&Action::Archive { ids: vec![m(1)] }).await.unwrap();
        p.apply(&Action::Trash { ids: vec![m(2)] }).await.unwrap();
        p.apply(&Action::Move { ids: vec![m(3)], from: Some(MailboxId::new("INBOX")), to: MailboxId::new("Drafts") }).await.unwrap();
    }
    {
        let s = server.lock();
        assert_eq!(s.folder("INBOX").messages.len(), 1, "{capabilities}");
        assert_eq!(s.folder("Archive").messages.len(), 1, "{capabilities}: archived once");
        assert_eq!(s.folder("Trash").messages.len(), 1, "{capabilities}: trashed once");
        assert_eq!(s.folder("Drafts").messages.len(), 1, "{capabilities}: moved once");
    }
    // Trash on something already in Trash deletes it.
    let trash_uid = *server.lock().folder("Trash").messages.keys().next().unwrap();
    p.apply(&Action::Trash { ids: vec![MessageId::new(format!("Trash:400:{trash_uid}"))] }).await.unwrap();
    assert!(server.lock().folder("Trash").messages.is_empty());
    assert!(matches!(
        p.apply(&Action::Move { ids: vec![m(4)], from: None, to: MailboxId::new("Nope") }).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn move_archive_trash_with_move() {
    move_archive_trash(BASIC).await;
}

#[tokio::test]
async fn move_archive_trash_with_copy_and_delete() {
    move_archive_trash("IMAP4rev1 SPECIAL-USE").await;
}

#[tokio::test]
async fn copy_fallback_does_not_duplicate_on_retry() {
    let server = fake_imap::serve(state(2, "IMAP4rev1 UIDPLUS SPECIAL-USE"));
    let p = ImapProvider::new(account(server.addr, None));
    // The COPY works, then the line drops before the delete.
    server.lock().drop_after_next = Some("UID COPY".into());
    let a = Action::Archive { ids: vec![MessageId::new("INBOX:100:1")] };
    assert!(matches!(p.apply(&a).await, Err(Error::Offline(_))));
    assert_eq!(server.lock().folder("Archive").messages.len(), 1);
    p.apply(&a).await.unwrap();
    let s = server.lock();
    assert_eq!(s.folder("Archive").messages.len(), 1, "still one copy");
    assert_eq!(s.folder("INBOX").messages.len(), 1);
}

#[tokio::test]
async fn archive_without_an_archive_folder_creates_one() {
    let mut st = state(1, "IMAP4rev1 UIDPLUS MOVE");
    st.folders.retain(|f| f.path != "Archive");
    let server = fake_imap::serve(st);
    let p = ImapProvider::new(account(server.addr, None));
    p.apply(&Action::Archive { ids: vec![MessageId::new("INBOX:100:1")] }).await.unwrap();
    assert_eq!(server.lock().folder("Archive").messages.len(), 1);
}

fn draft(local_id: &str) -> Draft {
    Draft {
        local_id: local_id.into(),
        to: vec![Address::new(Some("Ada Example"), "ada@example.com")],
        bcc: vec![Address::new(None, "quiet@example.org")],
        subject: "Re: Message 1".into(),
        text: "Sounds good.".into(),
        reply_to: Some(MessageId::new("INBOX:100:1")),
        in_reply_to: Some("m1@example.com".into()),
        references: vec!["m1@example.com".into()],
        ..Default::default()
    }
}

#[tokio::test]
async fn drafts_are_replaced_not_duplicated() {
    let server = fake_imap::serve(state(1, BASIC));
    let p = ImapProvider::new(account(server.addr, None));
    let mut d = draft("draft-1");
    let first = p.apply(&Action::SaveDraft { draft: d.clone() }).await.unwrap();
    d.text = "Sounds good, see you.".into();
    p.apply(&Action::SaveDraft { draft: d.clone() }).await.unwrap();
    let last = p.apply(&Action::SaveDraft { draft: d.clone() }).await.unwrap();
    {
        let s = server.lock();
        let drafts = s.folder("Drafts");
        assert_eq!(drafts.messages.len(), 1);
        let (uid, m) = drafts.messages.iter().next().unwrap();
        // The outcome names the copy that is on the server now.
        assert_eq!(last.server_id, Some(format!("Drafts:300:{uid}")));
        assert_ne!(first.server_id, last.server_id);
        assert!(m.has_flag("\\Draft"));
        assert!(String::from_utf8_lossy(&m.raw).contains("see you"));
        assert!(String::from_utf8_lossy(&m.raw).contains("quiet@example.org"), "drafts keep Bcc");
    }
    p.apply(&Action::SaveDraft { draft: draft("draft-2") }).await.unwrap();
    assert_eq!(server.lock().folder("Drafts").messages.len(), 2);
    p.apply(&Action::DeleteDraft { local_id: "draft-1".into(), server_id: None }).await.unwrap();
    p.apply(&Action::DeleteDraft { local_id: "draft-1".into(), server_id: None }).await.unwrap();
    assert_eq!(server.lock().folder("Drafts").messages.len(), 1);
}

#[tokio::test]
async fn send_files_a_copy_and_never_sends_twice() {
    let imap = fake_imap::serve(state(1, BASIC));
    let smtp = fake_smtp::serve(SmtpState::default());
    let p = ImapProvider::new(account(imap.addr, Some(smtp.addr)));
    let d = draft("send-1");
    p.apply(&Action::SaveDraft { draft: d.clone() }).await.unwrap();
    p.apply(&Action::Send { draft: d.clone() }).await.unwrap();
    p.apply(&Action::Send { draft: d.clone() }).await.unwrap();
    let delivered = smtp.lock().delivered.clone();
    assert_eq!(delivered.len(), 1, "sent once");
    let wire = String::from_utf8_lossy(&delivered[0].data).to_string();
    assert!(!wire.contains("quiet@example.org"), "no Bcc header on the wire");
    assert!(wire.contains("In-Reply-To: <m1@example.com>"));
    let mut rcpt = delivered[0].to.clone();
    rcpt.sort();
    assert_eq!(rcpt, vec!["ada@example.com", "quiet@example.org"]);
    assert_eq!(delivered[0].from, "jason@example.com");
    let s = imap.lock();
    assert_eq!(s.folder("Sent").messages.len(), 1, "one copy in Sent");
    let copy = s.folder("Sent").messages.values().next().unwrap();
    assert!(copy.has_flag("\\Seen"));
    assert!(String::from_utf8_lossy(&copy.raw).contains("quiet@example.org"), "the Sent copy keeps Bcc");
    assert!(s.folder("Drafts").messages.is_empty(), "the draft is gone");
    assert!(s.folder("INBOX").messages[&1].has_flag("\\Answered"));
    assert!(smtp.lock().log.iter().all(|l| !l.contains("hunter2")));
}

#[tokio::test]
async fn send_retry_after_a_lost_reply_checks_sent_first() {
    // Gmail-like: the server files sent mail itself.
    let imap = fake_imap::serve(state(1, &format!("{BASIC} X-GM-EXT-1")));
    let smtp = fake_smtp::serve(SmtpState { file_into: Some((imap.state.clone(), "Sent".into())), drop_after_data: true, ..Default::default() });
    let p = ImapProvider::new(account(imap.addr, Some(smtp.addr)));
    let d = draft("send-2");
    let first = p.apply(&Action::Send { draft: d.clone() }).await;
    assert!(matches!(first, Err(Error::Offline(_))), "{first:?}");
    assert_eq!(smtp.lock().delivered.len(), 1);
    p.apply(&Action::Send { draft: d.clone() }).await.unwrap();
    assert_eq!(smtp.lock().delivered.len(), 1, "the retry found it in Sent and did not send again");
    assert_eq!(imap.lock().folder("Sent").messages.len(), 1, "no extra copy: the server files it");
}

#[tokio::test]
async fn send_retry_after_a_temporary_refusal() {
    let imap = fake_imap::serve(state(1, BASIC));
    let smtp = fake_smtp::serve(SmtpState { fail_rcpt: Some("451 4.3.0 Try again later".into()), ..Default::default() });
    let p = ImapProvider::new(account(imap.addr, Some(smtp.addr)));
    let d = draft("send-3");
    assert!(matches!(p.apply(&Action::Send { draft: d.clone() }).await, Err(Error::Offline(_))));
    assert!(smtp.lock().delivered.is_empty());
    assert!(imap.lock().folder("Sent").messages.is_empty());
    p.apply(&Action::Send { draft: d }).await.unwrap();
    assert_eq!(smtp.lock().delivered.len(), 1);
    assert_eq!(imap.lock().folder("Sent").messages.len(), 1);
}

#[tokio::test]
async fn send_needs_smtp_and_recipients() {
    let imap = fake_imap::serve(state(0, BASIC));
    let p = ImapProvider::new(account(imap.addr, None));
    assert!(matches!(p.apply(&Action::Send { draft: draft("x") }).await, Err(Error::Unsupported(_))));
    let smtp = fake_smtp::serve(SmtpState::default());
    let p = ImapProvider::new(account(imap.addr, Some(smtp.addr)));
    let empty = Draft { local_id: "y".into(), text: "hi".into(), ..Default::default() };
    assert!(matches!(p.apply(&Action::Send { draft: empty }).await, Err(Error::Invalid(_))));
    assert!(smtp.lock().delivered.is_empty());
}

#[tokio::test]
async fn oauth_sign_in_for_imap_and_smtp() {
    let imap = fake_imap::serve(state(1, BASIC));
    let smtp = fake_smtp::serve(SmtpState::default());
    let mut acct = account(imap.addr, Some(smtp.addr));
    acct.credentials = Credentials::OAuth2 { username: "jason@example.com".into(), token: "made-up-oauth-token".into() };
    let p = ImapProvider::new(acct);
    assert_eq!(p.sync(None).await.unwrap().upserted.len(), 1);
    p.apply(&Action::Send { draft: draft("oauth-1") }).await.unwrap();
    assert_eq!(smtp.lock().delivered.len(), 1);
    assert!(imap.lock().log.iter().any(|l| l.contains("AUTHENTICATE XOAUTH2")));
}

#[tokio::test]
async fn bad_credentials_are_auth_errors_without_secrets() {
    let imap = fake_imap::serve(state(1, BASIC));
    let mut acct = account(imap.addr, None);
    acct.credentials = Credentials::Password { username: "jason@example.com".into(), password: "wrong-secret-pw".into() };
    let p = ImapProvider::new(acct);
    let e = p.sync(None).await.unwrap_err();
    assert!(matches!(e, Error::Auth(_)), "{e:?}");
    assert!(!e.to_string().contains("wrong-secret-pw"));
    assert!(!format!("{p:?}").contains("wrong-secret-pw"));

    // A refreshed token is used on the next connection.
    p.set_credentials(Credentials::OAuth2 { username: "jason@example.com".into(), token: "stale".into() }, None);
    assert!(matches!(p.sync(None).await, Err(Error::Auth(_))));
    p.set_credentials(Credentials::OAuth2 { username: "jason@example.com".into(), token: "made-up-oauth-token".into() }, None);
    assert!(p.sync(None).await.is_ok());

    let unavailable = fake_imap::serve(fake_imap::FakeState { login: Login::Bye, ..state(0, BASIC) });
    assert!(matches!(ImapProvider::new(account(unavailable.addr, None)).sync(None).await, Err(Error::Offline(_))));
}

#[tokio::test]
async fn plaintext_to_a_remote_host_is_refused_before_connecting() {
    let mut acct = account("127.0.0.1:1".parse().unwrap(), None);
    acct.imap = Server::new("imap.example.com", 143, Security::Plain);
    let p = ImapProvider::new(acct);
    assert!(matches!(p.sync(None).await, Err(Error::Refused(_))));
}

#[tokio::test]
async fn a_dropped_connection_is_offline_and_recovers() {
    let server = fake_imap::serve(state(2, BASIC));
    let p = ImapProvider::new(account(server.addr, None));
    let (_, cursor) = sync_all(&p, None).await;
    server.lock().drop_next_command = true;
    assert!(matches!(p.sync(Some(&cursor)).await, Err(Error::Offline(_))));
    assert!(p.sync(Some(&cursor)).await.is_ok());
    assert!(server.lock().connections >= 2);
}

#[tokio::test]
async fn idle_wakes_on_new_mail() {
    let server = fake_imap::serve(state(1, BASIC));
    let p = Arc::new(ImapProvider::new(account(server.addr, None)));
    // First wait opens the IDLE connection and returns on the timeout.
    let start = Instant::now();
    p.wait_for_change(1).await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(900));
    let waiter = {
        let p = p.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            p.wait_for_change(30).await.unwrap();
            start.elapsed()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    server.lock().folder_mut("INBOX").add(msg(2, &[]));
    let took = tokio::time::timeout(Duration::from_secs(10), waiter).await.unwrap().unwrap();
    assert!(took < Duration::from_secs(10), "woke on EXISTS after {took:?}");
    assert!(server.lock().log.iter().any(|l| l.ends_with("IDLE")));
}

#[tokio::test]
async fn without_idle_wait_just_sleeps() {
    let server = fake_imap::serve(state(1, OLD));
    let p = ImapProvider::new(account(server.addr, None));
    let start = Instant::now();
    p.wait_for_change(1).await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert!(!server.lock().log.iter().any(|l| l.ends_with("IDLE")));
}

#[tokio::test]
async fn old_servers_without_special_use_or_uidplus() {
    let mut st = state(3, OLD);
    for f in &mut st.folders {
        f.attributes = vec!["\\HasNoChildren".into()];
    }
    let server = fake_imap::serve(st);
    let p = ImapProvider::new(account(server.addr, None));
    let (deltas, cursor) = sync_all(&p, None).await;
    assert_eq!(deltas.iter().map(|d| d.upserted.len()).sum::<usize>(), 3);
    let roles: Vec<MailboxRole> = deltas[0].mailboxes.iter().flatten().map(|m| m.role).collect();
    assert!(roles.contains(&MailboxRole::Sent) && roles.contains(&MailboxRole::Trash));
    p.apply(&Action::Trash { ids: vec![MessageId::new("INBOX:100:1")] }).await.unwrap();
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert_eq!(d.removed, vec![MessageId::new("INBOX:100:1")]);
    assert_eq!(ids(&d), vec!["Trash:400:1"]);
}

#[tokio::test]
async fn a_failed_command_never_looks_like_an_empty_mailbox() {
    let server = fake_imap::serve(state(5, CONDSTORE));
    let p = ImapProvider::new(account(server.addr, None));
    let (_, cursor) = sync_all(&p, None).await;
    server.lock().folder_mut("INBOX").remove(3);
    // The expunge check's search fails: the sync fails, nothing is reported removed.
    server.lock().fail_next = Some(("SEARCH UID 1".into(), "NO [SERVERBUG] Internal error".into()));
    assert!(p.sync(Some(&cursor)).await.is_err());
    // The connection drops in the middle of LIST: offline, not "every folder is gone".
    server.lock().drop_after_next = Some("LIST".into());
    assert!(matches!(p.sync(Some(&cursor)).await, Err(Error::Offline(_))));
    let d = p.sync(Some(&cursor)).await.unwrap();
    assert_eq!(d.removed, vec![MessageId::new("INBOX:100:3")]);
}

#[tokio::test]
async fn move_without_from_adds_and_removes_nothing() {
    for caps in [BASIC, "IMAP4rev1 SPECIAL-USE"] {
        let server = fake_imap::serve(state(2, caps));
        let p = ImapProvider::new(account(server.addr, None));
        let id = MessageId::new("INBOX:100:1");
        for _ in 0..2 {
            p.apply(&Action::Move { ids: vec![id.clone()], from: None, to: MailboxId::new("Archive") }).await.unwrap();
        }
        let s = server.lock();
        assert_eq!(s.folder("INBOX").messages.len(), 2, "{caps}: nothing removed");
        assert_eq!(s.folder("Archive").messages.len(), 1, "{caps}: copied once");
    }
}

#[tokio::test]
async fn the_first_sync_is_a_reset_and_bad_cursors_ask_for_a_resync() {
    let server = fake_imap::serve(state(5, BASIC));
    let mut acct = account(server.addr, None);
    acct.options.page_size = 2;
    let p = ImapProvider::new(acct);
    let (deltas, cursor) = sync_all(&p, None).await;
    assert!(deltas.len() > 1);
    assert!(deltas.iter().all(|d| d.reset), "every page of the first sync is a reset page");
    let later = p.sync(Some(&cursor)).await.unwrap();
    assert!(!later.reset);
    assert!(matches!(p.sync(Some(&SyncCursor("{\"k\":\"hist\"}".into()))).await, Err(Error::ResyncNeeded(_))));
}
