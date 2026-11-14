//! `Query` to `ThreadPage`: mailbox lists, the unified inbox, search, with keyset paging.
//!
//! Lists walk an index newest first (`thread_mailboxes` by mailbox or by role, or `threads` by date) and
//! stop after one page. The page token is the last row's `(date, thread pk)`; the next page starts strictly
//! after it, so threads changing between pages never repeat a row (a thread that got newer mail moves to
//! the top and is seen on the next refresh).

use crate::accounts::allowed_accounts;
use crate::search::{SearchQuery, fts_expression, parse_search};
use crate::{DEFAULT_PAGE, Db, MAX_PAGE, Store, category_str, from_json, parse_role, role_str};
use lumbridge_mail_core::rpc::{CategoryCount, ThreadPage};
use lumbridge_mail_core::{AccountId, Category, Error, MailboxRole, Query, Result, ThreadId, ThreadSummary};
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params_from_iter};

/// Up to this many matching threads, a search fetches them all and sorts; above it, it walks by date.
const SMALL_MATCH: usize = 5_000;

fn token(date: i64, tid: i64) -> String {
    format!("{date}.{tid}")
}

fn parse_token(s: &str) -> Result<(i64, i64)> {
    let bad = || Error::Invalid(format!("bad page token {s:?}"));
    let (d, t) = s.split_once('.').ok_or_else(bad)?;
    Ok((d.parse().map_err(|_| bad())?, t.parse().map_err(|_| bad())?))
}

fn in_list(pks: &[i64]) -> String {
    let list: Vec<String> = pks.iter().map(i64::to_string).collect();
    format!("({})", list.join(","))
}

/// SQL conditions on `messages m` for the parts of a search that are not its positive text (flags,
/// dates, mailboxes, categories, `OR` groups and `-` parts, whose own text is matched through the index).
fn search_conditions(conn: &rusqlite::Connection, s: &SearchQuery, sql: &mut Vec<String>, args: &mut Vec<Value>) -> Result<()> {
    if let Some(u) = s.unread {
        sql.push("m.seen = ?".into());
        args.push(Value::Integer(i64::from(!u)));
    }
    if let Some(f) = s.starred {
        sql.push("m.flagged = ?".into());
        args.push(Value::Integer(i64::from(f)));
    }
    if let Some(a) = s.answered {
        sql.push("m.answered = ?".into());
        args.push(Value::Integer(i64::from(a)));
    }
    if s.has_attachment {
        sql.push("m.has_attachments = 1".into());
    }
    if let Some(b) = s.before {
        sql.push("m.date < ?".into());
        args.push(Value::Integer(b));
    }
    if let Some(a) = s.after {
        sql.push("m.date >= ?".into());
        args.push(Value::Integer(a));
    }
    for v in &s.in_mailboxes {
        let role = parse_role(v).or(match v.as_str() {
            "draft" => Some(lumbridge_mail_core::MailboxRole::Drafts),
            _ => None,
        });
        let starred = if v == "starred" { " OR m.flagged = 1" } else { "" };
        sql.push(format!("({IN_BOX} (b.role = ? OR lower(b.name) = ? OR lower(b.id) = ?)){starred})"));
        args.push(Value::Text(role.map(role_str).unwrap_or("").into()));
        args.push(Value::Text(v.clone()));
        args.push(Value::Text(v.clone()));
    }
    for v in &s.categories {
        sql.push(category_condition(v, args));
    }
    for group in &s.any_of {
        let mut alts = Vec::new();
        for alt in group {
            if let Some(c) = part_condition(conn, &alt.part, args)? {
                alts.push(if alt.negated { format!("NOT ({c})") } else { format!("({c})") });
            }
        }
        if !alts.is_empty() {
            sql.push(format!("({})", alts.join(" OR ")));
        }
    }
    for part in &s.none_of {
        if let Some(c) = part_condition(conn, part, args)? {
            sql.push(format!("NOT ({c})"));
        }
    }
    Ok(())
}

const IN_BOX: &str = "EXISTS (SELECT 1 FROM message_mailboxes mm JOIN mailboxes b ON b.pk=mm.mbox WHERE mm.msg=m.pk AND";

/// One part of a search (an `OR` alternative or a `-` part) as one condition on `m`, its text through
/// the index; None when it has nothing that can match (punctuation only).
fn part_condition(conn: &rusqlite::Connection, part: &SearchQuery, args: &mut Vec<Value>) -> Result<Option<String>> {
    let mut sql = Vec::new();
    search_conditions(conn, part, &mut sql, args)?;
    if part.has_text() {
        match fts_expression(conn, part)? {
            Some(expr) => {
                sql.push("m.pk IN (SELECT rowid FROM fts WHERE fts MATCH ?)".into());
                args.push(Value::Text(expr));
            }
            None if sql.is_empty() => return Ok(None),
            None => {}
        }
    }
    Ok((!sql.is_empty()).then(|| sql.join(" AND ")))
}

/// `category:` — a triage category (`newsletter`, `promotions`), or one of Gmail's tabs, which are
/// labels on the message (`promotions`, `social`, `updates`, `forums`, `primary`).
fn category_condition(v: &str, args: &mut Vec<Value>) -> String {
    let ours = Category::parse(v).or_else(|| v.strip_suffix('s').and_then(Category::parse));
    let tab = match v {
        "promotions" | "promotion" => Some("CATEGORY_PROMOTIONS"),
        "social" => Some("CATEGORY_SOCIAL"),
        "updates" => Some("CATEGORY_UPDATES"),
        "forums" => Some("CATEGORY_FORUMS"),
        "primary" => Some("CATEGORY_PERSONAL"),
        _ => None,
    };
    let mut any = Vec::new();
    if let Some(c) = ours {
        any.push("m.tid IN (SELECT pk FROM threads WHERE category = ?)".to_owned());
        args.push(Value::Text(category_str(c)));
    }
    if let Some(label) = tab {
        any.push(format!("{IN_BOX} b.id = ?)"));
        args.push(Value::Text(label.into()));
    }
    if any.is_empty() { "0".into() } else { format!("({})", any.join(" OR ")) }
}

/// The Query's own mailbox / role, as conditions on `m`.
fn query_conditions(q: &Query, sql: &mut Vec<String>, args: &mut Vec<Value>) {
    if let Some(mbox) = &q.mailbox {
        sql.push(format!("{IN_BOX} b.id = ?)"));
        args.push(Value::Text(mbox.0.clone()));
    } else if let Some(role) = q.role {
        sql.push(format!("{IN_BOX} b.role = ?)"));
        args.push(Value::Text(role_str(role).into()));
    }
}

impl Store {
    /// Runs a [`Query`] (`threads.list`): newest thread first, one page.
    ///
    /// - `accounts: None` means every account except anon ones (the anon wall); anon accounts are only
    ///   included when named in `accounts`.
    /// - `mailbox`: threads with a message in that mailbox (by id, in each chosen account).
    /// - `role` with no `mailbox`: threads in any mailbox with that role across the chosen accounts
    ///   (`role: Inbox` is the unified inbox).
    /// - `text`: the search language in [`crate::search`]; a thread matches if one of its messages
    ///   matches every part (and is in `mailbox` / `role` when those are set).
    /// - `category`: threads whose triage put them in that category. `unread_only`: threads with unread mail.
    /// - `since`: threads with mail at or after that time. `needs_reply` / `min_priority`: from the stored
    ///   triage (threads not triaged yet match neither).
    /// - `limit` 0 means [`DEFAULT_PAGE`]; it is capped at [`MAX_PAGE`]. `page` is the previous page's `next`.
    pub fn list_threads(&self, q: &Query) -> Result<ThreadPage> {
        let limit = match q.limit {
            0 => DEFAULT_PAGE,
            n => n.min(MAX_PAGE),
        } as i64;
        let empty = ThreadPage { threads: Vec::new(), next: None };
        let allowed = allowed_accounts(&self.conn, q.accounts.as_deref())?;
        if allowed.as_ref().is_some_and(|a| a.is_empty()) {
            return Ok(empty);
        }
        let cursor = q.page.as_deref().map(parse_token).transpose()?;
        let search = q.text.as_deref().map(parse_search).filter(|s| *s != SearchQuery::default());

        let mut sql;
        let mut cond: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        let (date_col, tid_col);

        if let Some(s) = &search {
            date_col = "t.date";
            tid_col = "t.pk";
            sql = "SELECT t.pk, t.date FROM threads t".to_owned();
            let mut mcond: Vec<String> = Vec::new();
            if let Some(a) = &allowed {
                cond.push(format!("t.acct IN {}", in_list(a)));
                mcond.push(format!("m.acct IN {}", in_list(a)));
            }
            // Conditions on one message `m`; their arguments are kept apart and appended in SQL order.
            let mut margs: Vec<Value> = Vec::new();
            search_conditions(&self.conn, s, &mut mcond, &mut margs)?;
            query_conditions(q, &mut mcond, &mut margs);
            let fts = if s.has_text() { fts_expression(&self.conn, s)? } else { None };
            let and_m: String = mcond.iter().map(|c| format!(" AND {c}")).collect();

            // Probe: up to SMALL_MATCH + 1 matching threads, streaming from the search index when there is
            // a text part. Few matches: fetch exactly those and sort. Many: walk threads newest first and
            // check each (matches are dense, so a page fills quickly).
            let (probe_sql, probe_args) = match &fts {
                Some(expr) => {
                    let mut a = vec![Value::Text(expr.clone())];
                    a.extend(margs.iter().cloned());
                    (format!("SELECT DISTINCT m.tid FROM fts JOIN messages m ON m.pk = fts.rowid WHERE fts MATCH ?{and_m}"), a)
                }
                None => (format!("SELECT DISTINCT m.tid FROM messages m WHERE 1{and_m}"), margs.clone()),
            };
            let probe: Vec<i64> = self
                .conn
                .prepare_cached(&format!("{probe_sql} LIMIT {}", SMALL_MATCH + 1))
                .db()?
                .query_map(params_from_iter(probe_args), |r| r.get(0))
                .db()?
                .collect::<rusqlite::Result<_>>()
                .db()?;
            if probe.is_empty() {
                return Ok(empty);
            }
            if probe.len() <= SMALL_MATCH {
                cond.push(format!("t.pk IN {}", in_list(&probe)));
            } else {
                // The index is read once (a correlated MATCH per message re-reads prefix doclists every time).
                let fts_check = if fts.is_some() { " AND m.pk IN (SELECT rowid FROM fts WHERE fts MATCH ?)" } else { "" };
                cond.push(format!("EXISTS (SELECT 1 FROM messages m WHERE m.tid = t.pk{and_m}{fts_check})"));
                args.extend(margs);
                if let Some(expr) = fts {
                    args.push(Value::Text(expr));
                }
            }
        } else if q.category.is_some() {
            // Category views drive from the category index; the mailbox or role becomes a check.
            date_col = "t.date";
            tid_col = "t.pk";
            sql = "SELECT t.pk, t.date FROM threads t".to_owned();
            if let Some(a) = &allowed {
                cond.push(format!("t.acct IN {}", in_list(a)));
            }
            if let Some(mbox) = &q.mailbox {
                let boxes = self.boxes_named(mbox, allowed.as_deref())?;
                if boxes.is_empty() {
                    return Ok(empty);
                }
                cond.push(format!("EXISTS (SELECT 1 FROM thread_mailboxes tm WHERE tm.tid = t.pk AND tm.mbox IN {})", in_list(&boxes)));
            } else if let Some(role) = q.role {
                cond.push("EXISTS (SELECT 1 FROM thread_mailboxes tm WHERE tm.tid = t.pk AND tm.role = ?)".into());
                args.push(Value::Text(role_str(role).into()));
            }
        } else if let Some(mbox) = &q.mailbox {
            date_col = "tm.date";
            tid_col = "tm.tid";
            let boxes = self.boxes_named(mbox, allowed.as_deref())?;
            if boxes.is_empty() {
                return Ok(empty);
            }
            sql = "SELECT tm.tid, tm.date FROM thread_mailboxes tm".to_owned();
            cond.push(if boxes.len() == 1 { format!("tm.mbox = {}", boxes[0]) } else { format!("tm.mbox IN {}", in_list(&boxes)) });
        } else if let Some(role) = q.role {
            date_col = "tm.date";
            tid_col = "tm.tid";
            sql = "SELECT tm.tid, tm.date FROM thread_mailboxes tm".to_owned();
            cond.push("tm.role = ?".into());
            args.push(Value::Text(role_str(role).into()));
            if let Some(a) = &allowed {
                cond.push(format!("tm.acct IN {}", in_list(a)));
            }
        } else {
            date_col = "t.date";
            tid_col = "t.pk";
            sql = "SELECT t.pk, t.date FROM threads t".to_owned();
            if let Some(a) = &allowed {
                cond.push(format!("t.acct IN {}", in_list(a)));
            }
        }

        let needs_thread = q.unread_only || q.category.is_some() || q.needs_reply || q.min_priority.is_some();
        if needs_thread && tid_col == "tm.tid" {
            sql.push_str(" JOIN threads t ON t.pk = tm.tid");
        }
        if q.unread_only {
            cond.push("t.unread_count > 0".into());
        }
        if let Some(c) = q.category {
            cond.push("t.category = ?".into());
            args.push(Value::Text(category_str(c)));
        }
        if let Some(since) = q.since {
            cond.push(format!("{date_col} >= ?"));
            args.push(Value::Integer(since));
        }
        // From the stored triage: threads without one match neither.
        if q.needs_reply {
            cond.push("json_extract(t.triage, '$.needs_reply') = 1".into());
        }
        if let Some(p) = q.min_priority {
            cond.push("json_extract(t.triage, '$.priority') >= ?".into());
            args.push(Value::Integer(i64::from(p)));
        }
        if let Some((d, t)) = cursor {
            cond.push(format!("({date_col}, {tid_col}) < (?, ?)"));
            args.push(Value::Integer(d));
            args.push(Value::Integer(t));
        }
        if !cond.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&cond.join(" AND "));
        }
        sql.push_str(&format!(" ORDER BY {date_col} DESC, {tid_col} DESC LIMIT ?"));
        args.push(Value::Integer(limit + 1));

        // Not cached: the SQL embeds id lists that change from call to call.
        let mut st = self.conn.prepare(&sql).db()?;
        let rows: Vec<(i64, i64)> = st
            .query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?)))
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?;
        let more = rows.len() as i64 > limit;
        let rows = &rows[..rows.len().min(limit as usize)];
        let next = if more { rows.last().map(|(t, d)| token(*d, *t)) } else { None };
        let mut threads = Vec::with_capacity(rows.len());
        let mut last = None;
        for (tid, _) in rows {
            // A thread in two mailboxes with the same role shows up twice in a role list: keep one.
            if last == Some(*tid) {
                continue;
            }
            last = Some(*tid);
            if let Some(t) = self.summary_by_pk(*tid)? {
                threads.push(t);
            }
        }
        Ok(ThreadPage { threads, next })
    }

    /// Pks of the mailboxes with this id in the allowed accounts.
    fn boxes_named(&self, mbox: &lumbridge_mail_core::MailboxId, allowed: Option<&[i64]>) -> Result<Vec<i64>> {
        let mut boxes = Vec::new();
        let mut st = self.conn.prepare_cached("SELECT pk, acct FROM mailboxes WHERE id=?1").db()?;
        for r in st.query_map([mbox.as_str()], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).db()? {
            let (pk, acct) = r.db()?;
            if allowed.is_none_or(|a| a.contains(&acct)) {
                boxes.push(pk);
            }
        }
        Ok(boxes)
    }

    /// Threads per AI category, and how many of them have unread mail (`counts`). `accounts: None` is
    /// every account except anon ones (the anon wall); `role` limits it to threads in a mailbox with that
    /// role (e.g. the unified inbox). Categories with no threads are left out; order is `Category::ALL`.
    pub fn category_counts(&self, accounts: Option<&[AccountId]>, role: Option<MailboxRole>) -> Result<Vec<CategoryCount>> {
        let allowed = allowed_accounts(&self.conn, accounts)?;
        if allowed.as_ref().is_some_and(|a| a.is_empty()) {
            return Ok(Vec::new());
        }
        let mut sql = "SELECT t.category, count(*), coalesce(sum(t.unread_count > 0), 0) FROM threads t WHERE t.category IS NOT NULL"
            .to_owned();
        let mut args: Vec<Value> = Vec::new();
        if let Some(a) = &allowed {
            sql.push_str(&format!(" AND t.acct IN {}", in_list(a)));
        }
        if let Some(r) = role {
            sql.push_str(" AND EXISTS (SELECT 1 FROM thread_mailboxes tm WHERE tm.tid=t.pk AND tm.role=?)");
            args.push(Value::Text(role_str(r).into()));
        }
        sql.push_str(" GROUP BY t.category");
        let rows: Vec<(String, u32, u32)> = self
            .conn
            .prepare(&sql)
            .db()?
            .query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?;
        let mut out: Vec<CategoryCount> = rows
            .into_iter()
            .filter_map(|(c, threads, unread)| Some(CategoryCount { category: Category::parse(&c)?, threads, unread }))
            .collect();
        out.sort_by_key(|c| Category::ALL.iter().position(|x| *x == c.category));
        Ok(out)
    }

    /// One thread's list row.
    pub fn thread_summary(&self, account: &AccountId, thread: &ThreadId) -> Result<Option<ThreadSummary>> {
        let acct = crate::accounts::acct_pk(&self.conn, account)?;
        match crate::sync::thread_pk(&self.conn, acct, thread)? {
            Some(tid) => self.summary_by_pk(tid),
            None => Ok(None),
        }
    }

    fn summary_by_pk(&self, tid: i64) -> Result<Option<ThreadSummary>> {
        let row = self
            .conn
            .prepare_cached(
                "SELECT a.id, t.id, t.subject, t.snippet, t.participants, t.message_count, t.unread_count, t.date,
                   t.flagged, t.has_attachments, t.mailboxes, t.triage
                 FROM threads t JOIN accounts a ON a.pk=t.acct WHERE t.pk=?1",
            )
            .db()?
            .query_row([tid], |r| {
                Ok((
                    (r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?),
                    (r.get::<_, String>(4)?, r.get::<_, u32>(5)?, r.get::<_, u32>(6)?, r.get::<_, i64>(7)?),
                    (r.get::<_, bool>(8)?, r.get::<_, bool>(9)?, r.get::<_, String>(10)?, r.get::<_, Option<String>>(11)?),
                ))
            })
            .optional()
            .db()?;
        let Some(((account, id, subject, snippet), (participants, message_count, unread_count, date), (flagged, att, boxes, triage))) =
            row
        else {
            return Ok(None);
        };
        Ok(Some(ThreadSummary {
            id: ThreadId(id),
            account: AccountId(account),
            subject,
            snippet,
            participants: from_json(&participants)?,
            message_count,
            unread_count,
            date,
            flagged,
            has_attachments: att,
            mailboxes: from_json(&boxes)?,
            triage: triage.map(|t| from_json(&t)).transpose()?,
        }))
    }
}
