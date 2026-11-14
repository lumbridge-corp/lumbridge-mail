//! Threads for providers that hand out none (Zoho's `list_mails` rows have no thread id): the store
//! groups their messages itself, with the same scheme IMAP uses (`core::threading`):
//!
//! 1. **Headers.** A message whose `In-Reply-To` / `References` names a stored message's `Message-ID`
//!    joins that message's thread (the latest reference first); a message that a stored message names
//!    takes that message's thread (the parent arrived after its reply).
//! 2. **Subject and people**, while the headers are not known yet (a listing has none; they come with
//!    the body): the same normalised subject (`Re:`, `Fwd:`, `[list]` stripped), at least one of the two
//!    carrying a reply or forward prefix, someone other than the account itself on both, and no more
//!    than [`SUBJECT_WINDOW_S`] outside the thread's first and last message.
//! 3. Otherwise the message starts its own thread, named by its own id (what such providers used before,
//!    so a message alone keeps its thread id).
//!
//! A message already stored keeps its thread unless new headers place it. [`Store::rethread`] runs the
//! same rules over an account's whole mail, oldest first (an upgrade from one-message threads, without a
//! resync).

use crate::accounts::acct_pk;
use crate::sync::{Touched, ensure_thread};
use crate::{Db, Store, from_json};
use lumbridge_mail_core::threading::{clean_message_id, has_reply_prefix, normalise_subject, parse_message_ids};
use lumbridge_mail_core::{AccountId, Address, MessageId, MessageMeta, Result, ThreadId};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeSet, HashMap, HashSet};

/// How far from a thread's first and last message a message may be and still join it by subject.
pub const SUBJECT_WINDOW_S: i64 = 30 * 86_400;

/// What threading needs to know about one message.
#[derive(Debug, Clone)]
struct Known {
    id: String,
    date: i64,
    /// Its own Message-ID (cleaned).
    rfc: Option<String>,
    /// In-Reply-To then References (cleaned, no repeats), the latest reference last.
    refs: Vec<String>,
    subject: String,
    reply: bool,
    people: BTreeSet<String>,
}

impl Known {
    fn new(id: &str, date: i64, subject: &str, rfc: Option<&str>, irt: Option<&str>, references: &[String], people: BTreeSet<String>) -> Self {
        let mut refs: Vec<String> = Vec::new();
        for r in references.iter().map(|r| clean_message_id(r)).chain(irt.map(clean_message_id)) {
            if !r.is_empty() && !refs.contains(&r) {
                refs.push(r);
            }
        }
        Known {
            id: id.to_owned(),
            date,
            rfc: rfc.map(clean_message_id).filter(|r| !r.is_empty()),
            refs,
            subject: normalise_subject(subject),
            reply: has_reply_prefix(subject),
            people,
        }
    }

    fn of(m: &MessageMeta) -> Self {
        let people = people(m.from.iter().chain(&m.to).chain(&m.cc));
        Known::new(m.id.as_str(), m.date, &m.subject, m.rfc_message_id.as_deref(), m.in_reply_to.as_deref(), &m.references, people)
    }
}

fn people<'a>(addrs: impl Iterator<Item = &'a Address>) -> BTreeSet<String> {
    addrs.map(|a| a.email.trim().to_ascii_lowercase()).filter(|e| !e.is_empty()).collect()
}

struct SubjectThread {
    thread: String,
    first: i64,
    last: i64,
    reply: bool,
    people: BTreeSet<String>,
}

/// The rules over an in-memory index of messages already placed.
#[derive(Default)]
struct Threader {
    /// The account's own addresses (everyone shares those with every message).
    me: HashSet<String>,
    by_rfc: HashMap<String, String>,
    /// A Message-ID some placed message refers to -> that message's thread.
    referred: HashMap<String, String>,
    by_subject: HashMap<String, Vec<SubjectThread>>,
}

impl Threader {
    fn new(me: &str) -> Self {
        let mut t = Threader::default();
        if !me.trim().is_empty() {
            t.me.insert(me.trim().to_ascii_lowercase());
        }
        t
    }

    fn add(&mut self, k: &Known, thread: &str) {
        if let Some(r) = &k.rfc {
            self.by_rfc.entry(r.clone()).or_insert_with(|| thread.to_owned());
        }
        for r in &k.refs {
            self.referred.entry(r.clone()).or_insert_with(|| thread.to_owned());
        }
        if k.subject.is_empty() {
            return;
        }
        let list = self.by_subject.entry(k.subject.clone()).or_default();
        match list.iter_mut().find(|s| s.thread == thread) {
            Some(s) => {
                s.first = s.first.min(k.date);
                s.last = s.last.max(k.date);
                s.reply |= k.reply;
                s.people.extend(k.people.iter().cloned());
            }
            None => list.push(SubjectThread {
                thread: thread.to_owned(),
                first: k.date,
                last: k.date,
                reply: k.reply,
                people: k.people.clone(),
            }),
        }
    }

    fn by_headers(&self, k: &Known) -> Option<String> {
        k.refs
            .iter()
            .rev()
            .find_map(|r| self.by_rfc.get(r))
            .or_else(|| k.rfc.as_ref().and_then(|r| self.referred.get(r)))
            .cloned()
    }

    fn others<'a>(&self, people: &'a BTreeSet<String>) -> BTreeSet<&'a String> {
        let o: BTreeSet<&String> = people.iter().filter(|p| !self.me.contains(*p)).collect();
        if o.is_empty() { people.iter().collect() } else { o }
    }

    fn by_subject(&self, k: &Known) -> Option<String> {
        if k.subject.is_empty() {
            return None;
        }
        let mine = self.others(&k.people);
        self.by_subject
            .get(&k.subject)?
            .iter()
            .filter(|s| (k.reply || s.reply) && k.date >= s.first - SUBJECT_WINDOW_S && k.date <= s.last + SUBJECT_WINDOW_S)
            .filter(|s| self.others(&s.people).iter().any(|p| mine.contains(p)))
            // The closest in time.
            .min_by_key(|s| {
                if k.date < s.first {
                    s.first - k.date
                } else if k.date > s.last {
                    k.date - s.last
                } else {
                    0
                }
            })
            .map(|s| s.thread.clone())
    }

    fn resolve(&self, k: &Known) -> Option<String> {
        self.by_headers(k).or_else(|| self.by_subject(k))
    }
}

/// A stored message as threading sees it, with its thread.
fn stored_rows(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<(Known, String)>> {
    let mut st = conn.prepare_cached(sql).db()?;
    let rows = st
        .query_map(p, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                [r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?],
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, String>(9)?,
            ))
        })
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, thread, date, subject, addrs, rfc, irt, refs) in rows {
        let references: Vec<String> = from_json(&refs).unwrap_or_default();
        let mut who = BTreeSet::new();
        for a in &addrs {
            who.extend(people(from_json::<Vec<Address>>(a).unwrap_or_default().iter()));
        }
        out.push((Known::new(&id, date, &subject, rfc.as_deref(), irt.as_deref(), &references, who), thread));
    }
    Ok(out)
}

const ROW: &str = "SELECT m.id, t.id, m.date, m.subject, m.from_json, m.to_json, m.cc_json, m.rfc_id, m.in_reply_to, m.refs_json
                   FROM messages m JOIN threads t ON t.pk = m.tid";

fn account_email(conn: &Connection, acct: i64) -> Result<String> {
    let json: String = conn.prepare_cached("SELECT json FROM accounts WHERE pk=?1").db()?.query_row([acct], |r| r.get(0)).db()?;
    Ok(from_json::<lumbridge_mail_core::Account>(&json).map(|a| a.email).unwrap_or_default())
}

impl Store {
    /// For an account whose provider gives no thread ids: sets `thread` on each record (see the module
    /// docs), before [`Store::apply_delta`] / [`Store::merge_metas`]. Records of messages already stored
    /// keep their stored thread unless their headers now place them; new ones are placed by headers,
    /// then by subject and people against the stored mail around their date and the other records.
    pub fn assign_threads(&self, account: &AccountId, metas: &mut [MessageMeta]) -> Result<()> {
        if metas.is_empty() {
            return Ok(());
        }
        let conn = &self.conn;
        let acct = acct_pk(conn, account)?;
        let mut th = Threader::new(&account_email(conn, acct)?);
        let lo = metas.iter().map(|m| m.date).min().unwrap_or(0).saturating_sub(SUBJECT_WINDOW_S);
        let hi = metas.iter().map(|m| m.date).max().unwrap_or(0).saturating_add(SUBJECT_WINDOW_S);
        let mut stored: HashMap<String, (Known, String)> = HashMap::new();
        for (k, t) in stored_rows(conn, &format!("{ROW} WHERE m.acct=?1 AND m.date BETWEEN ?2 AND ?3 ORDER BY m.date, m.pk"), params![acct, lo, hi])? {
            th.add(&k, &t);
            stored.insert(k.id.clone(), (k, t));
        }
        // Referenced mail outside the window, and the records' own stored rows.
        let by_rfc = format!("{ROW} WHERE m.acct=?1 AND m.rfc_id=?2");
        let by_id = format!("{ROW} WHERE m.acct=?1 AND m.id=?2");
        for m in metas.iter() {
            for r in m.references.iter().map(|r| clean_message_id(r)).chain(m.in_reply_to.as_deref().map(clean_message_id)) {
                if !r.is_empty() && !th.by_rfc.contains_key(&r) {
                    for (k, t) in stored_rows(conn, &by_rfc, params![acct, r])? {
                        th.add(&k, &t);
                    }
                }
            }
            if !stored.contains_key(m.id.as_str()) {
                for (k, t) in stored_rows(conn, &by_id, params![acct, m.id.as_str()])? {
                    stored.insert(k.id.clone(), (k, t));
                }
            }
        }
        let mut order: Vec<usize> = (0..metas.len()).collect();
        order.sort_by_key(|&i| (metas[i].date, i));
        for i in order {
            let m = &mut metas[i];
            let mut k = Known::of(m);
            let thread = match stored.get(m.id.as_str()) {
                // The provider did name a thread (Zoho, when asked for `threadId`): that one.
                _ if !m.thread.as_str().is_empty() && m.thread != ThreadId(m.id.0.clone()) => m.thread.0.clone(),
                Some((old, t)) => {
                    let new_headers = (k.rfc.is_some() && old.rfc.is_none()) || (!k.refs.is_empty() && old.refs.is_empty());
                    if k.rfc.is_none() {
                        k.rfc = old.rfc.clone();
                    }
                    if k.refs.is_empty() {
                        k.refs = old.refs.clone();
                    }
                    match new_headers.then(|| th.by_headers(&k)).flatten() {
                        Some(h) => h,
                        None => t.clone(),
                    }
                }
                None => th.resolve(&k).unwrap_or_else(|| k.id.clone()),
            };
            th.add(&k, &thread);
            m.thread = ThreadId(thread);
        }
        Ok(())
    }

    /// Places every message of `account` again with the rules above, oldest first, and moves those whose
    /// thread changed (one transaction). Messages in a thread not named by their own id (one the provider
    /// named) stay where they are. For accounts whose provider gives no thread ids, once after an
    /// upgrade from one-message threads: no provider call, no resync. Returns the threads that changed.
    pub fn rethread(&mut self, account: &AccountId) -> Result<Vec<ThreadId>> {
        let tx = self.conn.transaction().db()?;
        let acct = acct_pk(&tx, account)?;
        let mut th = Threader::new(&account_email(&tx, acct)?);
        let rows = stored_rows(&tx, &format!("{ROW} WHERE m.acct=?1 ORDER BY m.date, m.pk"), params![acct])?;
        let mut touched = Touched::default();
        for (k, old) in rows {
            // Before this, such a message was alone in a thread named by its own id, or in one the
            // provider named: that one stays.
            let thread = if old != k.id { old.clone() } else { th.resolve(&k).unwrap_or_else(|| k.id.clone()) };
            th.add(&k, &thread);
            if thread != old {
                move_message(&tx, acct, &k.id, &thread, &mut touched)?;
            }
        }
        let changed = touched.finish(&tx)?;
        tx.commit().db()?;
        Ok(changed)
    }

    /// For an account whose provider gives no thread ids: a fetched body's headers tell a message's own
    /// `Message-ID`, `In-Reply-To` and `References`, which its listing did not. Stores the ones missing;
    /// when they name a stored message, this one moves to that message's thread, and stored messages that
    /// name this one (their bodies came first) move to this one's. Returns the threads that changed.
    pub fn learn_thread_headers(&mut self, account: &AccountId, message: &MessageId, headers: &[(String, String)]) -> Result<Vec<ThreadId>> {
        let get = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
        let rfc = get("Message-ID").and_then(|v| parse_message_ids(v).into_iter().next());
        let irt = get("In-Reply-To").and_then(|v| parse_message_ids(v).into_iter().last());
        let refs: Vec<String> = get("References").map(parse_message_ids).unwrap_or_default();
        if rfc.is_none() && irt.is_none() && refs.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self.conn.transaction().db()?;
        let acct = acct_pk(&tx, account)?;
        let Some((mut k, thread)) = stored_rows(&tx, &format!("{ROW} WHERE m.acct=?1 AND m.id=?2"), params![acct, message.as_str()])?.pop()
        else {
            return Ok(Vec::new());
        };
        let had = (k.rfc.clone(), k.refs.clone());
        let refs_json = crate::to_json(&refs)?;
        tx.prepare_cached(
            "UPDATE messages SET rfc_id = COALESCE(rfc_id, ?3), in_reply_to = COALESCE(in_reply_to, ?4),
               refs_json = CASE WHEN refs_json = '[]' THEN ?5 ELSE refs_json END WHERE acct=?1 AND id=?2",
        )
        .db()?
        .execute(params![acct, message.as_str(), rfc, irt, refs_json])
        .db()?;
        if k.rfc.is_none() {
            k.rfc = rfc.clone();
        }
        if k.refs.is_empty() {
            k.refs = Known::new("", 0, "", None, irt.as_deref(), &refs, BTreeSet::new()).refs;
        }
        if (k.rfc.clone(), k.refs.clone()) == had {
            tx.commit().db()?;
            return Ok(Vec::new());
        }
        let mut touched = Touched::default();
        // It answers a stored message: it joins that message's thread.
        let mut th = Threader::new(&account_email(&tx, acct)?);
        for r in &k.refs {
            for (o, t) in stored_rows(&tx, &format!("{ROW} WHERE m.acct=?1 AND m.rfc_id=?2"), params![acct, r])? {
                if o.id != k.id {
                    th.add(&o, &t);
                }
            }
        }
        let thread = match th.by_headers(&k).filter(|t| *t != thread) {
            Some(to) => {
                move_message(&tx, acct, &k.id, &to, &mut touched)?;
                to
            }
            None => thread,
        };
        // Stored messages that answer it (their bodies came first) join its thread.
        if let Some(own) = &k.rfc {
            let sql = format!("{ROW} WHERE m.acct=?1 AND (m.in_reply_to=?2 OR m.refs_json LIKE ?3 ESCAPE '\\')");
            let like = format!("%\"{}\"%", own.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            for (o, t) in stored_rows(&tx, &sql, params![acct, own, like])? {
                if o.id != k.id && t != thread && o.refs.contains(own) {
                    move_message(&tx, acct, &o.id, &thread, &mut touched)?;
                }
            }
        }
        let changed = touched.finish(&tx)?;
        tx.commit().db()?;
        Ok(changed)
    }
}

fn move_message(conn: &Connection, acct: i64, id: &str, thread: &str, touched: &mut Touched) -> Result<()> {
    let tid = ensure_thread(conn, acct, &ThreadId(thread.to_owned()))?;
    let old: Option<i64> =
        conn.prepare_cached("SELECT tid FROM messages WHERE acct=?1 AND id=?2").db()?.query_row(params![acct, id], |r| r.get(0)).optional().db()?;
    let Some(old) = old else { return Ok(()) };
    conn.prepare_cached("UPDATE messages SET tid=?3 WHERE acct=?1 AND id=?2").db()?.execute(params![acct, id, tid]).db()?;
    let mut st = conn.prepare_cached("SELECT mm.mbox FROM message_mailboxes mm JOIN messages m ON m.pk = mm.msg WHERE m.acct=?1 AND m.id=?2").db()?;
    for b in st.query_map(params![acct, id], |r| r.get::<_, i64>(0)).db()? {
        touched.mboxes.insert(b.db()?);
    }
    touched.threads.insert(old);
    touched.threads.insert(tid);
    Ok(())
}
