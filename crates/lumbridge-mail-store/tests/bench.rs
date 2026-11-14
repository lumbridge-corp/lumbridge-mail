//! Load and query timings on generated, made-up mail. Ignored by default; run with
//!
//!     cargo test --release -p lumbridge-mail-store --test bench -- --ignored --nocapture
//!
//! `BENCH_MESSAGES` changes the size (default 500 000); `BENCH_DB=<path>` keeps the database.

mod common;

use common::*;
use lumbridge_mail_core::*;
use lumbridge_mail_store::{Caller, Store};
use std::time::{Duration, Instant};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Roughly Zipf: small indexes much more often.
    fn skewed(&mut self, n: u64) -> u64 {
        let r = (self.below(1_000_000) as f64 / 1_000_000.0).powi(3);
        (r * n as f64) as u64
    }
}

const SYLLABLES: &[&str] = &[
    "ka", "lo", "mer", "tin", "sa", "ve", "rho", "dan", "qui", "bel", "tor", "nis", "pa", "gru", "shen", "vor", "li",
    "mo", "zen", "fra", "cul", "dri", "po", "wex", "ta", "ro", "len", "gi", "bo", "nu", "sel", "ham",
];

fn vocabulary(rng: &mut Rng, n: usize) -> Vec<String> {
    let mut words = Vec::with_capacity(n);
    let mut seen = std::collections::HashSet::new();
    while words.len() < n {
        let parts = 2 + rng.below(3);
        let w: String = (0..parts).map(|_| SYLLABLES[rng.below(SYLLABLES.len() as u64) as usize]).collect();
        if seen.insert(w.clone()) {
            words.push(w);
        }
    }
    words
}

fn sentence(rng: &mut Rng, vocab: &[String], n: usize) -> String {
    (0..n).map(|_| vocab[rng.skewed(vocab.len() as u64) as usize].as_str()).collect::<Vec<_>>().join(" ")
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut times = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        last = Some(f());
        times.push(t.elapsed());
    }
    (median(times), last.unwrap())
}

#[test]
#[ignore = "benchmark: run explicitly with --ignored --release"]
fn bench_500k_messages() {
    let total: usize = std::env::var("BENCH_MESSAGES").ok().and_then(|v| v.parse().ok()).unwrap_or(500_000);
    let dir = tempfile::tempdir().unwrap();
    // BENCH_DB keeps the generated database for poking at afterwards.
    let path = std::env::var_os("BENCH_DB").map(std::path::PathBuf::from).unwrap_or_else(|| dir.path().join("mail.db"));
    let _ = std::fs::remove_file(&path);
    let mut s = Store::open(&path).unwrap();
    let accounts = [("personal", Persona::Personal), ("work", Persona::Work), ("anon", Persona::Anon)];
    for (id, p) in &accounts {
        s.upsert_account(&account(id, p.clone())).unwrap();
        s.apply_delta(&a(id), &Delta { mailboxes: Some(standard_mailboxes(id)), ..Delta::default() }).unwrap();
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let vocab = vocabulary(&mut rng, 30_000);
    let people: Vec<(String, String)> = (0..2_000)
        .map(|i| {
            let first = &vocab[rng.below(500) as usize];
            let last = &vocab[rng.below(5_000) as usize];
            let domain = ["example.com", "example.org", "example.net"][i % 3];
            (format!("{first} {last}"), format!("{first}.{last}@{domain}"))
        })
        .collect();

    let start_date = 1_600_000_000i64;
    let span = 5 * 365 * 86_400i64;
    let batch = 500;
    let load = Instant::now();
    let mut made = 0usize;
    let mut thread_no = 0u64;
    let mut ids_by_account: Vec<Vec<String>> = vec![Vec::new(); 3];
    while made < total {
        // Pick an account by share, then fill a batch for it.
        let pick = rng.below(100);
        let ai = if pick < 50 { 0 } else if pick < 90 { 1 } else { 2 };
        let acct = accounts[ai].0;
        let mut upserted = Vec::with_capacity(batch);
        while upserted.len() < batch && made < total {
            thread_no += 1;
            // 1 to 6 messages, about 2.5 on average.
            let in_thread = 1 + rng.below(4) + rng.skewed(3);
            let words = 3 + rng.below(5) as usize;
            let subject = sentence(&mut rng, &vocab, words);
            for k in 0..in_thread {
                if upserted.len() >= batch || made >= total {
                    break;
                }
                let who = &people[rng.skewed(people.len() as u64) as usize];
                let r = rng.below(100);
                let boxes: Vec<MailboxId> = match r {
                    0..=29 => vec![MailboxId::new("INBOX")],
                    30..=79 => vec![MailboxId::new("Archive")],
                    80..=89 => vec![MailboxId::new("Sent")],
                    90..=94 => vec![MailboxId::new("INBOX"), MailboxId::new("Projects")],
                    _ => vec![MailboxId::new("Trash")],
                };
                let id = format!("{acct}-{made}");
                ids_by_account[ai].push(id.clone());
                upserted.push(MessageMeta {
                    id: MessageId::new(&id),
                    account: a(acct),
                    thread: ThreadId::new(format!("t{thread_no}")),
                    mailboxes: boxes,
                    from: vec![Address::new(Some(&who.0), &who.1)],
                    to: vec![Address::new(Some("Test User"), &format!("{acct}@example.com"))],
                    cc: vec![],
                    bcc: vec![],
                    subject: if k == 0 { subject.clone() } else { format!("Re: {subject}") },
                    snippet: sentence(&mut rng, &vocab, 25),
                    date: start_date + rng.below(span as u64) as i64,
                    flags: Flags { seen: rng.below(100) < 85, flagged: rng.below(100) < 3, ..Flags::default() },
                    has_attachments: rng.below(100) < 8,
                    size: 2_000 + rng.below(50_000),
                    rfc_message_id: Some(format!("{id}@mail.example.com")),
                    in_reply_to: None,
                    references: vec![],
                    partial: false,
                });
                made += 1;
            }
        }
        s.apply_delta(&a(acct), &Delta { upserted, cursor: Some(SyncCursor(made.to_string())), ..Delta::default() })
            .unwrap();
    }
    let load_time = load.elapsed();

    // Bodies for 20k messages.
    let bodies = Instant::now();
    let n_bodies = 20_000.min(ids_by_account[0].len());
    for id in ids_by_account[0].iter().take(n_bodies) {
        let text = sentence(&mut rng, &vocab, 300);
        s.put_body(&a("personal"), &MessageId::new(id), &Body { text: Some(text), ..Body::default() }).unwrap();
    }
    let body_time = bodies.elapsed();
    drop(s);
    let size_mb = std::fs::metadata(&path).unwrap().len() as f64 / 1e6;

    let (open_time, mut s) = {
        let t = Instant::now();
        let s = Store::open(&path).unwrap();
        (t.elapsed(), s)
    };

    println!("\n== lumbridge-mail-store bench: {made} made-up messages, {thread_no} threads ==");
    println!("load via apply_delta (batches of {batch}): {:.1?} ({:.0} msg/s)", load_time, made as f64 / load_time.as_secs_f64());
    println!("put_body x{n_bodies} (300 words each): {:.1?} ({:.0}/s)", body_time, n_bodies as f64 / body_time.as_secs_f64());
    println!("database size: {size_mb:.0} MB; reopen: {open_time:.1?}");

    let inbox = Query { role: Some(MailboxRole::Inbox), limit: 50, ..q() };
    let runs = 15;
    let report = |label: &str, d: Duration, n: usize| println!("{label:<52} {:>9.2} ms  ({n} threads)", d.as_secs_f64() * 1e3);

    let (d, p) = time(runs, || s.list_threads(&inbox).unwrap());
    report("unified inbox, page 1", d, p.threads.len());
    let mut page = p.next.clone();
    for _ in 0..18 {
        page = s.list_threads(&Query { page: page.clone(), ..inbox.clone() }).unwrap().next;
    }
    let (d, p) = time(runs, || s.list_threads(&Query { page: page.clone(), ..inbox.clone() }).unwrap());
    report("unified inbox, page 20", d, p.threads.len());
    let (d, p) = time(runs, || s.list_threads(&Query { unread_only: true, ..inbox.clone() }).unwrap());
    report("unified inbox, unread only", d, p.threads.len());
    let one = Query { accounts: Some(vec![a("work")]), mailbox: Some(MailboxId::new("INBOX")), limit: 50, ..q() };
    let (d, p) = time(runs, || s.list_threads(&one).unwrap());
    report("one account's INBOX, page 1", d, p.threads.len());
    let (d, p) = time(runs, || s.list_threads(&Query { limit: 50, ..q() }).unwrap());
    report("all mail (no filter), page 1", d, p.threads.len());
    let (d, p) = time(runs, || s.list_threads(&Query { category: Some(ai::Category::Newsletter), ..inbox.clone() }).unwrap());
    report("unified inbox, category with no triage yet (worst)", d, p.threads.len());

    let common_word = vocab[0].clone();
    let rare_word = vocab[20_000].clone();
    let mut typo: Vec<char> = vocab[300].chars().collect();
    typo.swap(1, 2);
    let typo: String = typo.into_iter().collect();
    let sender = people[5].1.clone();
    let searches = [
        ("search: most common word", common_word.clone()),
        ("search: rare word", rare_word.clone()),
        ("search: common word + is:starred", format!("{common_word} is:starred")),
        ("search: two words", format!("{} {}", vocab[10], vocab[40])),
        ("search: typo (swapped letters)", typo.clone()),
        ("search: from:<address>", format!("from:{sender}")),
        ("search: subject:<word> is:unread", format!("subject:{} is:unread", vocab[3])),
        ("search: has:attachment before: after:", "has:attachment after:2021-01-01 before:2021-03-01".to_owned()),
        ("search: in:inbox is:starred", "in:inbox is:starred".to_owned()),
        ("search: body word (cached bodies)", vocab[25_000].clone()),
        ("search: no match", "zzzzqqq".to_owned()),
    ];
    for (label, t) in searches {
        let query = Query { text: Some(t.clone()), limit: 50, ..q() };
        let (d, p) = time(runs, || s.list_threads(&query).unwrap());
        report(&format!("{label} [{t}]"), d, p.threads.len());
    }

    let t1 = s.list_threads(&inbox).unwrap().threads[0].clone();
    let (d, msgs) = time(runs, || s.thread(&t1.account, &t1.id).unwrap());
    report("thread.get (messages of one thread)", d, msgs.len());

    let target = MessageId::new(&ids_by_account[0][123]);
    let (d, _) = time(runs, || {
        let e = s.enqueue(&a("personal"), Action::SetSeen { ids: vec![target.clone()], seen: true }, Caller::Person).unwrap();
        let q = s.next_action(i64::MAX / 4).unwrap().unwrap();
        s.action_done(q.id).unwrap();
        e
    });
    report("enqueue SetSeen + next_action + action_done", d, 1);

    let changed: Vec<MessageMeta> =
        ids_by_account[0].iter().take(100).map(|id| s.message(&a("personal"), &MessageId::new(id)).unwrap().unwrap()).collect();
    let (d, _) = time(5, || {
        let mut c = changed.clone();
        for m in &mut c {
            m.flags.seen = !m.flags.seen;
        }
        s.apply_delta(&a("personal"), &Delta { upserted: c, cursor: Some(SyncCursor("x".into())), ..Delta::default() }).unwrap()
    });
    report("incremental delta: 100 flag changes", d, 100);
    let (d, _) = time(5, || s.mailboxes(None).unwrap());
    report("mailboxes.list (all accounts)", d, 0);
}
