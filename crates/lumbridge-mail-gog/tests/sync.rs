//! Sync through gog with as few processes as possible: one `gmail messages search` per page of 100
//! (plus a resume-point lookup on the first page and a probe on the last), a window instead of the
//! whole mailbox, label changes from listings instead of refetches, slow backfill, and whole threads
//! per body fetch. Every gog reply here is made up (`example.*` addresses); no real account, no real gog.

use lumbridge_mail_core::*;
use lumbridge_mail_gog::fake::FakeRunner;
use lumbridge_mail_gog::{ComposeMode, GogCall, GogConfig, GogOutput, GogProvider};
use serde_json::{Value, json};
use std::sync::Arc;

const EMAIL: &str = "test.user@example.com";
const NOT_FOUND: &str = "Google API error (404 notFound): Requested entity was not found.";
const WINDOW_Q: &str = "in:anywhere newer_than:90d";
const RECENT_Q: &str = "in:anywhere newer_than:7d";

fn fx(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap()
}

fn id(n: usize) -> String {
    format!("19a0000000{n:06x}")
}

/// A search row as gog `version` prints it: 0.28 has id/thread/date/from/subject/labels (+ body and
/// attachments with --include-body); 0.47+ adds internalDateIso, recipients and headers.
fn row(n: usize, new_gog: bool, labels: &[&str]) -> Value {
    let mut r = json!({
        "id": id(n), "threadId": id(n - n % 2), "date": "2026-11-09 14:00",
        "from": format!("Sender {n} <sender{n}@example.org>"), "subject": format!("Made-up subject {n}"),
        "labels": labels, "body": format!("Hello, this is made-up message number {n}."),
    });
    if new_gog {
        r["internalDateIso"] = json!(format!("2026-11-09T14:{:02}:00Z", n % 60));
        r["to"] = json!(format!("Test User <{EMAIL}>"));
        r["headers"] = json!({"Message-Id": format!("<m{n}@mail.example.org>"), "Content-Type": "multipart/alternative"});
    }
    r
}

fn page_json(rows: std::ops::Range<usize>, new_gog: bool, next: &str) -> Value {
    json!({"messages": rows.map(|n| row(n, new_gog, &["INBOX", "UNREAD"])).collect::<Vec<_>>(), "nextPageToken": next})
}

fn fake(version: &str) -> Arc<FakeRunner> {
    let f = Arc::new(FakeRunner::new());
    f.on_json("version", &[], json!({"version": version, "commit": "abc1234", "date": "2026-06-20"}));
    f.on_json("gmail.labels.list", &[], fx("labels_list.json"));
    f
}

fn cfg() -> GogConfig {
    let mut c = GogConfig::new(AccountId::new("personal-gmail"), EMAIL);
    c.compose = ComposeMode::Raw;
    c
}

fn provider(f: &Arc<FakeRunner>, c: GogConfig) -> GogProvider {
    GogProvider::new(c, f.clone())
}

fn cursor(d: &Delta) -> Value {
    serde_json::from_str(&d.cursor.as_ref().expect("cursor").0).unwrap()
}

fn args(c: &GogCall) -> Vec<String> {
    c.display_args()
}

fn has_seq(a: &[String], seq: &[&str]) -> bool {
    a.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(x, y)| x == y))
}

/// The full sync of 250 messages with gog `version`: every call it made, by command.
async fn full_sync_250(version: &str, new_gog: bool) -> (Arc<FakeRunner>, Vec<Delta>) {
    let f = fake(version);
    f.on_json("gmail.messages.search", &[WINDOW_Q], page_json(1..101, new_gog, "p2"));
    f.on_json("gmail.messages.search", &[WINDOW_Q, "p2"], page_json(101..201, new_gog, "p3"));
    f.on_json("gmail.messages.search", &[WINDOW_Q, "p3"], page_json(201..251, new_gog, ""));
    f.on_json("gmail.raw", &[&id(1), "minimal"], json!({"id": id(1), "historyId": "7000"}));
    f.on_json("gmail.history", &["--since", "7000", "1"], json!({"historyId": "7010", "messages": [], "nextPageToken": ""}));
    let p = provider(&f, cfg());
    let mut deltas = vec![p.sync(None).await.unwrap()];
    while deltas.last().unwrap().more {
        let c = deltas.last().unwrap().cursor.clone();
        deltas.push(p.sync(c.as_ref()).await.unwrap());
    }
    (f, deltas)
}

#[tokio::test]
async fn a_page_of_100_messages_is_one_gog_process_on_new_gog() {
    let (f, deltas) = full_sync_250("v0.48.0", true).await;
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas.iter().map(|d| d.upserted.len()).sum::<usize>(), 250);
    // 250 messages: 3 listings + the label list + gog version + 1 resume point + 1 probe = 7 processes
    // (it was 3 listings + 250 `gmail raw` + 6 label calls + 1 probe = 260 before).
    assert_eq!(f.calls().len(), 7, "{:?}", f.commands());
    assert_eq!(f.calls_to("gmail.messages.search").len(), 3);
    assert_eq!(f.calls_to("gmail.raw").len(), 1);
    assert!(f.calls_to("gmail.labels.get").is_empty(), "counts come from the store, not from gog");
    for c in f.calls_to("gmail.messages.search") {
        let a = args(&c);
        assert!(has_seq(&a, &["--max", "100"]) && has_seq(&a, &["--timezone", "UTC"]), "{a:?}");
        assert!(a.contains(&"--include-body".into()) && a.contains(&"--include-recipients".into()), "{a:?}");
        assert!(has_seq(&a, &["--headers", "Message-Id,In-Reply-To,References,Content-Type"]), "{a:?}");
    }
    for d in &deltas {
        assert!(d.reset && d.window_floor.is_some(), "every page belongs to a windowed full sync");
    }
    assert!(deltas[0].mailboxes.is_some() && deltas[1].mailboxes.is_none(), "labels once, then cached");
    let m = &deltas[0].upserted[0];
    assert!(m.partial);
    assert_eq!(m.to[0].email, EMAIL);
    assert_eq!(m.rfc_message_id.as_deref(), Some("m1@mail.example.org"));
    assert_eq!(m.snippet, "Hello, this is made-up message number 1.");
    assert_eq!(m.thread.as_str(), id(0));
    assert!(!m.flags.seen && m.mailboxes.contains(&MailboxId::new("INBOX")));
    assert_eq!(cursor(&deltas[0])["q"], WINDOW_Q);
    let last = cursor(&deltas[2]);
    assert_eq!((last["k"].as_str(), last["h"].as_u64()), (Some("hist"), Some(7010)));
    assert!(last["lo"].as_i64().is_some(), "the window's floor is kept for backfill: {last}");
}

#[tokio::test]
async fn old_gog_costs_the_same_processes_with_lighter_records() {
    let (f, deltas) = full_sync_250("v0.28.0", false).await;
    assert_eq!(f.calls().len(), 7, "{:?}", f.commands());
    for c in f.calls_to("gmail.messages.search") {
        let a = args(&c);
        assert!(a.contains(&"--include-body".into()), "{a:?}");
        assert!(!a.contains(&"--include-recipients".into()) && !a.contains(&"--headers".into()), "0.28 has neither: {a:?}");
    }
    let m = &deltas[0].upserted[0];
    assert!(m.partial && m.to.is_empty() && m.rfc_message_id.is_none(), "filled in later from a thread fetch");
    assert_eq!(m.date, 1_794_232_800, "minutes of the Date header, in UTC");
    assert!(!m.snippet.is_empty());
}

#[tokio::test]
async fn a_message_dated_long_ago_cannot_widen_the_windows_floor() {
    // gog 0.28 dates rows by their Date header: an imported message can claim 1999 while Gmail got it
    // yesterday. The floor (what the store may delete below) stays inside the 90-day window.
    let f = fake("v0.28.0");
    let mut odd = row(2, false, &["INBOX"]);
    odd["date"] = json!("1999-01-01 00:00");
    f.on_json("gmail.messages.search", &[WINDOW_Q], json!({"messages": [row(1, false, &["INBOX"]), odd], "nextPageToken": ""}));
    f.on_json("gmail.raw", &[&id(1), "minimal"], json!({"id": id(1), "historyId": "7000"}));
    f.on_json("gmail.history", &["--since", "7000", "1"], json!({"historyId": "7000", "messages": [], "nextPageToken": ""}));
    let d = provider(&f, cfg()).sync(None).await.unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let floor = d.window_floor.unwrap();
    assert!(floor >= now - 89 * 86_400 - 60, "floor {floor} must be inside the window");
    assert_eq!(cursor(&d)["lo"], floor, "backfill starts there");
}

#[tokio::test]
async fn the_window_stops_at_its_message_limit() {
    let f = fake("v0.48.0");
    f.on_json("gmail.messages.search", &[WINDOW_Q], page_json(1..101, true, "p2"));
    f.on_json("gmail.messages.search", &[WINDOW_Q, "p2"], page_json(101..151, true, "p3"));
    f.on_json("gmail.raw", &[&id(1), "minimal"], json!({"id": id(1), "historyId": "7000"}));
    f.on_json("gmail.history", &["--since", "7000", "1"], json!({"historyId": "7000", "messages": [], "nextPageToken": ""}));
    let mut c = cfg();
    c.window_max = 150;
    let p = provider(&f, c);
    let d = p.sync(None).await.unwrap();
    let d2 = p.sync(d.cursor.as_ref()).await.unwrap();
    assert!(!d2.more, "150 messages is the window, even though Gmail has a next page");
    assert!(has_seq(&args(&f.calls_to("gmail.messages.search")[1]), &["--max", "50"]));
    assert!(f.calls_to("gmail.messages.search").iter().all(|c| !args(c).contains(&"p3".into())));
    assert_eq!(cursor(&d2)["k"], "hist");

    // Without a window the old meaning holds: everything, not windowed.
    let f = fake("v0.48.0");
    f.on_json("gmail.messages.search", &["in:anywhere"], page_json(1..3, true, ""));
    f.on_json("gmail.raw", &[&id(1), "minimal"], json!({"id": id(1), "historyId": "7000"}));
    f.on_json("gmail.history", &["--since", "7000", "1"], json!({"historyId": "7000", "messages": [], "nextPageToken": ""}));
    let mut c = cfg();
    c.window_days = 0;
    c.window_max = 0;
    let d = provider(&f, c).sync(None).await.unwrap();
    assert!(d.reset && d.window_floor.is_none());
    assert!(cursor(&d).get("lo").is_none());
}

#[tokio::test]
async fn incremental_sync_takes_records_and_label_changes_from_one_listing() {
    let f = fake("v0.48.0");
    // New in history: 1 (in the recent listing), 5 (not listed: imported with an old date), 9 (gone).
    f.on_json("gmail.history", &["--since", "5000", "100"], json!({"historyId": "5200", "messages": [id(1), id(5), id(9), id(1)], "nextPageToken": ""}));
    let recent = |starred: bool| json!({"messages": [
        row(1, true, &["INBOX", "UNREAD"]),
        row(2, true, if starred { &["INBOX", "STARRED"] } else { &["INBOX"] }),
        row(3, true, &["INBOX"]),
    ], "nextPageToken": ""});
    f.on_json("gmail.messages.search", &[RECENT_Q], recent(false));
    f.on_json("gmail.raw", &[&id(5), "metadata"], json!({"id": id(5), "threadId": id(5), "labelIds": ["INBOX"], "historyId": "5100",
        "internalDate": "1700000000000", "payload": {"headers": [{"name": "From", "value": "old@example.org"}, {"name": "Subject", "value": "Imported"}]}}));
    f.on("gmail.raw", &[&id(9), "metadata"], GogOutput::fail(5, NOT_FOUND));
    let p = provider(&f, cfg());
    let c = SyncCursor(r#"{"k":"hist","h":5000,"page":null,"lo":1700000000}"#.into());
    let d = p.sync(Some(&c)).await.unwrap();
    assert!(!d.more && !d.reset && !d.backfill);
    let mut ids: Vec<&str> = d.upserted.iter().map(|m| m.id.as_str()).collect();
    ids.sort();
    // First listing in this process: every listed row counts as changed (cheap: no extra calls).
    assert_eq!(ids, vec![id(1).as_str(), id(2).as_str(), id(3).as_str(), id(5).as_str()]);
    assert_eq!(d.removed, vec![MessageId::new(id(9))]);
    assert_eq!(cursor(&d)["h"], 5200);
    assert_eq!(cursor(&d)["lo"], 1_700_000_000);
    // history + labels + version + one listing + 2 lookups for what the listing did not have.
    assert_eq!(f.commands(), vec!["gmail.history", "gmail.labels.list", "version", "gmail.messages.search", "gmail.raw", "gmail.raw"]);

    // Next round: message 2 was starred on the phone. History says nothing; the listing does.
    f.clear_calls();
    f.on_json("gmail.history", &["--since", "5200", "100"], json!({"historyId": "5300", "messages": [], "nextPageToken": ""}));
    f.on_json("gmail.messages.search", &[RECENT_Q], recent(true));
    let d = p.sync(d.cursor.as_ref()).await.unwrap();
    assert_eq!(d.upserted.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec![id(2).as_str()]);
    assert!(d.upserted[0].flags.flagged && d.upserted[0].partial);
    assert!(d.mailboxes.is_none(), "label list still fresh");
    // Two processes per quiet sync round: history and one listing. No refetches.
    assert_eq!(f.commands(), vec!["gmail.history", "gmail.messages.search"]);
}

#[tokio::test]
async fn expired_history_starts_a_windowed_resync_that_keeps_older_mail() {
    let f = fake("v0.48.0");
    f.on("gmail.history", &["--since", "100", "100"], GogOutput::fail(5, NOT_FOUND));
    f.on_json("gmail.messages.search", &[WINDOW_Q], page_json(1..3, true, "p2"));
    f.on_json("gmail.raw", &[&id(1), "minimal"], json!({"id": id(1), "historyId": "7000"}));
    let p = provider(&f, cfg());
    let d = p.sync(Some(&SyncCursor(r#"{"k":"hist","h":100,"page":null,"lo":1600000000}"#.into()))).await.unwrap();
    assert!(d.reset && d.window_floor.is_some() && d.more);
    assert_eq!(cursor(&d)["k"], "full");
    assert_eq!(cursor(&d)["old"], 1_600_000_000, "the floor of what is stored survives the resync");
    // ...but never decides what the resync may delete: that is the window's own floor (rows of 2026-11-09).
    assert!(d.window_floor.unwrap() > 1_790_000_000, "{:?}", d.window_floor);
}

#[tokio::test]
async fn a_quiet_mailbox_catches_up_with_one_listing() {
    let f = fake("v0.48.0");
    f.on("gmail.history", &["--since", "100", "100"], GogOutput::fail(5, NOT_FOUND));
    f.on_json("gmail.messages.search", &["in:anywhere"], page_json(1..4, true, "more-but-not-needed"));
    f.on_json("gmail.raw", &[&id(1), "minimal"], json!({"id": id(1), "historyId": "5003"}));
    f.on_json("gmail.history", &["--since", "5003", "1"], json!({"historyId": "5050", "messages": [], "nextPageToken": ""}));
    let p = provider(&f, cfg());
    let d = p.sync(Some(&SyncCursor(r#"{"k":"hist","h":100,"page":null,"quiet":true}"#.into()))).await.unwrap();
    assert!(!d.more && !d.reset);
    assert_eq!(d.upserted.len(), 3);
    assert_eq!(cursor(&d)["h"], 5050);
    assert_eq!(f.calls_to("gmail.messages.search").len(), 1);
}

#[tokio::test]
async fn backfill_runs_one_page_per_round_only_when_asked() {
    let f = fake("v0.48.0");
    f.on_json("gmail.history", &["--since", "5000"], json!({"historyId": "5000", "messages": [], "nextPageToken": ""}));
    f.on_json("gmail.messages.search", &[RECENT_Q], json!({"messages": [], "nextPageToken": ""}));
    f.on_json("gmail.messages.search", &["in:anywhere before:1700000000"], page_json(500..600, true, "bf2"));
    f.on_json("gmail.messages.search", &["in:anywhere before:1700000000", "bf2"], page_json(600..610, true, ""));
    let mut c = cfg();
    c.backfill_every = std::time::Duration::from_secs(3600);
    let p = provider(&f, c);
    let start = SyncCursor(r#"{"k":"hist","h":5000,"page":null,"lo":1700000000}"#.into());
    // Not asked: nothing older is listed.
    let d = p.sync(Some(&start)).await.unwrap();
    assert!(!d.more);
    assert!(f.calls_to("gmail.messages.search").iter().all(|c| !args(c).iter().any(|a| a.contains("before:"))));

    p.request_backfill(None).await.unwrap();
    let d = p.sync(d.cursor.as_ref()).await.unwrap();
    assert!(d.more, "an incremental round, then straight to one backfill page");
    assert_eq!(cursor(&d)["step"], true);
    let d = p.sync(d.cursor.as_ref()).await.unwrap();
    assert!(d.backfill && !d.reset && !d.more, "a backfill page: stored, not announced, then wait");
    assert_eq!(d.upserted.len(), 100);
    assert_eq!(cursor(&d)["bf"]["page"], "bf2");
    // The next round is within backfill_every: history only, no backfill page.
    f.clear_calls();
    let d = p.sync(d.cursor.as_ref()).await.unwrap();
    assert!(!d.more && !d.backfill);
    assert!(f.calls_to("gmail.messages.search").iter().all(|c| !args(c).iter().any(|a| a.contains("before:"))));
    assert_eq!(cursor(&d)["bf"]["page"], "bf2", "still pending");

    // A fresh provider (as after a restart) with backfill_every 0 finishes it.
    let mut c = cfg();
    c.backfill_every = std::time::Duration::ZERO;
    let p2 = provider(&f, c);
    let d = p2.sync(d.cursor.as_ref()).await.unwrap();
    assert!(d.more);
    let d = p2.sync(d.cursor.as_ref()).await.unwrap();
    assert!(d.backfill && d.upserted.len() == 10);
    let end = cursor(&d);
    assert!(end.get("bf").is_none() && end.get("lo").is_none(), "done: all mail is stored: {end}");
}

#[tokio::test]
async fn a_backfill_request_without_a_window_does_nothing() {
    let f = fake("v0.48.0");
    f.on_json("gmail.history", &["--since", "5000"], json!({"historyId": "5000", "messages": [], "nextPageToken": ""}));
    f.on_json("gmail.messages.search", &[RECENT_Q], json!({"messages": [], "nextPageToken": ""}));
    let p = provider(&f, cfg());
    p.request_backfill(Some(1_000)).await.unwrap();
    let d = p.sync(Some(&SyncCursor("5000".into()))).await.unwrap();
    assert!(!d.more && cursor(&d).get("bf").is_none());
}

#[tokio::test]
async fn one_body_fetch_brings_the_whole_thread() {
    let f = fake("v0.48.0");
    let m3 = fx("m3_full.json");
    let mut m2 = fx("m2_metadata.json");
    m2["payload"]["parts"] = json!([{"partId": "0", "mimeType": "text/plain", "body": {"data": "U3VyZSwgVGh1cnNkYXkgd29ya3Mu"}}]);
    m2["payload"]["mimeType"] = json!("multipart/alternative");
    let t2 = m3["threadId"].as_str().unwrap().to_owned();
    f.on_json("gmail.thread.get", &[&t2], json!({"thread": {"id": t2, "historyId": "5003", "messages": [m2, m3]}, "downloaded": []}));
    let p = provider(&f, cfg());
    let m3_id = MessageId::new(m3_id_of(&f));
    let b = p.fetch_body_bundle(&m3_id, Some(&ThreadId::new(&t2))).await.unwrap();
    assert!(b.body.text.as_deref().unwrap().starts_with("Can we move the garden meeting"));
    assert_eq!(b.others.len(), 1);
    assert_eq!(b.others[0].1.text.as_deref(), Some("Sure, Thursday works."));
    assert_eq!(b.metas.len(), 2);
    assert!(b.metas.iter().all(|m| !m.partial && m.rfc_message_id.is_some()), "full records fill in what listings lacked");
    assert_eq!(f.commands(), vec!["gmail.thread.get"], "one process for the whole thread");

    // Without a thread hint: the message alone.
    f.on_json("gmail.raw", &[m3_id.as_str(), "full"], fx("m3_full.json"));
    f.clear_calls();
    let b = p.fetch_body_bundle(&m3_id, None).await.unwrap();
    assert!(b.others.is_empty() && b.metas.is_empty());
    assert_eq!(f.commands(), vec!["gmail.raw"]);
}

fn m3_id_of(_f: &FakeRunner) -> String {
    fx("m3_full.json")["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn version_is_asked_once_and_an_unreachable_gog_is_not_cached_as_old() {
    let f = Arc::new(FakeRunner::new());
    f.on_json("gmail.labels.list", &[], fx("labels_list.json"));
    f.on_err("version", &[], Error::Offline("ssh gog-host: Connection reset by peer".into()));
    let p = provider(&f, cfg());
    assert!(p.features().await.is_err());
    f.on_json("version", &[], json!({"version": "v0.47.1"}));
    assert!(p.features().await.unwrap().headers());
    assert!(p.features().await.unwrap().recipients());
    assert_eq!(f.calls_to("version").len(), 2, "the failure was not remembered, the answer is");
}
