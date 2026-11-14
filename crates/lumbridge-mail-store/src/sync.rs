//! Applying a provider [`Delta`], keeping the thread rows and mailbox counts derived from messages, and
//! reading messages back.

use crate::accounts::acct_pk;
use crate::{Db, Store, from_json, now, role_str, to_json};
use lumbridge_mail_core::{
    AccountId, Address, Delta, Error, Flags, Mailbox, MailboxId, MailboxRole, MessageId, MessageMeta, Result,
    ThreadId,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeSet, HashSet};

/// What [`Store::apply_delta`] changed, for the daemon's events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Threads added, changed or emptied (`Event::Changed`).
    pub changed_threads: Vec<ThreadId>,
    /// Messages the store had not seen before (candidates for `Event::NewMail` and AI triage).
    pub new_messages: Vec<MessageId>,
    /// How many of `Delta::removed` were in the store.
    pub removed: u32,
}

/// Thread rows and mailbox counts to recompute at the end of a transaction.
#[derive(Default)]
pub(crate) struct Touched {
    pub threads: BTreeSet<i64>,
    pub mboxes: BTreeSet<i64>,
    /// Words newly indexed, for the typo-tolerance vocabulary.
    pub terms: BTreeSet<String>,
}

impl Touched {
    /// Recomputes everything touched and returns the provider ids of the touched threads.
    pub fn finish(self, conn: &Connection) -> Result<Vec<ThreadId>> {
        save_terms(conn, &self.terms)?;
        let mut ids = Vec::with_capacity(self.threads.len());
        for tid in self.threads {
            if let Some(id) = refresh_thread(conn, tid)? {
                ids.push(id);
            }
        }
        let mut st = conn
            .prepare_cached(
                "UPDATE mailboxes SET
                   total=(SELECT count(*) FROM message_mailboxes WHERE mbox=?1),
                   unread=(SELECT count(*) FROM message_mailboxes WHERE mbox=?1 AND seen=0)
                 WHERE pk=?1",
            )
            .db()?;
        for mbox in self.mboxes {
            st.execute([mbox]).db()?;
        }
        Ok(ids)
    }
}

/// Well-known Gmail label ids, so a label seen on a message before the mailbox list arrives still gets
/// the right role.
fn guess_role(id: &str) -> MailboxRole {
    match id {
        "INBOX" => MailboxRole::Inbox,
        "SENT" => MailboxRole::Sent,
        "DRAFT" | "DRAFTS" => MailboxRole::Drafts,
        "TRASH" => MailboxRole::Trash,
        "SPAM" => MailboxRole::Spam,
        "STARRED" => MailboxRole::Starred,
        "IMPORTANT" => MailboxRole::Important,
        _ => MailboxRole::Custom,
    }
}

pub(crate) fn mailbox_pk(conn: &Connection, acct: i64, id: &MailboxId) -> Result<Option<i64>> {
    conn.prepare_cached("SELECT pk FROM mailboxes WHERE acct=?1 AND id=?2")
        .db()?
        .query_row(params![acct, id.as_str()], |r| r.get(0))
        .optional()
        .db()
}

/// The mailbox's pk, creating a placeholder (name = id) if a message names a mailbox we have not been told about.
fn ensure_mailbox(conn: &Connection, acct: i64, id: &MailboxId) -> Result<i64> {
    if let Some(pk) = mailbox_pk(conn, acct, id)? {
        return Ok(pk);
    }
    conn.prepare_cached("INSERT INTO mailboxes(acct, id, name, role) VALUES (?1, ?2, ?2, ?3)")
        .db()?
        .execute(params![acct, id.as_str(), role_str(guess_role(id.as_str()))])
        .db()?;
    Ok(conn.last_insert_rowid())
}

/// Pks of the account's mailboxes with `role`.
pub(crate) fn mailboxes_with_role(conn: &Connection, acct: i64, role: MailboxRole) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached("SELECT pk FROM mailboxes WHERE acct=?1 AND role=?2 ORDER BY pk").db()?;
    st.query_map(params![acct, role_str(role)], |r| r.get(0)).db()?.collect::<rusqlite::Result<_>>().db()
}

pub(crate) fn ensure_thread(conn: &Connection, acct: i64, id: &ThreadId) -> Result<i64> {
    let found: Option<i64> = conn
        .prepare_cached("SELECT pk FROM threads WHERE acct=?1 AND id=?2")
        .db()?
        .query_row(params![acct, id.as_str()], |r| r.get(0))
        .optional()
        .db()?;
    if let Some(pk) = found {
        return Ok(pk);
    }
    conn.prepare_cached("INSERT INTO threads(acct, id) VALUES (?1, ?2)").db()?.execute(params![acct, id.as_str()]).db()?;
    Ok(conn.last_insert_rowid())
}

/// Recomputes a thread's row and its `thread_mailboxes` from its messages; deletes it if it has none.
/// Returns the thread's provider id (None if there was no such row).
pub(crate) fn refresh_thread(conn: &Connection, tid: i64) -> Result<Option<ThreadId>> {
    let row: Option<(String, i64)> = conn
        .prepare_cached("SELECT id, acct FROM threads WHERE pk=?1")
        .db()?
        .query_row([tid], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .db()?;
    let Some((id, acct)) = row else { return Ok(None) };

    struct M {
        subject: String,
        snippet: String,
        from: String,
        date: i64,
        seen: bool,
        flagged: bool,
        att: bool,
    }
    let msgs: Vec<M> = conn
        .prepare_cached(
            "SELECT subject, snippet, from_json, date, seen, flagged, has_attachments
             FROM messages WHERE tid=?1 ORDER BY date, pk",
        )
        .db()?
        .query_map([tid], |r| {
            Ok(M {
                subject: r.get(0)?,
                snippet: r.get(1)?,
                from: r.get(2)?,
                date: r.get(3)?,
                seen: r.get(4)?,
                flagged: r.get(5)?,
                att: r.get(6)?,
            })
        })
        .db()?
        .collect::<rusqlite::Result<_>>()
        .db()?;
    if msgs.is_empty() {
        conn.prepare_cached("DELETE FROM threads WHERE pk=?1").db()?.execute([tid]).db()?;
        return Ok(Some(ThreadId(id)));
    }

    let mut participants: Vec<Address> = Vec::new();
    let mut seen_emails = HashSet::new();
    for m in &msgs {
        for a in from_json::<Vec<Address>>(&m.from)? {
            if seen_emails.insert(a.email.clone()) {
                participants.push(a);
            }
        }
    }
    let last = msgs.last().expect("not empty");
    let subject = msgs.iter().map(|m| m.subject.as_str()).find(|s| !s.trim().is_empty()).unwrap_or("");
    let unread = msgs.iter().filter(|m| !m.seen).count() as i64;
    let flagged = msgs.iter().any(|m| m.flagged);
    let att = msgs.iter().any(|m| m.att);

    let boxes: Vec<(i64, String, String)> = conn
        .prepare_cached(
            "SELECT DISTINCT b.pk, b.id, b.role FROM messages m
             JOIN message_mailboxes mm ON mm.msg=m.pk JOIN mailboxes b ON b.pk=mm.mbox
             WHERE m.tid=?1 ORDER BY b.pk",
        )
        .db()?
        .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .db()?
        .collect::<rusqlite::Result<_>>()
        .db()?;
    let box_ids: Vec<&str> = boxes.iter().map(|b| b.1.as_str()).collect();

    conn.prepare_cached(
        "UPDATE threads SET subject=?2, snippet=?3, participants=?4, message_count=?5, unread_count=?6,
           date=?7, flagged=?8, has_attachments=?9, mailboxes=?10 WHERE pk=?1",
    )
    .db()?
    .execute(params![
        tid,
        subject,
        last.snippet,
        to_json(&participants)?,
        msgs.len() as i64,
        unread,
        last.date,
        flagged,
        att,
        to_json(&box_ids)?
    ])
    .db()?;
    conn.prepare_cached("DELETE FROM thread_mailboxes WHERE tid=?1").db()?.execute([tid]).db()?;
    let mut ins = conn
        .prepare_cached("INSERT INTO thread_mailboxes(tid, mbox, role, acct, date) VALUES (?1, ?2, ?3, ?4, ?5)")
        .db()?;
    for (pk, _, role) in &boxes {
        ins.execute(params![tid, pk, role, acct, last.date]).db()?;
    }
    Ok(Some(ThreadId(id)))
}

/// "name email name email" for the search index.
fn addresses_text(lists: &[&[Address]]) -> String {
    let mut s = String::new();
    for list in lists {
        for a in *list {
            if let Some(n) = &a.name {
                s.push_str(n);
                s.push(' ');
            }
            s.push_str(&a.email);
            s.push(' ');
        }
    }
    s
}

/// Searchable text of an HTML-only body: render's plain text (no hidden text, scripts or styles), with
/// the quoted history and signature in, because people search for those too.
fn html_index_text(html: &str) -> String {
    let body = lumbridge_mail_core::Body { html: Some(html.to_owned()), ..Default::default() };
    let opts = lumbridge_mail_render::TextOptions {
        quoted: true,
        signature: true,
        max_chars: INDEXED_BODY_BYTES,
        ..Default::default()
    };
    lumbridge_mail_render::plain_text(&body, &opts)
}

/// Longest body text put in the search index, in bytes.
const INDEXED_BODY_BYTES: usize = 64 * 1024;

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Rebuilds one message's search row from what is stored (metadata, cached body, attachment names).
pub(crate) fn reindex_message(conn: &Connection, pk: i64, existed: bool, terms: &mut BTreeSet<String>) -> Result<()> {
    let row = conn
        .prepare_cached(
            "SELECT m.subject, m.from_json, m.to_json, m.cc_json, m.bcc_json, m.snippet, b.text,
               (SELECT group_concat(filename, ' ') FROM attachments WHERE msg=m.pk), b.html
             FROM messages m LEFT JOIN bodies b ON b.msg=m.pk WHERE m.pk=?1",
        )
        .db()?
        .query_row([pk], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })
        .optional()
        .db()?;
    let Some((subject, from, to, cc, bcc, snippet, text, files, html)) = row else { return Ok(()) };
    let from: Vec<Address> = from_json(&from)?;
    let (to, cc, bcc): (Vec<Address>, Vec<Address>, Vec<Address>) = (from_json(&to)?, from_json(&cc)?, from_json(&bcc)?);
    let mut body = snippet;
    let text = match text {
        Some(t) if !t.trim().is_empty() => Some(t),
        _ => html.filter(|h| !h.trim().is_empty()).map(|h| html_index_text(&h)),
    };
    if let Some(t) = text {
        body.push('\n');
        body.push_str(truncate(&t, INDEXED_BODY_BYTES));
    }
    fts_put(
        conn,
        pk,
        existed,
        terms,
        [&subject, &addresses_text(&[&from]), &addresses_text(&[&to, &cc, &bcc]), &body, &files.unwrap_or_default()],
    )
}

/// Longest word kept in the vocabulary (longer ones are ids, hashes, base64).
const MAX_TERM_CHARS: usize = 32;

/// Splits text into lower-cased words roughly the way FTS5's unicode61 tokenizer does.
pub(crate) fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2 && w.chars().count() <= MAX_TERM_CHARS)
        .map(str::to_lowercase)
}

/// Adds words to the vocabulary used for prefix checks and typo tolerance. Words are never removed (a
/// stale word only costs a fuzzy candidate that matches nothing).
pub(crate) fn save_terms(conn: &Connection, terms: &BTreeSet<String>) -> Result<()> {
    let mut st = conn.prepare_cached("INSERT OR IGNORE INTO terms(term) VALUES (?1)").db()?;
    for t in terms {
        st.execute([t]).db()?;
    }
    Ok(())
}

fn fts_put(conn: &Connection, pk: i64, existed: bool, terms: &mut BTreeSet<String>, cols: [&str; 5]) -> Result<()> {
    for c in cols {
        terms.extend(words(c));
    }
    if existed {
        conn.prepare_cached("DELETE FROM fts WHERE rowid=?1").db()?.execute([pk]).db()?;
    }
    conn.prepare_cached("INSERT INTO fts(rowid, subject, sender, recipients, body, files) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
        .db()?
        .execute(params![pk, cols[0], cols[1], cols[2], cols[3], cols[4]])
        .db()?;
    Ok(())
}

/// A partial record ([`MessageMeta::partial`]) filled in from what is stored: every empty field keeps
/// the stored value. None when the message is not stored yet (the record is used as it is).
fn merged_partial(conn: &Connection, acct: i64, m: &MessageMeta) -> Result<Option<MessageMeta>> {
    let row = conn
        .prepare_cached(
            "SELECT snippet, to_json, cc_json, bcc_json, date, has_attachments, size, rfc_id, in_reply_to, refs_json
             FROM messages WHERE acct=?1 AND id=?2",
        )
        .db()?
        .query_row(params![acct, m.id.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, String>(9)?,
            ))
        })
        .optional()
        .db()?;
    let Some((snippet, to, cc, bcc, date, att, size, rfc, irt, refs)) = row else { return Ok(None) };
    let mut out = m.clone();
    if out.snippet.trim().is_empty() {
        out.snippet = snippet;
    }
    if out.to.is_empty() {
        out.to = from_json(&to)?;
    }
    if out.cc.is_empty() {
        out.cc = from_json(&cc)?;
    }
    if out.bcc.is_empty() {
        out.bcc = from_json(&bcc)?;
    }
    if out.date == 0 {
        out.date = date;
    }
    out.has_attachments |= att;
    if out.size == 0 {
        out.size = size.max(0) as u64;
    }
    if out.rfc_message_id.is_none() {
        out.rfc_message_id = rfc;
    }
    if out.in_reply_to.is_none() {
        out.in_reply_to = irt;
    }
    if out.references.is_empty() {
        out.references = from_json(&refs)?;
    }
    Ok(Some(out))
}

/// Inserts or updates one message. Returns its pk and whether it is new.
fn upsert_message(conn: &Connection, acct: i64, m: &MessageMeta, touched: &mut Touched) -> Result<(i64, bool)> {
    if m.id.as_str().is_empty() || m.thread.as_str().is_empty() {
        return Err(Error::Invalid("message or thread id is empty".into()));
    }
    let merged = if m.partial { merged_partial(conn, acct, m)? } else { None };
    let m = merged.as_ref().unwrap_or(m);
    let tid = ensure_thread(conn, acct, &m.thread)?;
    let from = to_json(&m.from)?;
    let to = to_json(&m.to)?;
    let cc = to_json(&m.cc)?;
    let bcc = to_json(&m.bcc)?;
    let refs = to_json(&m.references)?;
    struct Old {
        pk: i64,
        tid: i64,
        /// subject, snippet, from, to, cc, bcc as stored.
        text: [String; 6],
    }
    let old: Option<Old> = conn
        .prepare_cached(
            "SELECT pk, tid, subject, snippet, from_json, to_json, cc_json, bcc_json FROM messages WHERE acct=?1 AND id=?2",
        )
        .db()?
        .query_row(params![acct, m.id.as_str()], |r| {
            Ok(Old { pk: r.get(0)?, tid: r.get(1)?, text: [r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?] })
        })
        .optional()
        .db()?;
    let f = m.flags;
    let (pk, is_new, text_changed) = match old {
        Some(Old { pk, tid: old_tid, text }) => {
            if old_tid != tid {
                touched.threads.insert(old_tid);
            }
            conn.prepare_cached(
                "UPDATE messages SET tid=?2, subject=?3, snippet=?4, from_json=?5, to_json=?6, cc_json=?7,
                   bcc_json=?8, date=?9, seen=?10, flagged=?11, answered=?12, draft=?13, has_attachments=?14,
                   size=?15, rfc_id=?16, in_reply_to=?17, refs_json=?18 WHERE pk=?1",
            )
            .db()?
            .execute(params![
                pk,
                tid,
                m.subject,
                m.snippet,
                from,
                to,
                cc,
                bcc,
                m.date,
                f.seen,
                f.flagged,
                f.answered,
                f.draft,
                m.has_attachments,
                m.size as i64,
                m.rfc_message_id,
                m.in_reply_to,
                refs
            ])
            .db()?;
            let changed = text.iter().zip([&m.subject, &m.snippet, &from, &to, &cc, &bcc]).any(|(a, b)| a != b);
            (pk, false, changed)
        }
        None => {
            conn.prepare_cached(
                "INSERT INTO messages(acct, id, tid, subject, snippet, from_json, to_json, cc_json, bcc_json, date,
                   seen, flagged, answered, draft, has_attachments, size, rfc_id, in_reply_to, refs_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            )
            .db()?
            .execute(params![
                acct,
                m.id.as_str(),
                tid,
                m.subject,
                m.snippet,
                from,
                to,
                cc,
                bcc,
                m.date,
                f.seen,
                f.flagged,
                f.answered,
                f.draft,
                m.has_attachments,
                m.size as i64,
                m.rfc_message_id,
                m.in_reply_to,
                refs
            ])
            .db()?;
            (conn.last_insert_rowid(), true, true)
        }
    };
    touched.threads.insert(tid);

    // Mailbox membership: rewrite it (a handful of rows) and recount every mailbox involved.
    if !is_new {
        let mut st = conn.prepare_cached("SELECT mbox FROM message_mailboxes WHERE msg=?1").db()?;
        for b in st.query_map([pk], |r| r.get::<_, i64>(0)).db()? {
            touched.mboxes.insert(b.db()?);
        }
        conn.prepare_cached("DELETE FROM message_mailboxes WHERE msg=?1").db()?.execute([pk]).db()?;
    }
    let mut ins = conn.prepare_cached("INSERT OR IGNORE INTO message_mailboxes(msg, mbox, seen) VALUES (?1, ?2, ?3)").db()?;
    for b in &m.mailboxes {
        let mbox = ensure_mailbox(conn, acct, b)?;
        ins.execute(params![pk, mbox, f.seen]).db()?;
        touched.mboxes.insert(mbox);
    }

    if is_new {
        fts_put(
            conn,
            pk,
            false,
            &mut touched.terms,
            [&m.subject, &addresses_text(&[&m.from]), &addresses_text(&[&m.to, &m.cc, &m.bcc]), &m.snippet, ""],
        )?;
    } else if text_changed {
        reindex_message(conn, pk, true, &mut touched.terms)?;
    }
    Ok((pk, is_new))
}

/// Deletes one message and its search row. Returns false if it was not stored.
fn remove_message(conn: &Connection, acct: i64, id: &MessageId, touched: &mut Touched) -> Result<bool> {
    let row: Option<(i64, i64)> = conn
        .prepare_cached("SELECT pk, tid FROM messages WHERE acct=?1 AND id=?2")
        .db()?
        .query_row(params![acct, id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .db()?;
    let Some((pk, tid)) = row else { return Ok(false) };
    let mut st = conn.prepare_cached("SELECT mbox FROM message_mailboxes WHERE msg=?1").db()?;
    for b in st.query_map([pk], |r| r.get::<_, i64>(0)).db()? {
        touched.mboxes.insert(b.db()?);
    }
    conn.prepare_cached("DELETE FROM fts WHERE rowid=?1").db()?.execute([pk]).db()?;
    conn.prepare_cached("DELETE FROM messages WHERE pk=?1").db()?.execute([pk]).db()?;
    touched.threads.insert(tid);
    Ok(true)
}

fn sync_mailboxes(conn: &Connection, acct: i64, list: &[Mailbox], touched: &mut Touched) -> Result<()> {
    let mut keep = HashSet::new();
    for b in list {
        let role = role_str(b.role);
        let old: Option<(i64, String)> = conn
            .prepare_cached("SELECT pk, role FROM mailboxes WHERE acct=?1 AND id=?2")
            .db()?
            .query_row(params![acct, b.id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .db()?;
        match old {
            Some((pk, old_role)) => {
                conn.prepare_cached("UPDATE mailboxes SET name=?2, role=?3 WHERE pk=?1")
                    .db()?
                    .execute(params![pk, b.name, role])
                    .db()?;
                if old_role != role {
                    conn.prepare_cached("UPDATE thread_mailboxes SET role=?2 WHERE mbox=?1")
                        .db()?
                        .execute(params![pk, role])
                        .db()?;
                }
            }
            None => {
                conn.prepare_cached("INSERT INTO mailboxes(acct, id, name, role) VALUES (?1, ?2, ?3, ?4)")
                    .db()?
                    .execute(params![acct, b.id.as_str(), b.name, role])
                    .db()?;
            }
        }
        keep.insert(b.id.as_str());
    }
    let existing: Vec<(i64, String)> = conn
        .prepare_cached("SELECT pk, id FROM mailboxes WHERE acct=?1")
        .db()?
        .query_map([acct], |r| Ok((r.get(0)?, r.get(1)?)))
        .db()?
        .collect::<rusqlite::Result<_>>()
        .db()?;
    for (pk, id) in existing {
        if keep.contains(id.as_str()) {
            continue;
        }
        let mut st = conn.prepare_cached("SELECT tid FROM thread_mailboxes WHERE mbox=?1").db()?;
        for t in st.query_map([pk], |r| r.get::<_, i64>(0)).db()? {
            touched.threads.insert(t.db()?);
        }
        conn.prepare_cached("DELETE FROM mailboxes WHERE pk=?1").db()?.execute([pk]).db()?;
        touched.mboxes.remove(&pk);
    }
    Ok(())
}

pub(crate) const MESSAGE_COLUMNS: &str = "m.pk, m.id, t.id, m.subject, m.snippet, m.from_json, m.to_json, m.cc_json,
    m.bcc_json, m.date, m.seen, m.flagged, m.answered, m.draft, m.has_attachments, m.size, m.rfc_id, m.in_reply_to,
    m.refs_json";

/// Reads a row selected with [`MESSAGE_COLUMNS`] (mailboxes filled in by the caller).
fn meta_from_row(r: &rusqlite::Row<'_>, account: &AccountId) -> rusqlite::Result<(i64, MessageMeta, [String; 5])> {
    let meta = MessageMeta {
        id: MessageId::new(r.get::<_, String>(1)?),
        account: account.clone(),
        thread: ThreadId::new(r.get::<_, String>(2)?),
        mailboxes: Vec::new(),
        from: Vec::new(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: r.get(3)?,
        snippet: r.get(4)?,
        date: r.get(9)?,
        flags: Flags { seen: r.get(10)?, flagged: r.get(11)?, answered: r.get(12)?, draft: r.get(13)? },
        has_attachments: r.get(14)?,
        size: r.get::<_, i64>(15)? as u64,
        rfc_message_id: r.get(16)?,
        in_reply_to: r.get(17)?,
        references: Vec::new(),
        partial: false,
    };
    Ok((r.get(0)?, meta, [r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(18)?]))
}

fn finish_meta(conn: &Connection, pk: i64, mut meta: MessageMeta, json: [String; 5]) -> Result<MessageMeta> {
    let [from, to, cc, bcc, refs] = json;
    meta.from = from_json(&from)?;
    meta.to = from_json(&to)?;
    meta.cc = from_json(&cc)?;
    meta.bcc = from_json(&bcc)?;
    meta.references = from_json(&refs)?;
    let mut st = conn
        .prepare_cached(
            "SELECT b.id FROM message_mailboxes mm JOIN mailboxes b ON b.pk=mm.mbox WHERE mm.msg=?1 ORDER BY b.pk",
        )
        .db()?;
    meta.mailboxes =
        st.query_map([pk], |r| Ok(MailboxId(r.get(0)?))).db()?.collect::<rusqlite::Result<_>>().db()?;
    Ok(meta)
}

/// The pk and thread pk of a stored message.
pub(crate) fn message_pk(conn: &Connection, acct: i64, id: &MessageId) -> Result<Option<(i64, i64)>> {
    conn.prepare_cached("SELECT pk, tid FROM messages WHERE acct=?1 AND id=?2")
        .db()?
        .query_row(params![acct, id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .db()
}

pub(crate) fn thread_pk(conn: &Connection, acct: i64, id: &ThreadId) -> Result<Option<i64>> {
    conn.prepare_cached("SELECT pk FROM threads WHERE acct=?1 AND id=?2")
        .db()?
        .query_row(params![acct, id.as_str()], |r| r.get(0))
        .optional()
        .db()
}

impl Store {
    /// Applies one provider delta for `account` in a single transaction: replaces the mailbox list
    /// (`mailboxes: None` means "unchanged", `Some(vec![])` "the account has none"), upserts and removes
    /// messages, keeps pending queued actions applied on top, recomputes the touched threads and mailbox
    /// counts, and stores `delta.cursor` (None clears it, so the next sync is a full one) with the time of
    /// this sync.
    ///
    /// `delta.reset`: the pages of a full sync. The store remembers which messages the run reported; when
    /// its last page (`more: false`) is applied, every other message of the account is removed (counted in
    /// `Applied::removed`); with `delta.window_floor` only those dated at or after it (older mail is outside
    /// the window and kept). A run starts with the first reset page after a non-reset one, or with any reset
    /// page while no cursor is stored (the engine called `sync(None)`).
    pub fn apply_delta(&mut self, account: &AccountId, delta: &Delta) -> Result<Applied> {
        let tx = self.conn.transaction().db()?;
        let acct = acct_pk(&tx, account)?;
        let mut touched = Touched::default();
        if let Some(list) = &delta.mailboxes {
            sync_mailboxes(&tx, acct, list, &mut touched)?;
        }
        // A reset run starts when a reset page arrives and no run is open, or the engine started over
        // (no stored cursor: it called `sync(None)`).
        let reset_gen = if delta.reset {
            let (generation, active, cursor): (i64, bool, Option<String>) = tx
                .prepare_cached("SELECT reset_gen, reset_active, cursor FROM accounts WHERE pk=?1")
                .db()?
                .query_row([acct], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .db()?;
            let generation = if active && cursor.is_some() { generation } else { generation + 1 };
            tx.prepare_cached("UPDATE accounts SET reset_gen=?2, reset_active=1 WHERE pk=?1")
                .db()?
                .execute(params![acct, generation])
                .db()?;
            Some(generation)
        } else {
            None
        };
        let mut new_messages = Vec::new();
        let mut upserted = HashSet::with_capacity(delta.upserted.len());
        for m in &delta.upserted {
            if m.account != *account {
                return Err(Error::Invalid(format!("message {} is for account {}, not {account}", m.id, m.account)));
            }
            let (pk, is_new) = upsert_message(&tx, acct, m, &mut touched)?;
            if let Some(generation) = reset_gen {
                tx.prepare_cached("UPDATE messages SET sync_gen=?2 WHERE pk=?1").db()?.execute(params![pk, generation]).db()?;
            }
            if is_new {
                new_messages.push(m.id.clone());
            }
            upserted.insert(m.id.as_str());
        }
        let mut removed = 0;
        for id in &delta.removed {
            if remove_message(&tx, acct, id, &mut touched)? {
                removed += 1;
            }
        }
        if let Some(generation) = reset_gen.filter(|_| !delta.more) {
            // The run is complete: whatever it did not report is gone from the server. A windowed run
            // only speaks for mail dated inside its window.
            let floor = delta.window_floor.unwrap_or(i64::MIN);
            let stale: Vec<String> = tx
                .prepare_cached("SELECT id FROM messages WHERE acct=?1 AND (sync_gen IS NULL OR sync_gen<>?2) AND date>=?3")
                .db()?
                .query_map(params![acct, generation, floor], |r| r.get(0))
                .db()?
                .collect::<rusqlite::Result<_>>()
                .db()?;
            for id in stale {
                if remove_message(&tx, acct, &MessageId(id), &mut touched)? {
                    removed += 1;
                }
            }
            tx.prepare_cached("UPDATE accounts SET reset_active=0 WHERE pk=?1").db()?.execute([acct]).db()?;
        }
        if !upserted.is_empty() {
            crate::queue::reapply_pending(&tx, acct, &upserted, &mut touched)?;
        }
        let changed_threads = touched.finish(&tx)?;
        tx.prepare_cached("UPDATE accounts SET cursor=?2, last_sync=?3 WHERE pk=?1")
            .db()?
            .execute(params![acct, delta.cursor.as_ref().map(|c| c.0.as_str()), now()])
            .db()?;
        tx.commit().db()?;
        Ok(Applied { changed_threads, new_messages, removed })
    }

    /// Stores message records that came with something else (a body fetch that returned the whole
    /// thread): upserted like a non-reset delta, but the sync cursor and last-sync time are untouched.
    /// Records for other accounts are refused.
    pub fn merge_metas(&mut self, account: &AccountId, metas: &[MessageMeta]) -> Result<Applied> {
        let tx = self.conn.transaction().db()?;
        let acct = acct_pk(&tx, account)?;
        let mut touched = Touched::default();
        let mut new_messages = Vec::new();
        let mut upserted = HashSet::with_capacity(metas.len());
        for m in metas {
            if m.account != *account {
                return Err(Error::Invalid(format!("message {} is for account {}, not {account}", m.id, m.account)));
            }
            let (_, is_new) = upsert_message(&tx, acct, m, &mut touched)?;
            if is_new {
                new_messages.push(m.id.clone());
            }
            upserted.insert(m.id.as_str());
        }
        if !upserted.is_empty() {
            crate::queue::reapply_pending(&tx, acct, &upserted, &mut touched)?;
        }
        let changed_threads = touched.finish(&tx)?;
        tx.commit().db()?;
        Ok(Applied { changed_threads, new_messages, removed: 0 })
    }

    /// One message's metadata.
    pub fn message(&self, account: &AccountId, id: &MessageId) -> Result<Option<MessageMeta>> {
        let acct = acct_pk(&self.conn, account)?;
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages m JOIN threads t ON t.pk=m.tid WHERE m.acct=?1 AND m.id=?2"
        );
        let row = self
            .conn
            .prepare_cached(&sql)
            .db()?
            .query_row(params![acct, id.as_str()], |r| meta_from_row(r, account))
            .optional()
            .db()?;
        row.map(|(pk, meta, json)| finish_meta(&self.conn, pk, meta, json)).transpose()
    }

    /// A thread's messages, oldest first (`thread.get`). Empty if the thread is unknown.
    pub fn thread(&self, account: &AccountId, thread: &ThreadId) -> Result<Vec<MessageMeta>> {
        let acct = acct_pk(&self.conn, account)?;
        let Some(tid) = thread_pk(&self.conn, acct, thread)? else { return Ok(Vec::new()) };
        let sql =
            format!("SELECT {MESSAGE_COLUMNS} FROM messages m JOIN threads t ON t.pk=m.tid WHERE m.tid=?1 ORDER BY m.date, m.pk");
        let rows: Vec<_> = self
            .conn
            .prepare_cached(&sql)
            .db()?
            .query_map([tid], |r| meta_from_row(r, account))
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?;
        rows.into_iter().map(|(pk, meta, json)| finish_meta(&self.conn, pk, meta, json)).collect()
    }

    /// For providers without threads (IMAP): the thread of a stored message whose `Message-ID` is one of
    /// `rfc_ids` (a new message's `In-Reply-To` and `References`), within one account only. The latest
    /// matching reference wins.
    pub fn thread_for_references(&self, account: &AccountId, rfc_ids: &[String]) -> Result<Option<ThreadId>> {
        let acct = acct_pk(&self.conn, account)?;
        let mut st = self
            .conn
            .prepare_cached(
                "SELECT t.id FROM messages m JOIN threads t ON t.pk=m.tid WHERE m.acct=?1 AND m.rfc_id=?2 LIMIT 1",
            )
            .db()?;
        for id in rfc_ids.iter().rev() {
            let id = id.trim().trim_start_matches('<').trim_end_matches('>');
            if let Some(t) = st.query_row(params![acct, id], |r| r.get::<_, String>(0)).optional().db()? {
                return Ok(Some(ThreadId(t)));
            }
        }
        Ok(None)
    }
}
