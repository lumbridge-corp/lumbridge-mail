#![allow(dead_code)]

pub mod fake_imap;
pub mod fake_smtp;

use fake_imap::{FakeFolder, FakeMessage, FakeState};
use lumbridge_mail_core::AccountId;
use lumbridge_mail_imap::{Credentials, Identity, ImapAccount, Security, Server};
use std::net::SocketAddr;
use std::time::Duration;

/// A made-up message. `n` varies the date and body.
pub fn mail(n: u32, subject: &str, msgid: &str, in_reply_to: Option<&str>) -> Vec<u8> {
    let threading = match in_reply_to {
        Some(p) => format!("In-Reply-To: <{p}>\r\nReferences: <{p}>\r\n"),
        None => String::new(),
    };
    format!(
        "From: Ada Example <ada@example.com>\r\nTo: Jason Test <jason@example.com>\r\nSubject: {subject}\r\n\
         Message-ID: <{msgid}>\r\n{threading}Date: Tue, 10 Nov 2026 09:{:02}:00 +0000\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nBody number {n}.\r\n",
        n % 60
    )
    .into_bytes()
}

pub fn msg(n: u32, flags: &[&str]) -> FakeMessage {
    FakeMessage::new(flags, &format!("10-Nov-2026 09:{:02}:00 +0000", n % 60), mail(n, &format!("Message {n}"), &format!("m{n}@example.com"), None))
}

/// INBOX with `n` messages plus the usual special-use folders.
pub fn state(n: u32, capabilities: &str) -> FakeState {
    let mut inbox = FakeFolder::new("INBOX", &["\\HasNoChildren"], 100);
    for i in 1..=n {
        inbox.add(msg(i, if i % 2 == 0 { &["\\Seen"] } else { &[] }));
    }
    FakeState {
        capabilities: capabilities.into(),
        folders: vec![
            inbox,
            FakeFolder::new("Sent", &["\\HasNoChildren", "\\Sent"], 200),
            FakeFolder::new("Drafts", &["\\HasNoChildren", "\\Drafts"], 300),
            FakeFolder::new("Trash", &["\\HasNoChildren", "\\Trash"], 400),
            FakeFolder::new("Archive", &["\\HasNoChildren", "\\Archive"], 500),
        ],
        ..Default::default()
    }
}

pub const BASIC: &str = "IMAP4rev1 UIDPLUS MOVE IDLE SPECIAL-USE ENABLE";
pub const CONDSTORE: &str = "IMAP4rev1 UIDPLUS MOVE IDLE SPECIAL-USE ENABLE CONDSTORE";
pub const QRESYNC: &str = "IMAP4rev1 UIDPLUS MOVE IDLE SPECIAL-USE ENABLE CONDSTORE QRESYNC";
/// An old server: no MOVE, no UIDPLUS, no IDLE, no SPECIAL-USE.
pub const OLD: &str = "IMAP4rev1";

pub fn account(imap: SocketAddr, smtp: Option<SocketAddr>) -> ImapAccount {
    let mut a = ImapAccount::new(
        AccountId::new("test-account"),
        Identity { email: "jason@example.com".into(), name: "Jason Test".into() },
        Server::new("127.0.0.1", imap.port(), Security::Plain),
        smtp.map(|s| Server::new("127.0.0.1", s.port(), Security::Plain)),
        Credentials::Password { username: "jason@example.com".into(), password: "hunter2".into() },
    );
    a.options.timeout = Duration::from_secs(10);
    a
}
