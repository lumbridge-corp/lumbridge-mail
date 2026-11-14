//! A signed-in IMAP connection with the commands the provider uses, each with a timeout and our error
//! mapping, plus what the server can do.

use crate::config::{Credentials, Server};
use crate::conn::{self, Session, imap_err, with_timeout};
use crate::folders::Folder;
use crate::parse::FetchInfo;
use crate::uidset::UidSet;
use async_imap::imap_proto::{
    AttributeValue, MailboxDatum, MessageSection, NameAttribute, Response, ResponseCode, SectionPath, Status, StatusAttribute,
};
use async_imap::types::UnsolicitedResponse;
use lumbridge_mail_core::{Error, Flags, Result};
use std::ops::RangeInclusive;
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub condstore: bool,
    pub qresync: bool,
    pub mv: bool,
    pub uidplus: bool,
    pub idle: bool,
    /// Gmail's extensions (`X-GM-EXT-1`): Gmail also files sent mail itself.
    pub gmail: bool,
}

/// What STATUS (or SELECT) told us about a folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderStatus {
    pub exists: u32,
    pub unseen: u32,
    pub uidnext: u32,
    pub validity: u32,
    pub modseq: Option<u64>,
}

pub struct Conn {
    pub session: Session,
    pub caps: Caps,
    /// The last LIST, for actions that need a folder by role.
    pub folders: Option<Vec<Folder>>,
    timeout: Duration,
}

/// One FETCH response, owned.
#[derive(Debug, Clone, Default)]
pub struct FetchItem {
    pub uid: Option<u32>,
    pub flags: Flags,
    pub internal_date: Option<i64>,
    pub size: Option<u32>,
    /// `BODY[]` (or a partial of it).
    pub body: Option<Vec<u8>>,
    /// `BODY[HEADER]`.
    pub header: Option<Vec<u8>>,
    pub modseq: Option<u64>,
}

impl FetchItem {
    fn from_attrs(attrs: &[AttributeValue<'_>]) -> Self {
        let mut f = FetchItem::default();
        for a in attrs {
            match a {
                AttributeValue::Uid(u) => f.uid = Some(*u),
                AttributeValue::Rfc822Size(n) => f.size = Some(*n),
                AttributeValue::ModSeq(m) => f.modseq = Some(*m),
                AttributeValue::InternalDate(d) => {
                    f.internal_date = chrono::DateTime::parse_from_str(d, "%d-%b-%Y %H:%M:%S %z").ok().map(|d| d.timestamp())
                }
                AttributeValue::Flags(flags) => {
                    for flag in flags {
                        match flag.to_ascii_lowercase().as_str() {
                            "\\seen" => f.flags.seen = true,
                            "\\flagged" => f.flags.flagged = true,
                            "\\answered" => f.flags.answered = true,
                            "\\draft" => f.flags.draft = true,
                            _ => {}
                        }
                    }
                }
                AttributeValue::BodySection { section: None, data: Some(d), .. } | AttributeValue::Rfc822(Some(d)) => {
                    f.body = Some(d.to_vec())
                }
                AttributeValue::BodySection { section: Some(SectionPath::Full(MessageSection::Header)), data: Some(d), .. }
                | AttributeValue::Rfc822Header(Some(d)) => f.header = Some(d.to_vec()),
                _ => {}
            }
        }
        f
    }

    pub fn info(&self) -> FetchInfo {
        FetchInfo { flags: self.flags, internal_date: self.internal_date, size: self.size }
    }
}

/// The untagged data of one command we care about.
enum Resp {
    Fetch(FetchItem),
    Search(Vec<u32>),
    List(Folder),
    Vanished(Vec<RangeInclusive<u32>>),
    Exists(u32),
    UidValidity(u32),
    UidNext(u32),
    HighestModSeq(u64),
    Status(FolderStatus),
}

fn status_of(attrs: &[StatusAttribute]) -> FolderStatus {
    let mut st = FolderStatus::default();
    for a in attrs {
        match a {
            StatusAttribute::Messages(n) => st.exists = *n,
            StatusAttribute::Unseen(n) => st.unseen = *n,
            StatusAttribute::UidNext(n) => st.uidnext = *n,
            StatusAttribute::UidValidity(n) => st.validity = *n,
            StatusAttribute::HighestModSeq(n) => st.modseq = Some(*n),
            _ => {}
        }
    }
    st
}

fn attr_name(a: &NameAttribute<'_>) -> String {
    match a {
        NameAttribute::NoInferiors => "noinferiors".to_owned(),
        NameAttribute::NoSelect => "noselect".to_owned(),
        NameAttribute::Marked => "marked".to_owned(),
        NameAttribute::Unmarked => "unmarked".to_owned(),
        NameAttribute::All => "all".to_owned(),
        NameAttribute::Archive => "archive".to_owned(),
        NameAttribute::Drafts => "drafts".to_owned(),
        NameAttribute::Flagged => "flagged".to_owned(),
        NameAttribute::Junk => "junk".to_owned(),
        NameAttribute::Sent => "sent".to_owned(),
        NameAttribute::Trash => "trash".to_owned(),
        NameAttribute::Extension(s) => s.trim_start_matches('\\').to_ascii_lowercase(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

/// A quoted IMAP string, or None if the value cannot be sent safely as one.
pub fn quoted(s: &str) -> Option<String> {
    if s.chars().any(|c| c.is_control() || !c.is_ascii()) {
        return None;
    }
    Some(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
}

impl Conn {
    pub async fn open(server: &Server, creds: &Credentials, timeout: Duration) -> Result<Self> {
        let mut session = conn::open(server, creds, timeout).await?;
        let caps = with_timeout(timeout, "CAPABILITY", async {
            session.capabilities().await.map_err(|e| imap_err("CAPABILITY", e))
        })
        .await?;
        let has = |c: &str| caps.has_str(c);
        let mut out = Caps {
            condstore: has("CONDSTORE") || has("QRESYNC"),
            qresync: false,
            mv: has("MOVE"),
            uidplus: has("UIDPLUS"),
            idle: has("IDLE"),
            gmail: has("X-GM-EXT-1"),
        };
        if has("QRESYNC") && has("ENABLE") {
            let ok = with_timeout(timeout, "ENABLE", async {
                session.run_command_and_check_ok("ENABLE QRESYNC").await.map_err(|e| imap_err("ENABLE", e))
            })
            .await;
            out.qresync = ok.is_ok();
        }
        let mut c = Self { session, caps: out, folders: None, timeout };
        c.drain();
        Ok(c)
    }

    pub fn into_parts(self) -> (Session, Caps, Option<Vec<Folder>>) {
        (self.session, self.caps, self.folders)
    }

    pub fn from_parts(session: Session, caps: Caps, folders: Option<Vec<Folder>>, timeout: Duration) -> Self {
        Self { session, caps, folders, timeout }
    }

    /// Throws away queued unsolicited responses, returning them.
    pub fn drain(&mut self) -> Vec<UnsolicitedResponse> {
        let mut out = Vec::new();
        while let Ok(r) = self.session.unsolicited_responses.try_recv() {
            out.push(r);
        }
        out
    }

    /// Runs one command and reads up to its tagged answer. Unlike async-imap's streams, a connection that
    /// closes before the answer, or a NO / BAD answer, is an error, never a short result.
    async fn run(&mut self, what: &str, command: String) -> Result<Vec<Resp>> {
        let t = self.timeout;
        with_timeout(t, what, async {
            let tag = self.session.run_command(&command).await.map_err(|e| imap_err(what, e))?;
            let mut out = Vec::new();
            loop {
                let Some(resp) = self.session.read_response().await.map_err(|e| Error::Offline(format!("{what}: {e}")))? else {
                    return Err(Error::Offline(format!("{what}: connection closed")));
                };
                match resp.parsed() {
                    Response::Done { tag: done, status, outcome } if *done == tag => {
                        let info = outcome.information.as_deref().unwrap_or("").chars().take(200).collect::<String>();
                        return match status {
                            Status::Ok => Ok(out),
                            Status::No => Err(Error::Other(format!("{what}: server said NO: {info}"))),
                            _ => Err(Error::Other(format!("{what}: server said BAD: {info}"))),
                        };
                    }
                    Response::Data { status: Status::Bye, .. } => {
                        return Err(Error::Offline(format!("{what}: server closed the session")));
                    }
                    Response::Fetch(_, attrs) => out.push(Resp::Fetch(FetchItem::from_attrs(attrs))),
                    Response::MailboxData(MailboxDatum::Search(ids)) => out.push(Resp::Search(ids.clone())),
                    Response::MailboxData(MailboxDatum::List(l)) => out.push(Resp::List(Folder::new(
                        &l.name,
                        l.delimiter.as_deref(),
                        l.name_attributes.iter().map(attr_name).collect(),
                    ))),
                    Response::Vanished { uids, .. } => out.push(Resp::Vanished(uids.clone())),
                    Response::MailboxData(MailboxDatum::Exists(n)) => out.push(Resp::Exists(*n)),
                    Response::MailboxData(MailboxDatum::Status { status, .. }) => out.push(Resp::Status(status_of(status))),
                    Response::Data { status: Status::Ok, outcome } => match &outcome.code {
                        Some(ResponseCode::UidValidity(n)) => out.push(Resp::UidValidity(*n)),
                        Some(ResponseCode::UidNext(n)) => out.push(Resp::UidNext(*n)),
                        Some(ResponseCode::HighestModSeq(n)) => out.push(Resp::HighestModSeq(*n)),
                        _ => {}
                    },
                    _ => {}
                }
            }
        })
        .await
    }

    pub async fn list(&mut self) -> Result<Vec<Folder>> {
        let folders: Vec<Folder> = self
            .run("LIST", "LIST \"\" \"*\"".into())
            .await?
            .into_iter()
            .filter_map(|r| match r {
                Resp::List(f) => Some(f),
                _ => None,
            })
            .collect();
        self.folders = Some(folders.clone());
        Ok(folders)
    }

    pub async fn folders(&mut self) -> Result<Vec<Folder>> {
        match &self.folders {
            Some(f) => Ok(f.clone()),
            None => self.list().await,
        }
    }

    fn quoted_name(folder: &str) -> Result<String> {
        if folder.contains(['\r', '\n', '\0']) {
            return Err(Error::Invalid(format!("bad folder name {folder:?}")));
        }
        Ok(format!("\"{}\"", folder.replace('\\', "\\\\").replace('"', "\\\"")))
    }

    pub async fn status(&mut self, folder: &str) -> Result<FolderStatus> {
        let items = if self.caps.condstore {
            "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)"
        } else {
            "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY)"
        };
        let name = Self::quoted_name(folder)?;
        let mut st = self
            .run("STATUS", format!("STATUS {name} {items}"))
            .await?
            .into_iter()
            .find_map(|r| match r {
                Resp::Status(st) => Some(st),
                _ => None,
            })
            .ok_or_else(|| Error::Other(format!("STATUS {folder}: no answer")))?;
        if st.uidnext == 0 || st.validity == 0 {
            // Some servers leave items out of STATUS; SELECT always has them (or we compute UIDNEXT).
            let sel = self.examine(folder).await?;
            st.validity = sel.validity;
            st.uidnext = sel.uidnext;
            st.exists = sel.exists;
            st.modseq = st.modseq.or(sel.modseq);
        }
        Ok(st)
    }

    async fn open_folder(&mut self, folder: &str, write: bool) -> Result<FolderStatus> {
        let name = Self::quoted_name(folder)?;
        let verb = if write { "SELECT" } else { "EXAMINE" };
        let resps = match self.run(verb, format!("{verb} {name}")).await {
            Ok(r) => r,
            Err(Error::Other(m)) if m.contains("said NO") => return Err(Error::NotFound(format!("folder {folder}"))),
            Err(e) => return Err(e),
        };
        let mut st = FolderStatus::default();
        for r in resps {
            match r {
                Resp::Exists(n) => st.exists = n,
                Resp::UidValidity(n) => st.validity = n,
                Resp::UidNext(n) => st.uidnext = n,
                Resp::HighestModSeq(n) => st.modseq = Some(n),
                _ => {}
            }
        }
        if st.validity == 0 {
            return Err(Error::Other(format!("{verb} {folder}: no UIDVALIDITY")));
        }
        if st.uidnext == 0 && st.exists > 0 {
            let all = self.search("ALL").await?;
            st.uidnext = all.last().map(|u| u + 1).unwrap_or(1);
        } else if st.uidnext == 0 {
            st.uidnext = 1;
        }
        Ok(st)
    }

    /// Read-only selection (does not touch \Recent or anything else).
    pub async fn examine(&mut self, folder: &str) -> Result<FolderStatus> {
        self.open_folder(folder, false).await
    }

    /// Read-write selection; reuses the current one when it already is.
    pub async fn select(&mut self, folder: &str) -> Result<FolderStatus> {
        self.open_folder(folder, true).await
    }

    /// `UID FETCH <set> <items>`, all results.
    pub async fn fetch(&mut self, set: &str, items: &str) -> Result<Vec<FetchItem>> {
        Ok(self.fetch_vanished(set, items).await?.0)
    }

    /// `UID FETCH`, plus the UIDs a `VANISHED (EARLIER)` answer listed (QRESYNC).
    pub async fn fetch_vanished(&mut self, set: &str, items: &str) -> Result<(Vec<FetchItem>, Vec<u32>)> {
        if set.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut fetched = Vec::new();
        let mut vanished = Vec::new();
        for r in self.run("FETCH", format!("UID FETCH {set} {items}")).await? {
            match r {
                Resp::Fetch(f) => fetched.push(f),
                Resp::Vanished(ranges) => vanished.extend(ranges.into_iter().flatten()),
                _ => {}
            }
        }
        Ok((fetched, vanished))
    }

    /// `UID SEARCH <query>`, sorted ascending.
    pub async fn search(&mut self, query: &str) -> Result<Vec<u32>> {
        let mut v: Vec<u32> = self
            .run("SEARCH", format!("UID SEARCH {query}"))
            .await?
            .into_iter()
            .flat_map(|r| match r {
                Resp::Search(ids) => ids,
                _ => Vec::new(),
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        Ok(v)
    }

    /// UIDs of messages in the selected folder with this Message-ID.
    pub async fn find_message_id(&mut self, message_id: &str) -> Result<Vec<u32>> {
        let Some(q) = quoted(&format!("<{}>", message_id.trim_start_matches('<').trim_end_matches('>'))) else {
            return Ok(Vec::new());
        };
        self.search(&format!("HEADER Message-ID {q}")).await
    }

    /// `UID STORE <set> <item> <flags>`.
    pub async fn store(&mut self, set: &UidSet, item: &str) -> Result<()> {
        if set.is_empty() {
            return Ok(());
        }
        self.run("STORE", format!("UID STORE {set} {item}")).await.map(|_| ())
    }

    pub async fn copy(&mut self, set: &UidSet, to: &str) -> Result<()> {
        if set.is_empty() {
            return Ok(());
        }
        let t = self.timeout;
        let s = set.to_imap();
        with_timeout(t, "COPY", async { self.session.uid_copy(&s, to).await.map_err(|e| imap_err("COPY", e)) }).await
    }

    pub async fn mv(&mut self, set: &UidSet, to: &str) -> Result<()> {
        if set.is_empty() {
            return Ok(());
        }
        let t = self.timeout;
        let s = set.to_imap();
        with_timeout(t, "MOVE", async { self.session.uid_mv(&s, to).await.map_err(|e| imap_err("MOVE", e)) }).await
    }

    /// Removes the given messages of the selected folder for good: \Deleted, then UID EXPUNGE (UIDPLUS) or
    /// EXPUNGE.
    pub async fn delete_uids(&mut self, set: &UidSet) -> Result<()> {
        if set.is_empty() {
            return Ok(());
        }
        self.store(set, "+FLAGS.SILENT (\\Deleted)").await?;
        let command = if self.caps.uidplus { format!("UID EXPUNGE {set}") } else { "EXPUNGE".to_owned() };
        self.run("EXPUNGE", command).await.map(|_| ())
    }

    pub async fn append(&mut self, folder: &str, flags: &str, bytes: &[u8]) -> Result<()> {
        let t = self.timeout;
        with_timeout(t, "APPEND", async {
            self.session.append(folder, Some(flags), None, bytes).await.map_err(|e| imap_err("APPEND", e))
        })
        .await
    }

    /// CREATE, treating "already exists" as success. Refreshes the folder list.
    pub async fn create(&mut self, folder: &str) -> Result<()> {
        let t = self.timeout;
        let r = with_timeout(t, "CREATE", async { self.session.create(folder).await.map_err(|e| imap_err("CREATE", e)) })
            .await;
        match r {
            Ok(()) => {}
            Err(Error::Other(m)) if m.contains("NO") => {}
            Err(e) => return Err(e),
        }
        self.list().await?;
        Ok(())
    }
}
