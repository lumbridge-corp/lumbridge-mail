//! Made-up mail for `--demo`, screenshots and tests. No network, no files: everything is generated from
//! `content.rs` and kept in memory.
//!
//! The first threads are hand-written stories (long threads, receipts, newsletters, a security code);
//! after them comes filler from templates, generated lazily by index, so 50,000 threads cost nothing until
//! a query walks that far. Actions change the in-memory copy and raise the same events the daemon will.
//! The demo follows the safety rules too: sends need a `Person` caller, sensitive mail never reaches the
//! (canned) model, anon accounts stay out of the unified inbox and cross-account answers.

mod content;

use super::Backend;
use async_trait::async_trait;
use content::*;
use lumbridge_mail_core::rpc::{AccountStatus, Act, Answer, AttachmentRef, Caller, CategoryCount, CountsRequest, DraftRequest,
                               Event, Hello, MessageRef, ThreadPage, ThreadRef};
use lumbridge_mail_core::{safety, Account, AccountId, Action, Address, AiPolicy, Attachment, AttachmentId, Body,
                          Category, Draft, Error, Flags, Mailbox, MailboxId, MailboxRole, MessageId, MessageMeta,
                          Persona, ProviderKind, Query, Result, ThreadId, ThreadSummary, Triage, ActionItem, Extracted, Owner};
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

/// Default size of the demo mailbox: the stories plus a few hundred generated threads.
pub const DEFAULT_THREADS: usize = 320;

const DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
    Inbox,
    Archive,
    Trash,
    Spam,
    Sent,
    Drafts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Story(usize),
    Template { t: usize, n: u32, replies: u8 },
    /// Written in the app (sent mail, drafts) or delivered by `sync`; messages live in `State::extra`.
    Fresh,
}

#[derive(Debug, Clone)]
struct Row {
    id: ThreadId,
    account: usize,
    kind: Kind,
    subject: String,
    snippet: String,
    from: Address,
    participants: Vec<Address>,
    date: i64,
    category: Category,
    priority: u8,
    summary: String,
    needs_reply: bool,
    due: Option<i64>,
    has_attachments: bool,
    labels: Vec<String>,
    message_count: u32,
    unread: u32,
    flagged: bool,
    loc: Loc,
    /// Moved to the top of the list (a reply was sent); the copy in `fresh` is the live one.
    moved: bool,
}

#[derive(Debug, Clone)]
struct DMsg {
    from: Address,
    to: Vec<Address>,
    cc: Vec<Address>,
    date: i64,
    subject: String,
    text: String,
    html: Option<String>,
    attachments: Vec<Attachment>,
    headers: Vec<(String, String)>,
}

struct State {
    /// Generated rows, by index; extended on demand.
    rows: Vec<Row>,
    /// Newest first; shown before `rows`.
    fresh: Vec<Row>,
    extra: HashMap<ThreadId, Vec<DMsg>>,
    drafts: HashMap<String, (usize, Draft)>,
    queued: [u32; 3],
    offline: bool,
    last_sync: i64,
    arrivals: usize,
    fresh_counter: usize,
    /// The last queue id handed out by `act`.
    act_counter: u64,
}

pub struct Demo {
    now: i64,
    count: usize,
    state: Mutex<State>,
    events_tx: async_channel::Sender<Event>,
    events_rx: async_channel::Receiver<Event>,
}

fn addr(p: P, account: usize) -> Address {
    if p.1.is_empty() {
        let a = &ACCOUNTS[account];
        Address::new(Some(a.name), a.email)
    } else {
        Address::new(Some(p.0), p.1)
    }
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn snippet_of(text: &str) -> String {
    let flat: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('>') && !(l.starts_with("On ") && l.ends_with("wrote:")))
        .collect::<Vec<_>>()
        .join(" ");
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(200).collect()
}

fn account_id(i: usize) -> AccountId {
    AccountId::new(ACCOUNTS[i].id)
}

fn account_index(id: &AccountId) -> Option<usize> {
    ACCOUNTS.iter().position(|a| a.id == id.as_str())
}

fn persona(i: usize) -> Persona {
    match i {
        PERSONAL => Persona::Personal,
        WORK => Persona::Work,
        _ => Persona::Anon,
    }
}

fn demo_account(i: usize) -> Account {
    let a = &ACCOUNTS[i];
    Account {
        id: account_id(i),
        persona: persona(i),
        label: a.label.into(),
        email: a.email.into(),
        name: a.name.into(),
        provider: ProviderKind::Demo,
        color: Some(a.color.into()),
        ai: if i == ANON { AiPolicy::LocalOnly } else { AiPolicy::Any },
        readonly: false,
    }
}

fn mailbox_id(account: usize, kind: &str) -> MailboxId {
    MailboxId::new(format!("{}:{kind}", ACCOUNTS[account].id))
}

/// The rules layer, as the daemon's AI layer runs it: sensitive mail gets a fixed line and no model.
fn apply_rules(row: &mut Row, body: &str) {
    if let Some(kind) = safety::sensitive_kind(&row.from.email, &row.subject, body) {
        row.category = Category::Sensitive;
        row.summary = format!("{kind} from {}", row.from.display());
        row.needs_reply = false;
        row.due = None;
    }
}

impl Demo {
    /// `now` is the clock the made-up mail is dated against (screenshots pin it); `count` threads in all.
    pub fn new(now: i64, count: usize) -> Self {
        let (events_tx, events_rx) = async_channel::bounded(1024);
        Self {
            now,
            count: count.max(STORIES.len()),
            state: Mutex::new(State {
                rows: Vec::new(),
                fresh: Vec::new(),
                extra: HashMap::new(),
                drafts: HashMap::new(),
                queued: [0; 3],
                offline: false,
                last_sync: now - 90,
                arrivals: 0,
                fresh_counter: 0,
                act_counter: 0,
            }),
            events_tx,
            events_rx,
        }
    }

    pub fn now(&self) -> i64 {
        self.now
    }

    pub fn thread_count(&self) -> usize {
        self.count
    }

    /// Pretend the network went away (or came back). Actions queue up while offline.
    pub fn set_offline(&self, offline: bool) {
        let statuses = {
            let mut s = self.state.lock();
            s.offline = offline;
            if !offline {
                s.queued = [0; 3];
                s.last_sync = self.now;
            }
            self.statuses(&s)
        };
        for status in statuses {
            let _ = self.events_tx.try_send(Event::Status { status });
        }
    }

    pub fn is_offline(&self) -> bool {
        self.state.lock().offline
    }

    fn statuses(&self, s: &State) -> Vec<AccountStatus> {
        (0..ACCOUNTS.len())
            .map(|i| AccountStatus {
                account: account_id(i),
                online: !s.offline,
                last_sync: Some(s.last_sync),
                queued: s.queued[i],
                error: s.offline.then(|| Error::Offline("no network (demo)".into())),
                paused: false,
                body_fill: None,
            })
            .collect()
    }

    fn generate(&self, idx: usize) -> Row {
        if idx < STORIES.len() {
            let st = &STORIES[idx];
            let last = st.msgs.last().expect("stories have messages");
            let mut participants: Vec<Address> = Vec::new();
            for msg in st.msgs {
                let a = addr(msg.from, st.account);
                if !participants.contains(&a) {
                    participants.push(a);
                }
            }
            let from = addr(last.from, st.account);
            let mut row = Row {
                id: ThreadId::new(format!("t{idx}")),
                account: st.account,
                kind: Kind::Story(idx),
                subject: st.subject.into(),
                snippet: snippet_of(last.text),
                from,
                participants,
                date: self.now - last.mins_ago * 60,
                category: st.category,
                priority: st.priority,
                summary: st.summary.into(),
                needs_reply: st.needs_reply,
                due: st.due_days.map(|d| self.now + d * DAY),
                has_attachments: st.msgs.iter().any(|m| !m.attachments.is_empty()),
                labels: st.labels.iter().map(|l| l.to_string()).collect(),
                message_count: st.msgs.len() as u32,
                unread: st.unread,
                flagged: st.flagged,
                loc: Loc::Inbox,
                moved: false,
            };
            apply_rules(&mut row, last.text);
            return row;
        }
        let k = (idx - STORIES.len()) as i64;
        let h = splitmix(idx as u64 ^ 0x5EED_1A7E);
        let ti = (h % TEMPLATES.len() as u64) as usize;
        let tm = &TEMPLATES[ti];
        let n = 100 + ((h >> 16) % 9000) as u32;
        let replies = if (h >> 40).is_multiple_of(5) { 1 } else { 0 };
        // The stories cover the last few days; filler goes back from there, about one thread per 2 hours.
        let jitter = ((h >> 24) % 5400) as i64;
        let date = self.now - 6 * DAY - k * 7_300 - jitter;
        let age = self.now - date;
        let fill = |s: &str| s.replace("{n}", &n.to_string());
        let from = addr(tm.from, tm.account);
        let loc = if tm.category == Category::Spam {
            Loc::Spam
        } else if age > 10 * DAY && (h >> 8) % 10 < 6 {
            Loc::Archive
        } else {
            Loc::Inbox
        };
        let unread = u32::from(if age < 9 * DAY { (h >> 4).is_multiple_of(3) } else { (h >> 4).is_multiple_of(14) });
        let mut participants = vec![from.clone()];
        if replies > 0 {
            participants.push(addr(ME, tm.account));
        }
        let mut row = Row {
            id: ThreadId::new(format!("t{idx}")),
            account: tm.account,
            kind: Kind::Template { t: ti, n, replies },
            subject: fill(tm.subject),
            snippet: snippet_of(&fill(tm.body)),
            from,
            participants,
            date,
            category: tm.category,
            priority: tm.priority,
            summary: fill(tm.summary),
            needs_reply: tm.needs_reply,
            due: None,
            has_attachments: tm.attachment.is_some(),
            labels: Vec::new(),
            message_count: 1 + replies as u32,
            unread,
            flagged: (h >> 12).is_multiple_of(23),
            loc,
            moved: false,
        };
        apply_rules(&mut row, tm.body);
        row
    }

    fn ensure(&self, s: &mut State, upto: usize) {
        let upto = upto.min(self.count);
        while s.rows.len() < upto {
            let idx = s.rows.len();
            let row = self.generate(idx);
            s.rows.push(row);
        }
    }

    /// The row for a thread id, generating it if needed.
    fn find_mut<'a>(&self, s: &'a mut State, id: &ThreadId) -> Option<&'a mut Row> {
        if let Some(pos) = s.fresh.iter().position(|r| &r.id == id) {
            return s.fresh.get_mut(pos);
        }
        let idx: usize = id.as_str().strip_prefix('t')?.parse().ok()?;
        if idx >= self.count {
            return None;
        }
        self.ensure(s, idx + 1);
        s.rows.get_mut(idx).filter(|r| !r.moved)
    }

    fn messages(&self, s: &State, row: &Row) -> Vec<DMsg> {
        let mut out = Vec::new();
        let a = row.account;
        let list_headers = |from: &Address| {
            vec![
                ("List-Unsubscribe".to_string(), format!("<mailto:unsubscribe@{}>", from.domain())),
                ("Authentication-Results".to_string(), format!("mx.example; spf=pass; dkim=pass header.d={}; dmarc=pass", from.domain())),
            ]
        };
        match row.kind {
            Kind::Story(i) => {
                let st = &STORIES[i];
                for (j, msg) in st.msgs.iter().enumerate() {
                    let from = addr(msg.from, a);
                    let headers = if !msg.headers.is_empty() {
                        msg.headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
                    } else if matches!(st.category, Category::Newsletter | Category::Promotion) {
                        list_headers(&from)
                    } else {
                        Vec::new()
                    };
                    out.push(DMsg {
                        to: msg.to.iter().map(|p| addr(*p, a)).collect(),
                        cc: msg.cc.iter().map(|p| addr(*p, a)).collect(),
                        from,
                        date: self.now - msg.mins_ago * 60,
                        subject: if j == 0 { st.subject.into() } else { format!("Re: {}", st.subject) },
                        text: msg.text.into(),
                        html: msg.html.map(str::to_owned),
                        attachments: msg
                            .attachments
                            .iter()
                            .enumerate()
                            .map(|(k, at)| Attachment {
                                id: AttachmentId::new(format!("a{k}")),
                                filename: at.0.into(),
                                mime: at.1.into(),
                                size: at.2,
                                content_id: at.3.map(str::to_owned),
                            })
                            .collect(),
                        headers,
                    });
                }
            }
            Kind::Template { t, n, replies } => {
                let tm = &TEMPLATES[t];
                let fill = |s: &str| s.replace("{n}", &n.to_string());
                let from = addr(tm.from, a);
                let me = addr(ME, a);
                let first_date = row.date - i64::from(replies) * 5_400;
                let headers = if matches!(tm.category, Category::Newsletter | Category::Promotion | Category::Notification) {
                    list_headers(&from)
                } else {
                    Vec::new()
                };
                out.push(DMsg {
                    from: from.clone(),
                    to: vec![me.clone()],
                    cc: vec![],
                    date: first_date,
                    subject: row.subject.clone(),
                    text: fill(tm.body),
                    html: None,
                    attachments: tm
                        .attachment
                        .map(|(name, mime, size)| Attachment {
                            id: AttachmentId::new("a0"),
                            filename: fill(name),
                            mime: mime.into(),
                            size,
                            content_id: None,
                        })
                        .into_iter()
                        .collect(),
                    headers,
                });
                if replies > 0 {
                    out.push(DMsg {
                        from: me,
                        to: vec![from],
                        cc: vec![],
                        date: row.date,
                        subject: format!("Re: {}", row.subject),
                        text: format!("Thanks, noted.\n\nOn an earlier day, {} wrote:\n> {}", tm.from.0,
                                      fill(tm.body).lines().next().unwrap_or("")),
                        html: None,
                        attachments: vec![],
                        headers: vec![],
                    });
                }
            }
            Kind::Fresh => {}
        }
        if let Some(extra) = s.extra.get(&row.id) {
            out.extend(extra.iter().cloned());
        }
        out
    }

    fn summary(&self, row: &Row) -> ThreadSummary {
        let by = match row.category {
            Category::Sensitive | Category::Newsletter | Category::Promotion | Category::Notification | Category::Spam => "rules",
            _ => "demo/local",
        };
        let mut mailboxes = vec![mailbox_id(row.account, match row.loc {
            Loc::Inbox => "inbox",
            Loc::Archive => "archive",
            Loc::Trash => "trash",
            Loc::Spam => "spam",
            Loc::Sent => "sent",
            Loc::Drafts => "drafts",
        })];
        if row.flagged {
            mailboxes.push(mailbox_id(row.account, "starred"));
        }
        for l in &row.labels {
            mailboxes.push(mailbox_id(row.account, &format!("label:{}", l.to_lowercase())));
        }
        ThreadSummary {
            id: row.id.clone(),
            account: account_id(row.account),
            subject: row.subject.clone(),
            snippet: if row.category == Category::Sensitive { String::new() } else { row.snippet.clone() },
            participants: row.participants.clone(),
            message_count: row.message_count,
            unread_count: row.unread,
            date: row.date,
            flagged: row.flagged,
            has_attachments: row.has_attachments,
            mailboxes,
            triage: Some(Triage {
                category: row.category,
                priority: row.priority,
                summary: row.summary.clone(),
                needs_reply: row.needs_reply,
                due: row.due,
                by: by.into(),
                at: self.now - 30,
                suspicious: false,
            }),
        }
    }

    fn emit_changed(&self, account: usize, threads: Vec<ThreadId>) {
        let _ = self.events_tx.try_send(Event::Changed { account: account_id(account), threads });
    }

    fn new_fresh_id(s: &mut State) -> ThreadId {
        s.fresh_counter += 1;
        ThreadId::new(format!("n{}", s.fresh_counter))
    }
}

// ---- queries

#[derive(Default, Debug)]
struct Search {
    words: Vec<String>,
    from: Vec<String>,
    to: Vec<String>,
    subject: Vec<String>,
    has_attachment: bool,
    unread: Option<bool>,
    starred: bool,
    within: Option<String>,
    before: Option<i64>,
    after: Option<i64>,
}

/// `YYYY-MM-DD` to Unix seconds (UTC midnight).
fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split(['-', '/']);
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(crate::time::days_from_civil(y, m, d) * DAY)
}

fn parse_search(text: &str) -> Search {
    let mut s = Search::default();
    for token in text.split_whitespace() {
        let lower = token.to_lowercase();
        let (op, val) = lower.split_once(':').unwrap_or(("", lower.as_str()));
        let val = val.trim_matches('"').to_string();
        match op {
            "from" => s.from.push(val),
            "to" => s.to.push(val),
            "subject" => s.subject.push(val),
            "has" if val.starts_with("attach") => s.has_attachment = true,
            "is" if val == "unread" => s.unread = Some(true),
            "is" if val == "read" => s.unread = Some(false),
            "is" if val == "starred" || val == "flagged" => s.starred = true,
            "in" => s.within = Some(val),
            "before" => s.before = parse_date(&val),
            "after" => s.after = parse_date(&val),
            _ => s.words.push(lower.clone()),
        }
    }
    s
}

fn row_matches_search(row: &Row, s: &Search) -> bool {
    let contains = |hay: &str, needle: &str| hay.to_lowercase().contains(needle);
    let from_hit = |n: &String| row.participants.iter().any(|a| contains(&a.email, n) || contains(a.display(), n));
    s.from.iter().all(from_hit)
        && s.to.iter().all(|n| contains(ACCOUNTS[row.account].email, n) || from_hit(n))
        && s.subject.iter().all(|n| contains(&row.subject, n))
        && (!s.has_attachment || row.has_attachments)
        && s.unread.is_none_or(|u| (row.unread > 0) == u)
        && (!s.starred || row.flagged)
        && s.before.is_none_or(|t| row.date < t)
        && s.after.is_none_or(|t| row.date >= t)
        && s.words.iter().all(|w| {
            contains(&row.subject, w) || contains(&row.summary, w) || contains(&row.snippet, w) || from_hit(w)
        })
}

fn loc_named(name: &str) -> Option<Loc> {
    Some(match name {
        "inbox" => Loc::Inbox,
        "archive" | "all" => Loc::Archive,
        "trash" => Loc::Trash,
        "spam" => Loc::Spam,
        "sent" => Loc::Sent,
        "drafts" => Loc::Drafts,
        _ => return None,
    })
}

fn row_in_box(row: &Row, kind: &str) -> bool {
    match kind {
        "starred" => row.flagged && !matches!(row.loc, Loc::Trash | Loc::Spam),
        "all" => !matches!(row.loc, Loc::Trash | Loc::Spam),
        k if k.starts_with("label:") => {
            let l = &k["label:".len()..];
            row.labels.iter().any(|x| x.to_lowercase() == l) && !matches!(row.loc, Loc::Trash | Loc::Spam)
        }
        k => loc_named(k) == Some(row.loc),
    }
}

fn row_matches(row: &Row, q: &Query, search: Option<&Search>) -> bool {
    let in_accounts = match &q.accounts {
        // The unified view never includes anon accounts.
        None => row.account != ANON,
        Some(ids) => ids.iter().any(|id| id.as_str() == ACCOUNTS[row.account].id),
    };
    if !in_accounts {
        return false;
    }
    let placed = if let Some(mb) = &q.mailbox {
        match mb.as_str().split_once(':') {
            Some((acct, kind)) => acct == ACCOUNTS[row.account].id && row_in_box(row, kind),
            None => false,
        }
    } else if let Some(role) = q.role {
        match role {
            MailboxRole::Inbox => row.loc == Loc::Inbox,
            MailboxRole::Sent => row.loc == Loc::Sent,
            MailboxRole::Drafts => row.loc == Loc::Drafts,
            MailboxRole::Archive => row.loc == Loc::Archive,
            MailboxRole::Trash => row.loc == Loc::Trash,
            MailboxRole::Spam => row.loc == Loc::Spam,
            MailboxRole::Starred => row_in_box(row, "starred"),
            MailboxRole::Important => row.category == Category::Important && row.loc == Loc::Inbox,
            MailboxRole::All | MailboxRole::Custom => row_in_box(row, "all"),
        }
    } else if let Some(within) = search.and_then(|s| s.within.as_deref()) {
        row_in_box(row, within)
    } else {
        !matches!(row.loc, Loc::Trash | Loc::Spam | Loc::Drafts)
    };
    placed
        && q.category.is_none_or(|c| row.category == c)
        && (!q.unread_only || row.unread > 0)
        && search.is_none_or(|s| row_matches_search(row, s))
}

/// The made-up content of a demo attachment: a drawn picture for images, a short note for the rest.
fn demo_file_bytes(att: &Attachment) -> Vec<u8> {
    if att.mime.starts_with("image/") {
        let seed = att.filename.bytes().fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)));
        if att.content_id.is_some() {
            return floor_plan_png();
        }
        return crate::remote::Painted::png(480, 320, seed);
    }
    format!("Made-up demo file: {} ({}). Lumbridge Mail demo data, not a real document.\n", att.filename, att.mime).into_bytes()
}

/// A simple drawn floor plan for the inline-image story.
fn floor_plan_png() -> Vec<u8> {
    let (w, h) = (560u32, 320u32);
    let rooms: [(u32, u32, u32, u32); 4] = [(20, 20, 300, 180), (320, 20, 540, 180), (20, 180, 220, 300), (220, 180, 540, 300)];
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        let wall = rooms.iter().any(|&(x0, y0, x1, y1)| {
            let on_x = (x == x0 || x == x1 || x + 1 == x0 || x == x1 + 1) && (y0..=y1).contains(&y);
            let on_y = (y == y0 || y == y1 || y + 1 == y0 || y == y1 + 1) && (x0..=x1).contains(&x);
            on_x || on_y
        });
        // Doors: gaps in the walls.
        let door = (x > 140 && x < 180 && (179..=182).contains(&y)) || (y > 80 && y < 120 && (319..=322).contains(&x))
            || (x > 360 && x < 400 && (179..=182).contains(&y));
        if wall && !door {
            image::Rgba([40, 48, 60, 255])
        } else if rooms.iter().any(|&(x0, y0, x1, y1)| x > x0 && x < x1 && y > y0 && y < y1) {
            let shade = if (x / 20 + y / 20) % 2 == 0 { 246 } else { 240 };
            image::Rgba([shade, shade - 4, shade - 12, 255])
        } else {
            image::Rgba([255, 255, 255, 255])
        }
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).expect("encode png");
    out.into_inner()
}

fn first_name(a: &Address) -> String {
    a.name.as_deref().and_then(|n| n.split_whitespace().next()).unwrap_or("there").to_string()
}

/// Turn "say yes, Saturday works" into a short reply. Canned: the real drafting is the AI layer's.
fn canned_reply(instruction: &str, to: Option<&Address>, me: &str) -> String {
    let greeting = to.map(first_name).map(|n| format!("Hi {n},")).unwrap_or_else(|| "Hi,".into());
    let mut said = instruction.trim().trim_end_matches(['.', '!']).to_string();
    for lead in ["say ", "tell them ", "tell him ", "tell her ", "reply ", "write ", "let them know ", "answer "] {
        if said.to_lowercase().starts_with(lead) {
            said = said[lead.len()..].trim().to_string();
            break;
        }
    }
    let lower = said.to_lowercase();
    let tail = |prefix_len: usize| {
        let rest = said[prefix_len..].trim_start_matches([',', ';', ':', ' ', '-']).trim();
        if rest.is_empty() { String::new() } else { format!(" {}.", capitalise(rest)) }
    };
    let body = if lower == "yes" || lower.starts_with("yes ") || lower.starts_with("yes,") {
        format!("Thanks for sorting this out, that sounds great.{}", tail(3))
    } else if lower == "no" || lower.starts_with("no ") || lower.starts_with("no,") {
        format!("Thanks for thinking of me. I'm afraid I can't this time.{}", tail(2))
    } else if said.is_empty() {
        "Thanks for your note. I'll get back to you properly soon.".to_string()
    } else {
        format!("Thanks for your note. {}.", capitalise(&said))
    };
    format!("{greeting}\n\n{body}\n\nBest,\n{me}")
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

const ARRIVALS: &[(usize, P, &str, &str, Category)] = &[
    (PERSONAL, MAYA, "Running late tonight", "Maya will be about 15 minutes late tonight.", Category::Personal),
    (WORK, AIKO, "Retry fix merged", "Aiko merged the webhook retry fix; CI is green.", Category::Important),
    (PERSONAL, P("Parcelly", "orders@parcelly.example"), "Out for delivery", "Your Parcelly parcel is out for delivery today.", Category::Transactional),
    (WORK, DANA, "Launch moved to 11:00", "Dana moved Thursday's launch to 11:00.", Category::Important),
];

#[async_trait]
impl Backend for Demo {
    fn name(&self) -> &str {
        "Demo"
    }

    async fn hello(&self) -> Result<Hello> {
        Ok(Hello { protocol: lumbridge_mail_core::rpc::PROTOCOL_VERSION, version: "demo".into(), person: true })
    }

    async fn accounts(&self) -> Result<Vec<Account>> {
        Ok((0..ACCOUNTS.len()).map(demo_account).collect())
    }

    async fn mailboxes(&self, account: Option<AccountId>) -> Result<Vec<Mailbox>> {
        let mut s = self.state.lock();
        self.ensure(&mut s, self.count);
        let accounts: Vec<usize> = match &account {
            Some(id) => vec![account_index(id).ok_or_else(|| Error::NotFound(id.to_string()))?],
            None => (0..ACCOUNTS.len()).collect(),
        };
        let mut out = Vec::new();
        for a in accounts {
            let mut boxes: Vec<(String, String, MailboxRole)> = vec![
                ("inbox".into(), "Inbox".into(), MailboxRole::Inbox),
                ("starred".into(), "Starred".into(), MailboxRole::Starred),
                ("sent".into(), "Sent".into(), MailboxRole::Sent),
                ("drafts".into(), "Drafts".into(), MailboxRole::Drafts),
                ("archive".into(), "Archive".into(), MailboxRole::Archive),
                ("spam".into(), "Spam".into(), MailboxRole::Spam),
                ("trash".into(), "Trash".into(), MailboxRole::Trash),
            ];
            for l in ACCOUNTS[a].labels {
                boxes.push((format!("label:{}", l.to_lowercase()), (*l).into(), MailboxRole::Custom));
            }
            for (kind, name, role) in boxes {
                let (mut unread, mut total) = (0u32, 0u32);
                for row in s.fresh.iter().chain(s.rows.iter().filter(|r| !r.moved)) {
                    if row.account == a && row_in_box(row, &kind) {
                        total += 1;
                        unread += u32::from(row.unread > 0);
                    }
                }
                out.push(Mailbox { id: mailbox_id(a, &kind), account: account_id(a), name, role, unread, total });
            }
        }
        Ok(out)
    }

    async fn threads(&self, query: Query) -> Result<ThreadPage> {
        let limit = if query.limit == 0 { 100 } else { query.limit as usize };
        let start: usize = query.page.as_deref().and_then(|p| p.parse().ok()).unwrap_or(0);
        let search = query.text.as_deref().filter(|t| !t.trim().is_empty()).map(parse_search);
        let mut s = self.state.lock();
        let mut out = Vec::with_capacity(limit);
        let mut pos = start;
        let total = s.fresh.len() + self.count;
        while pos < total && out.len() < limit {
            if pos < s.fresh.len() {
                let row = &s.fresh[pos];
                if row_matches(row, &query, search.as_ref()) {
                    out.push(self.summary(row));
                }
            } else {
                let idx = pos - s.fresh.len();
                // Generate in chunks so a long scan does not grow the vector one row at a time.
                if idx >= s.rows.len() {
                    self.ensure(&mut s, idx + 512);
                }
                let row = &s.rows[idx];
                if !row.moved && row_matches(row, &query, search.as_ref()) {
                    out.push(self.summary(row));
                }
            }
            pos += 1;
        }
        Ok(ThreadPage { threads: out, next: (pos < total).then(|| pos.to_string()) })
    }

    async fn thread(&self, thread: ThreadRef) -> Result<Vec<MessageMeta>> {
        let mut s = self.state.lock();
        let row = self.find_mut(&mut s, &thread.thread).ok_or_else(|| Error::NotFound(thread.thread.to_string()))?.clone();
        let msgs = self.messages(&s, &row);
        let n = msgs.len();
        Ok(msgs
            .into_iter()
            .enumerate()
            .map(|(j, m)| MessageMeta {
                id: MessageId::new(format!("{}.{j}", row.id)),
                account: account_id(row.account),
                thread: row.id.clone(),
                mailboxes: self.summary(&row).mailboxes,
                from: vec![m.from],
                to: m.to,
                cc: m.cc,
                bcc: vec![],
                subject: m.subject,
                snippet: snippet_of(&m.text),
                date: m.date,
                flags: Flags { seen: j + (row.unread as usize) < n, flagged: row.flagged && j + 1 == n, answered: false,
                               draft: row.loc == Loc::Drafts },
                has_attachments: !m.attachments.is_empty(),
                size: m.text.len() as u64 + m.attachments.iter().map(|a| a.size).sum::<u64>(),
                rfc_message_id: Some(format!("{}.{j}@demo.example", row.id)),
                in_reply_to: (j > 0).then(|| format!("{}.{}@demo.example", row.id, j - 1)),
                references: (0..j).map(|k| format!("{}.{k}@demo.example", row.id)).collect(),
                partial: false,
            })
            .collect())
    }

    async fn body(&self, message: MessageRef) -> Result<Body> {
        let (tid, j) = message
            .message
            .as_str()
            .rsplit_once('.')
            .and_then(|(t, j)| Some((ThreadId::new(t), j.parse::<usize>().ok()?)))
            .ok_or_else(|| Error::NotFound(message.message.to_string()))?;
        let mut s = self.state.lock();
        let row = self.find_mut(&mut s, &tid).ok_or_else(|| Error::NotFound(tid.to_string()))?.clone();
        let m = self.messages(&s, &row).into_iter().nth(j).ok_or_else(|| Error::NotFound(message.message.to_string()))?;
        Ok(Body { text: Some(m.text), html: m.html, attachments: m.attachments, headers: m.headers })
    }

    async fn attachment(&self, r: AttachmentRef) -> Result<PathBuf> {
        let body = self.body(MessageRef { account: r.account.clone(), message: r.message.clone() }).await?;
        let att = body.attachments.iter().find(|a| a.id == r.attachment)
            .ok_or_else(|| Error::NotFound(r.attachment.to_string()))?;
        // Made-up bytes, written under the temp directory like the daemon's attachment cache.
        let dir = std::env::temp_dir().join(format!("lumbridge-mail-demo-{}", std::process::id())).join(r.message.as_str());
        std::fs::create_dir_all(&dir).map_err(|e| Error::Other(e.to_string()))?;
        let name: String = att.filename.chars().map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect();
        let path = dir.join(name);
        let bytes = demo_file_bytes(att);
        std::fs::write(&path, bytes).map_err(|e| Error::Other(e.to_string()))?;
        Ok(path)
    }

    async fn contacts(&self, account: AccountId) -> Result<Vec<Address>> {
        let a = account_index(&account).ok_or_else(|| Error::NotFound(account.to_string()))?;
        let mut out: Vec<Address> = Vec::new();
        let mut add = |p: P| {
            let addr = addr(p, a);
            if addr.email != ACCOUNTS[a].email && !out.contains(&addr) {
                out.push(addr);
            }
        };
        for st in STORIES.iter().filter(|s| s.account == a) {
            for m in st.msgs {
                add(m.from);
                m.to.iter().chain(m.cc.iter()).for_each(|p| add(*p));
            }
        }
        for t in TEMPLATES.iter().filter(|t| t.account == a) {
            add(t.from);
        }
        Ok(out)
    }

    async fn act(&self, act: Act) -> Result<u64> {
        let a = account_index(&act.account).ok_or_else(|| Error::NotFound(act.account.to_string()))?;
        let thread_of = |id: &MessageId| ThreadId::new(id.as_str().rsplit_once('.').map(|(t, _)| t).unwrap_or(id.as_str()));
        let mut changed: Vec<ThreadId> = Vec::new();
        let mut s = self.state.lock();
        match act.action {
            Action::SetSeen { ref ids, seen } | Action::SetFlagged { ref ids, flagged: seen } => {
                let is_seen = matches!(act.action, Action::SetSeen { .. });
                let threads: BTreeSet<ThreadId> = ids.iter().map(thread_of).collect();
                for t in threads {
                    if let Some(row) = self.find_mut(&mut s, &t) {
                        if is_seen {
                            row.unread = if seen { 0 } else { row.unread.max(1) };
                        } else {
                            row.flagged = seen;
                        }
                        changed.push(t);
                    }
                }
            }
            Action::Archive { ref ids } | Action::Trash { ref ids } => {
                let to = if matches!(act.action, Action::Archive { .. }) { Loc::Archive } else { Loc::Trash };
                let threads: BTreeSet<ThreadId> = ids.iter().map(thread_of).collect();
                for t in threads {
                    if let Some(row) = self.find_mut(&mut s, &t) {
                        row.loc = to;
                        changed.push(t);
                    }
                }
            }
            Action::Move { ref ids, ref to, .. } => {
                let kind = to.as_str().split_once(':').map(|(_, k)| k.to_string()).unwrap_or_default();
                let threads: BTreeSet<ThreadId> = ids.iter().map(thread_of).collect();
                for t in threads {
                    if let Some(row) = self.find_mut(&mut s, &t) {
                        if let Some(l) = kind.strip_prefix("label:") {
                            if !row.labels.iter().any(|x| x.to_lowercase() == l) {
                                row.labels.push(capitalise(l));
                            }
                        } else if let Some(loc) = loc_named(&kind) {
                            row.loc = loc;
                        } else {
                            return Err(Error::NotFound(to.to_string()));
                        }
                        changed.push(t);
                    }
                }
            }
            Action::SaveDraft { ref draft } => {
                let existing = s.drafts.contains_key(&draft.local_id);
                let id = ThreadId::new(format!("d-{}", draft.local_id));
                if !existing {
                    let me = addr(ME, a);
                    let row = Row {
                        id: id.clone(), account: a, kind: Kind::Fresh, subject: draft.subject.clone(),
                        snippet: snippet_of(&draft.text), from: me.clone(), participants: vec![me], date: self.now,
                        category: Category::Other, priority: 0, summary: "Your draft".into(), needs_reply: false,
                        due: None, has_attachments: !draft.attachments.is_empty(), labels: vec![], message_count: 1,
                        unread: 0, flagged: false, loc: Loc::Drafts, moved: false,
                    };
                    s.fresh.insert(0, row);
                } else if let Some(row) = s.fresh.iter_mut().find(|r| r.id == id) {
                    row.subject = draft.subject.clone();
                    row.snippet = snippet_of(&draft.text);
                }
                s.drafts.insert(draft.local_id.clone(), (a, draft.clone()));
                changed.push(id);
            }
            Action::DeleteDraft { ref local_id, .. } => {
                s.drafts.remove(local_id);
                let id = ThreadId::new(format!("d-{local_id}"));
                s.fresh.retain(|r| r.id != id);
                changed.push(id);
            }
            Action::Send { ref draft } => {
                // The same rule the daemon enforces: only a person sends.
                if act.caller != Caller::Person {
                    return Err(Error::Refused(safety::SEND_NEEDS_A_PERSON.into()));
                }
                if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
                    return Err(Error::Invalid("a message needs at least one recipient".into()));
                }
                s.drafts.remove(&draft.local_id);
                let draft_id = ThreadId::new(format!("d-{}", draft.local_id));
                s.fresh.retain(|r| r.id != draft_id);
                let me = addr(ME, a);
                let msg = DMsg {
                    from: me.clone(), to: draft.to.clone(), cc: draft.cc.clone(), date: self.now,
                    subject: draft.subject.clone(), text: draft.text.clone(), html: draft.html.clone(),
                    attachments: draft.attachments.iter().enumerate().map(|(k, d)| Attachment {
                        id: AttachmentId::new(format!("a{k}")), filename: d.filename.clone(), mime: d.mime.clone(),
                        size: 0, content_id: None }).collect(),
                    headers: vec![],
                };
                let target = draft.reply_to.as_ref().map(thread_of);
                let replied = target.as_ref().and_then(|t| self.find_mut(&mut s, t).map(|r| r.clone()));
                if let Some(mut row) = replied {
                    // Move the thread to the top with the reply on the end.
                    if let Some(orig) = self.find_mut(&mut s, &row.id) {
                        orig.moved = true;
                    }
                    s.fresh.retain(|r| r.id != row.id);
                    row.moved = false;
                    row.date = self.now;
                    row.message_count += 1;
                    row.snippet = snippet_of(&draft.text);
                    if !row.participants.contains(&me) {
                        row.participants.push(me);
                    }
                    row.needs_reply = false;
                    s.extra.entry(row.id.clone()).or_default().push(msg);
                    changed.push(row.id.clone());
                    s.fresh.insert(0, row);
                } else {
                    let id = Self::new_fresh_id(&mut s);
                    let mut participants = vec![me.clone()];
                    participants.extend(draft.to.iter().cloned());
                    let row = Row {
                        id: id.clone(), account: a, kind: Kind::Fresh, subject: draft.subject.clone(),
                        snippet: snippet_of(&draft.text), from: me, participants, date: self.now,
                        category: Category::Personal, priority: 0, summary: format!("You wrote to {}.",
                            draft.to.first().map(|t| t.display().to_string()).unwrap_or_default()),
                        needs_reply: false, due: None, has_attachments: !draft.attachments.is_empty(), labels: vec![],
                        message_count: 1, unread: 0, flagged: false, loc: Loc::Sent, moved: false,
                    };
                    s.extra.insert(id.clone(), vec![msg]);
                    s.fresh.insert(0, row);
                    changed.push(id);
                }
            }
        }
        if s.offline {
            s.queued[a] += 1;
        }
        s.act_counter += 1;
        let id = s.act_counter;
        let statuses = self.statuses(&s);
        drop(s);
        self.emit_changed(a, changed);
        // Like the daemon: every act is followed by the account's status (queued, online).
        let _ = self.events_tx.try_send(Event::Status { status: statuses[a].clone() });
        Ok(id)
    }

    async fn sync(&self, account: Option<AccountId>) -> Result<()> {
        let (arrival, statuses) = {
            let mut s = self.state.lock();
            if s.offline {
                return Err(Error::Offline("no network (demo)".into()));
            }
            s.queued = [0; 3];
            s.last_sync = self.now;
            let k = s.arrivals;
            let wanted = account.as_ref().and_then(account_index);
            let arrival = ARRIVALS
                .iter()
                .cycle()
                .skip(k % ARRIVALS.len())
                .take(ARRIVALS.len())
                .find(|(a, ..)| wanted.is_none_or(|w| w == *a));
            let made = arrival.map(|(a, from, subject, summary, category)| {
                s.arrivals += 1;
                let id = Self::new_fresh_id(&mut s);
                let from = addr(*from, *a);
                let text = format!("{summary}\n\n{}", first_name(&from));
                let row = Row {
                    id: id.clone(), account: *a, kind: Kind::Fresh, subject: (*subject).into(),
                    snippet: snippet_of(&text), from: from.clone(), participants: vec![from.clone()], date: self.now,
                    category: *category, priority: 60, summary: (*summary).into(), needs_reply: false, due: None,
                    has_attachments: false, labels: vec![], message_count: 1, unread: 1, flagged: false,
                    loc: Loc::Inbox, moved: false,
                };
                s.extra.insert(id.clone(), vec![DMsg {
                    from: from.clone(), to: vec![addr(ME, *a)], cc: vec![], date: self.now, subject: (*subject).into(),
                    text, html: None, attachments: vec![], headers: vec![] }]);
                s.fresh.insert(0, row);
                (*a, id, from.display().to_string(), subject.to_string())
            });
            (made, self.statuses(&s))
        };
        for status in statuses {
            let _ = self.events_tx.try_send(Event::Status { status });
        }
        if let Some((a, id, from, subject)) = arrival {
            let _ = self.events_tx.try_send(Event::NewMail { account: account_id(a), thread: id.clone(), from, subject });
            self.emit_changed(a, vec![id]);
        }
        Ok(())
    }

    async fn status(&self) -> Result<Vec<AccountStatus>> {
        Ok(self.statuses(&self.state.lock()))
    }

    async fn triage(&self, thread: ThreadRef) -> Result<Triage> {
        let mut s = self.state.lock();
        let row = self.find_mut(&mut s, &thread.thread).ok_or_else(|| Error::NotFound(thread.thread.to_string()))?.clone();
        Ok(self.summary(&row).triage.expect("demo rows are triaged"))
    }

    async fn summarise(&self, thread: ThreadRef) -> Result<String> {
        let mut s = self.state.lock();
        let row = self.find_mut(&mut s, &thread.thread).ok_or_else(|| Error::NotFound(thread.thread.to_string()))?.clone();
        if row.category == Category::Sensitive {
            return Err(Error::Refused("sensitive mail is never given to a model".into()));
        }
        Ok(match row.kind {
            Kind::Story(i) if !STORIES[i].ai_summary.is_empty() => STORIES[i].ai_summary.into(),
            _ if row.message_count > 1 => format!("{} {} messages; the latest is from {}.", row.summary,
                                                   row.message_count, row.from.display()),
            _ => row.summary.clone(),
        })
    }

    async fn draft(&self, request: DraftRequest) -> Result<Draft> {
        let a = account_index(&request.account).ok_or_else(|| Error::NotFound(request.account.to_string()))?;
        let me = ACCOUNTS[a].name.split_whitespace().next().unwrap_or("Me").to_string();
        let mut draft = Draft { local_id: format!("ai-{}", self.now), ..Default::default() };
        if let Some(t) = &request.thread {
            let mut s = self.state.lock();
            let row = self.find_mut(&mut s, t).ok_or_else(|| Error::NotFound(t.to_string()))?.clone();
            if row.category == Category::Sensitive {
                return Err(Error::Refused("sensitive mail is never given to a model".into()));
            }
            let them = self.messages(&s, &row).into_iter().rev().map(|m| m.from).find(|f| f.email != ACCOUNTS[a].email);
            draft.subject = if row.subject.starts_with("Re:") { row.subject.clone() } else { format!("Re: {}", row.subject) };
            draft.text = canned_reply(&request.instruction, them.as_ref(), &me);
            draft.to = them.into_iter().collect();
        } else {
            draft.text = canned_reply(&request.instruction, None, &me);
        }
        Ok(draft)
    }

    async fn ask(&self, question: String, accounts: Option<Vec<AccountId>>) -> Result<Answer> {
        const STOP: &[&str] = &["what", "when", "where", "who", "is", "the", "a", "an", "my", "me", "do", "i", "to",
                                "about", "of", "for", "in", "on", "did", "does", "have", "any", "and", "with", "from"];
        let words: Vec<String> = question
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2 && !STOP.contains(w))
            .map(str::to_owned)
            .collect();
        let q = Query { accounts: accounts.clone(), ..Default::default() };
        let mut s = self.state.lock();
        self.ensure(&mut s, self.count.min(2000));
        let mut scored: Vec<(usize, &Row)> = s
            .fresh
            .iter()
            .chain(s.rows.iter().filter(|r| !r.moved))
            .filter(|r| row_matches(r, &q, None) && r.category != Category::Sensitive)
            .map(|r| {
                let hay = format!("{} {} {}", r.subject, r.summary, r.from.display()).to_lowercase();
                (words.iter().filter(|w| hay.contains(w.as_str())).count(), r)
            })
            .filter(|(n, _)| *n > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.date.cmp(&a.1.date)));
        scored.truncate(3);
        if scored.is_empty() {
            return Ok(Answer { text: "I couldn't find anything about that in your mail.".into(), sources: vec![] });
        }
        let mut text = String::from("From your mail:");
        for (_, r) in &scored {
            text.push_str(&format!("\n• {} ({}, {})", r.summary, r.from.display(),
                                   crate::time::short_date(r.date, self.now)));
        }
        Ok(Answer {
            text,
            sources: scored.iter().map(|(_, r)| ThreadRef { account: account_id(r.account), thread: r.id.clone() }).collect(),
        })
    }

    async fn extract(&self, thread: ThreadRef, triage: Option<Triage>) -> Result<Extracted> {
        let mut s = self.state.lock();
        let row = self.find_mut(&mut s, &thread.thread).ok_or_else(|| Error::NotFound(thread.thread.to_string()))?.clone();
        if row.category == Category::Sensitive {
            // The daemon refuses these too: sensitive mail never goes to a model.
            return Err(Error::Refused("sensitive mail is never given to a model".into()));
        }
        let items = match row.kind {
            Kind::Story(i) => STORIES[i]
                .actions
                .iter()
                .map(|(text, due)| ActionItem { text: (*text).into(), owner: Owner::Me, due: due.map(|d| self.now + d * DAY) })
                .collect(),
            _ => super::items_from_triage(triage.as_ref()),
        };
        Ok(Extracted { items, dates: Vec::new(), by: "demo".into() })
    }

    async fn counts(&self, request: CountsRequest) -> Result<Vec<CategoryCount>> {
        // The same anon wall as the daemon: None = every account but anon.
        let accounts = request.accounts.unwrap_or_else(|| (0..3).filter(|i| *i != ANON).map(account_id).collect());
        let page = self.threads(Query { accounts: Some(accounts), role: request.role,
                                             limit: (self.count + 1000) as u32, ..Default::default() }).await?;
        let mut out: Vec<CategoryCount> = Vec::new();
        for t in page.threads {
            let Some(c) = t.triage.as_ref().map(|t| t.category) else { continue };
            let row = match out.iter_mut().position(|x| x.category == c) {
                Some(i) => &mut out[i],
                None => {
                    out.push(CategoryCount { category: c, threads: 0, unread: 0 });
                    out.last_mut().expect("just pushed")
                }
            };
            row.threads += 1;
            row.unread += u32::from(t.unread_count > 0);
        }
        Ok(out)
    }

    fn events(&self) -> async_channel::Receiver<Event> {
        self.events_rx.clone()
    }

    fn as_demo(&self) -> Option<&Demo> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    const NOW: i64 = 1_793_000_000;

    fn demo() -> Demo {
        Demo::new(NOW, DEFAULT_THREADS)
    }

    fn inbox() -> Query {
        Query { role: Some(MailboxRole::Inbox), limit: 500, ..Default::default() }
    }

    #[test]
    fn content_is_made_up() {
        for a in ACCOUNTS.iter() {
            assert!(a.email.ends_with(".example"), "{}", a.email);
        }
        let d = demo();
        let all = block_on(d.threads(Query { accounts: Some((0..3).map(account_id).collect()), limit: 10_000, ..Default::default() })).unwrap();
        for t in &all.threads {
            for p in &t.participants {
                assert!(p.email.ends_with(".example"), "{}", p.email);
            }
        }
    }

    #[test]
    fn unified_inbox_never_includes_anon() {
        let d = demo();
        let page = block_on(d.threads(inbox())).unwrap();
        assert!(page.threads.len() > 50);
        assert!(page.threads.iter().all(|t| t.account.as_str() != "anon"));
        let search = block_on(d.threads(Query { text: Some("synth".into()), limit: 500, ..Default::default() })).unwrap();
        assert!(search.threads.is_empty(), "cross-account search must not reach anon");
        let anon = block_on(d.threads(Query { accounts: Some(vec![AccountId::new("anon")]), text: Some("synth".into()), limit: 500, ..Default::default() })).unwrap();
        assert!(!anon.threads.is_empty(), "naming the anon account searches it");
        let answer = block_on(d.ask("synth listing offer".into(), None)).unwrap();
        assert!(answer.sources.iter().all(|s| s.account.as_str() != "anon"));
    }

    #[test]
    fn sensitive_mail_is_ruled_and_never_summarised() {
        let d = demo();
        let page = block_on(d.threads(inbox())).unwrap();
        let code = page.threads.iter().find(|t| t.subject == "Your sign-in code").unwrap();
        let tr = code.triage.as_ref().unwrap();
        assert_eq!(tr.category, Category::Sensitive);
        assert_eq!(tr.summary, "A security code from Brightdesk");
        assert!(code.snippet.is_empty(), "the code must not show in the list");
        let r = ThreadRef { account: code.account.clone(), thread: code.id.clone() };
        assert!(matches!(block_on(d.summarise(r.clone())), Err(Error::Refused(_))));
        let req = DraftRequest { account: code.account.clone(), thread: Some(code.id.clone()), instruction: "reply".into(),
                                 reply_all: false, extra_cc: vec![] };
        assert!(matches!(block_on(d.draft(req)), Err(Error::Refused(_))));
        // Every generated thread went through the same rule.
        let all = block_on(d.threads(Query { accounts: Some((0..3).map(account_id).collect()), limit: 10_000, ..Default::default() })).unwrap();
        for t in &all.threads {
            let from = &t.participants[0];
            if safety::sensitive_kind(&from.email, &t.subject, "").is_some() {
                assert_eq!(t.triage.as_ref().unwrap().category, Category::Sensitive, "{}", t.subject);
            }
        }
    }

    #[test]
    fn send_needs_a_person_and_shows_up_in_sent() {
        let d = demo();
        let draft = Draft { local_id: "x1".into(), to: vec![Address::new(Some("Maya Chen"), "maya.chen@postbox.example")],
                            subject: "Hello".into(), text: "Hi Maya".into(), ..Default::default() };
        let agent = Act { account: AccountId::new("personal"), action: Action::Send { draft: draft.clone() }, caller: Caller::Agent };
        assert_eq!(block_on(d.act(agent)), Err(Error::Refused(safety::SEND_NEEDS_A_PERSON.into())));
        let person = Act { account: AccountId::new("personal"), action: Action::Send { draft }, caller: Caller::Person };
        block_on(d.act(person)).unwrap();
        let sent = block_on(d.threads(Query { role: Some(MailboxRole::Sent), ..Default::default() })).unwrap();
        assert_eq!(sent.threads[0].subject, "Hello");
    }

    #[test]
    fn replies_join_their_thread_and_move_it_to_the_top() {
        let d = demo();
        let page = block_on(d.threads(inbox())).unwrap();
        let dinner = page.threads.iter().find(|t| t.subject == "Dinner Saturday?").unwrap().clone();
        let r = ThreadRef { account: dinner.account.clone(), thread: dinner.id.clone() };
        let msgs = block_on(d.thread(r.clone())).unwrap();
        let draft = Draft { local_id: "r1".into(), to: vec![Address::new(None, "maya.chen@postbox.example")],
                            subject: "Re: Dinner Saturday?".into(), text: "Saturday is perfect.".into(),
                            reply_to: Some(msgs.last().unwrap().id.clone()), ..Default::default() };
        block_on(d.act(Act { account: dinner.account.clone(), action: Action::Send { draft }, caller: Caller::Person })).unwrap();
        let after = block_on(d.thread(r)).unwrap();
        assert_eq!(after.len(), msgs.len() + 1);
        let page = block_on(d.threads(inbox())).unwrap();
        assert_eq!(page.threads[0].id, dinner.id);
        assert_eq!(page.threads.iter().filter(|t| t.id == dinner.id).count(), 1);
    }

    #[test]
    fn archive_star_and_read_change_the_lists_and_raise_events() {
        let d = demo();
        let events = d.events();
        let first = block_on(d.threads(inbox())).unwrap().threads[0].clone();
        let r = ThreadRef { account: first.account.clone(), thread: first.id.clone() };
        let ids: Vec<MessageId> = block_on(d.thread(r)).unwrap().into_iter().map(|m| m.id).collect();
        let act = |action| Act { account: first.account.clone(), action, caller: Caller::Person };
        block_on(d.act(act(Action::SetSeen { ids: ids.clone(), seen: true }))).unwrap();
        block_on(d.act(act(Action::SetFlagged { ids: ids.clone(), flagged: true }))).unwrap();
        block_on(d.act(act(Action::Archive { ids: ids.clone() }))).unwrap();
        assert!(matches!(block_on(events.recv()).unwrap(), Event::Changed { .. }));
        let inbox_now = block_on(d.threads(inbox())).unwrap();
        assert!(inbox_now.threads.iter().all(|t| t.id != first.id));
        let archived = block_on(d.threads(Query { mailbox: Some(MailboxId::new(format!("{}:archive", first.account))), limit: 1000, ..Default::default() })).unwrap();
        let t = archived.threads.iter().find(|t| t.id == first.id).unwrap();
        assert_eq!(t.unread_count, 0);
        assert!(t.flagged);
        // Undo is a move back to the inbox.
        block_on(d.act(act(Action::Move { ids, from: None, to: MailboxId::new(format!("{}:inbox", first.account)) }))).unwrap();
        assert!(block_on(d.threads(inbox())).unwrap().threads.iter().any(|t| t.id == first.id));
    }

    #[test]
    fn offline_queues_actions_and_sync_delivers_mail() {
        let d = demo();
        d.set_offline(true);
        let first = block_on(d.threads(inbox())).unwrap().threads[0].clone();
        let n = block_on(d.act(Act { account: first.account.clone(), caller: Caller::Person,
            action: Action::SetFlagged { ids: vec![MessageId::new(format!("{}.0", first.id))], flagged: true } })).unwrap();
        assert!(n > 0, "a queue id");
        assert!(matches!(block_on(d.sync(None)), Err(Error::Offline(_))));
        let st = block_on(d.status()).unwrap();
        assert!(st.iter().all(|s| !s.online));
        assert_eq!(st.iter().find(|s| s.account == first.account).unwrap().queued, 1, "the queue is in the status");
        d.set_offline(false);
        block_on(d.sync(None)).unwrap();
        let st = block_on(d.status()).unwrap();
        assert!(st.iter().all(|s| s.online && s.queued == 0));
        let top = &block_on(d.threads(inbox())).unwrap().threads[0];
        assert_eq!(top.subject, ARRIVALS[0].2);
        assert_eq!(top.unread_count, 1);
    }

    #[test]
    fn counts_follow_the_anon_wall() {
        let d = demo();
        let all = block_on(d.counts(CountsRequest::default())).unwrap();
        let total: u32 = all.iter().map(|c| c.threads).sum();
        let unified = block_on(d.threads(Query { limit: 100_000, ..Default::default() })).unwrap().threads;
        assert_eq!(total as usize, unified.iter().filter(|t| t.triage.is_some()).count(), "anon is not counted");
        assert!(all.iter().all(|c| c.unread <= c.threads));
        let anon = block_on(d.counts(CountsRequest { accounts: Some(vec![account_id(ANON)]), role: None })).unwrap();
        assert!(anon.iter().map(|c| c.threads).sum::<u32>() > 0, "named, anon is counted");
    }

    #[test]
    fn search_operators() {
        let d = demo();
        let q = |text: &str| block_on(d.threads(Query { text: Some(text.into()), limit: 2000, ..Default::default() })).unwrap().threads;
        assert!(q("from:dana").iter().all(|t| t.participants.iter().any(|p| p.email.contains("dana"))));
        assert!(!q("from:dana").is_empty());
        assert!(q("has:attachment").iter().all(|t| t.has_attachments));
        assert!(q("is:unread").iter().all(|t| t.unread_count > 0));
        assert!(q("is:starred").iter().all(|t| t.flagged));
        assert_eq!(q("subject:\"dinner\"").len(), 1);
        assert!(q("in:trash").is_empty());
        assert!(q("atlas launch").iter().any(|t| t.subject.contains("go/no-go")));
        let old = q("before:2026-01-01");
        assert!(old.iter().all(|t| t.date < parse_date("2026-01-01").unwrap()));
        assert_eq!(parse_date("2026-11-10"), Some(1_794_268_800));
    }

    #[test]
    fn fifty_thousand_threads_page_quickly() {
        let d = Demo::new(NOW, 50_000);
        let started = std::time::Instant::now();
        let mut q = Query { role: Some(MailboxRole::Inbox), limit: 200, ..Default::default() };
        let first = block_on(d.threads(q.clone())).unwrap();
        assert_eq!(first.threads.len(), 200);
        q.page = first.next.clone();
        let second = block_on(d.threads(q)).unwrap();
        assert_ne!(first.threads[0].id, second.threads[0].id);
        // Only what was asked for is generated.
        assert!(d.state.lock().rows.len() < 2_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        // A query that matches little walks everything once, still fast.
        let started = std::time::Instant::now();
        let rare = block_on(d.threads(Query { text: Some("subject:invoice".into()), limit: 50, ..Default::default() })).unwrap();
        assert!(!rare.threads.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn mailboxes_count_unread() {
        let d = demo();
        let boxes = block_on(d.mailboxes(None)).unwrap();
        let inbox = boxes.iter().find(|m| m.id.as_str() == "work:inbox").unwrap();
        assert!(inbox.unread > 0 && inbox.total >= inbox.unread);
        assert!(boxes.iter().any(|m| m.id.as_str() == "anon:label:forums"));
    }

    fn body_of(d: &Demo, subject: &str) -> (Body, MessageMeta) {
        let page = block_on(d.threads(Query { accounts: Some((0..3).map(account_id).collect()), limit: 200, ..Default::default() })).unwrap();
        let t = page.threads.iter().find(|t| t.subject == subject).unwrap_or_else(|| panic!("{subject}"));
        let msgs = block_on(d.thread(ThreadRef { account: t.account.clone(), thread: t.id.clone() })).unwrap();
        let last = msgs.last().unwrap().clone();
        (block_on(d.body(MessageRef { account: t.account.clone(), message: last.id.clone() })).unwrap(), last)
    }

    #[test]
    fn html_stories_exercise_the_renderer() {
        use lumbridge_mail_render::{Block, Policy, Trust};
        let d = demo();
        let (news, m) = body_of(&d, "The Weekly Byte #212: the quiet return of local-first");
        let doc = lumbridge_mail_render::blocks(&news, &Policy::default());
        assert!(doc.report.remote_blocked && doc.report.remote_images >= 3 && doc.report.tracking_pixels >= 1);
        let info = lumbridge_mail_render::header_info(&news, m.from.first());
        assert!(info.unsubscribe.as_ref().is_some_and(|u| u.one_click && u.mailto.is_some()));
        assert_eq!(info.auth.unwrap().trust, Trust::Verified);

        let (receipt, _) = body_of(&d, "Your order #4471-2290 has shipped");
        let doc = lumbridge_mail_render::blocks(&receipt, &Policy::default());
        assert!(doc.blocks.iter().any(|b| matches!(b, Block::Table { rows, .. } if rows.len() == 5)), "{:?}", doc.blocks);

        let (chain, _) = body_of(&d, "RE: Q3 numbers for the board deck");
        let doc = lumbridge_mail_render::blocks(&chain, &Policy::default());
        assert!(!doc.quoted.is_empty(), "the Outlook history folds");

        let (phish, m) = body_of(&d, "Action required: your account is on hold");
        let doc = lumbridge_mail_render::blocks(&phish, &Policy::default());
        assert!(doc.report.suspicious_links.len() >= 3 && doc.report.removed.scripts >= 1);
        let info = lumbridge_mail_render::header_info(&phish, m.from.first());
        assert!(info.reply_to_differs);
        assert_eq!(info.auth.unwrap().trust, Trust::Failed);
    }

    #[test]
    fn attachments_are_saved_as_made_up_files() {
        let d = demo();
        let (plan, m) = body_of(&d, "Floor plan for Quay Street");
        let inline = plan.attachments.iter().find(|a| a.content_id.is_some()).unwrap();
        let path = block_on(d.attachment(AttachmentRef { account: m.account.clone(), message: m.id.clone(), attachment: inline.id.clone() })).unwrap();
        let pic = crate::remote::decode(std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((pic.width, pic.height), (560, 320));
        let pdf = plan.attachments.iter().find(|a| a.mime == "application/pdf").unwrap();
        let path = block_on(d.attachment(AttachmentRef { account: m.account.clone(), message: m.id.clone(), attachment: pdf.id.clone() })).unwrap();
        assert!(std::fs::read_to_string(path).unwrap().contains("Made-up demo file"));
    }

    #[test]
    fn contacts_are_per_account_and_never_yourself() {
        let d = demo();
        let personal = block_on(d.contacts(AccountId::new("personal"))).unwrap();
        assert!(personal.iter().any(|a| a.email == "maya.chen@postbox.example"));
        assert!(personal.iter().all(|a| a.email != ACCOUNTS[PERSONAL].email));
        assert!(personal.iter().all(|a| !a.email.ends_with("relaymail.example")), "no anon people in personal");
        let anon = block_on(d.contacts(AccountId::new("anon"))).unwrap();
        assert!(anon.iter().any(|a| a.email == "kestrel92@relaymail.example"));
    }

    #[test]
    fn drafts_are_canned_but_addressed() {
        let d = demo();
        let page = block_on(d.threads(inbox())).unwrap();
        let dinner = page.threads.iter().find(|t| t.subject == "Dinner Saturday?").unwrap();
        let draft = block_on(d.draft(DraftRequest { account: dinner.account.clone(), thread: Some(dinner.id.clone()),
                                                    instruction: "say yes, Saturday works".into(),
                                                    reply_all: false, extra_cc: vec![] })).unwrap();
        assert!(draft.text.starts_with("Hi Maya,"), "{}", draft.text);
        assert!(draft.text.ends_with("Sam"));
        assert_eq!(draft.subject, "Re: Dinner Saturday?");
        assert_eq!(draft.to[0].email, "maya.chen@postbox.example");
    }
}
