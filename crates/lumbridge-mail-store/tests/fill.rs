//! The body fill's work list: newest first, new mail before the walk, what it leaves out, and that the
//! walk survives a reopen without starting again from the top.
mod common;

use common::*;
use lumbridge_mail_core::*;
use lumbridge_mail_store::Store;

fn msg(id: &'static str, date: i64) -> MessageMeta {
    M { id, thread: id, date, subject: "Made-up", ..M::default() }.meta()
}

fn next(s: &mut Store) -> Option<String> {
    let item = s.body_fill_next(&a("personal")).unwrap()?;
    s.body_fill_passed(&a("personal"), &item).unwrap();
    Some(item.message.0)
}

fn body(text: &str) -> Body {
    Body { text: Some(text.into()), ..Body::default() }
}

#[test]
fn newest_first_leaving_out_bodies_drafts_and_what_was_set_aside() {
    let mut s = store();
    let p = a("personal");
    s.apply_delta(&p, &Delta { mailboxes: Some(vec![mailbox("personal", "Drafts", MailboxRole::Drafts)]), ..Delta::default() }).unwrap();
    let mut draft_flag = msg("m4", 400);
    draft_flag.flags.draft = true;
    let in_drafts = M { id: "m5", thread: "m5", date: 500, mailboxes: &["Drafts"], ..M::default() }.meta();
    put(&mut s, &[msg("m1", 100), msg("m2", 200), msg("m3", 300), draft_flag, in_drafts, msg("m6", 600)]);
    s.put_body(&p, &MessageId::new("m3"), &body("already here")).unwrap();
    s.body_fill_unfetchable(&p, &MessageId::new("m2"), "not_found").unwrap();
    assert_eq!(s.body_fill_progress(&p).unwrap().total, 4, "drafts by flag are not counted");

    assert_eq!(next(&mut s).as_deref(), Some("m6"));
    assert_eq!(next(&mut s).as_deref(), Some("m1"), "m5 is in Drafts, m4 a draft, m3 has a body, m2 was set aside");
    assert_eq!(next(&mut s), None);
    let p1 = s.body_fill_progress(&p).unwrap();
    assert!(p1.done_at.is_some() && !p1.asked);
    assert_eq!((p1.have, p1.unfetchable), (1, 1));

    // New mail after the walk is done comes first, and only it.
    put(&mut s, &[msg("m7", 700)]);
    let item = s.body_fill_next(&p).unwrap().unwrap();
    assert_eq!((item.message.as_str(), item.new_mail), ("m7", true));
    s.put_body(&p, &item.message, &body("new")).unwrap();
    assert_eq!(s.body_fill_next(&p).unwrap(), None);

    // A rewind with `since` looks again at what was set aside from then on (and only that).
    s.body_fill_rewind(&p, Some(150), true).unwrap();
    assert!(s.body_fill_progress(&p).unwrap().asked);
    let left: Vec<String> = std::iter::from_fn(|| next(&mut s)).collect();
    assert_eq!(left, vec!["m6", "m2", "m1"]);
    assert!(!s.body_fill_progress(&p).unwrap().asked, "asked ends with the walk");
}

#[test]
fn new_mail_comes_before_the_rest_of_the_walk() {
    let mut s = store();
    put(&mut s, &[msg("m1", 100), msg("m2", 200), msg("m3", 300)]);
    assert_eq!(next(&mut s).as_deref(), Some("m3"));
    put(&mut s, &[msg("m9", 900), msg("m8", 800)]);
    assert_eq!(next(&mut s).as_deref(), Some("m9"));
    // Not fetched (an outage): still first next time.
    assert_eq!(s.body_fill_next(&a("personal")).unwrap().unwrap().message.as_str(), "m9");
    s.put_body(&a("personal"), &MessageId::new("m9"), &body("x")).unwrap();
    s.body_fill_unfetchable(&a("personal"), &MessageId::new("m8"), "not_found").unwrap();
    assert_eq!(next(&mut s).as_deref(), Some("m2"), "then the walk goes on below m3");
}

#[test]
fn the_walk_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mail.db");
    {
        let mut s = Store::open(&path).unwrap();
        s.upsert_account(&account("personal", Persona::Personal)).unwrap();
        s.apply_delta(&a("personal"), &Delta { mailboxes: Some(standard_mailboxes("personal")), ..Delta::default() }).unwrap();
        put(&mut s, &[msg("m1", 100), msg("m2", 200), msg("m3", 300), msg("m4", 400)]);
        assert_eq!(next(&mut s).as_deref(), Some("m4"));
        // Passed without a body (say it failed for good, and the daemon moved on).
        assert_eq!(next(&mut s).as_deref(), Some("m3"));
    }
    let mut s = Store::open(&path).unwrap();
    assert_eq!(next(&mut s).as_deref(), Some("m2"), "not m4 again: the marks were kept");
    assert_eq!(next(&mut s).as_deref(), Some("m1"));
    assert_eq!(next(&mut s), None);
}

#[test]
fn accounts_are_separate_and_unknown_ones_are_not_found() {
    let mut s = store();
    put(&mut s, &[msg("m1", 100), M { account: "work", id: "w1", thread: "w1", date: 50, ..M::default() }.meta()]);
    assert_eq!(s.body_fill_next(&a("work")).unwrap().unwrap().message.as_str(), "w1");
    assert_eq!(next(&mut s).as_deref(), Some("m1"));
    assert!(matches!(s.body_fill_next(&a("nobody")), Err(Error::NotFound(_))));
    assert_eq!(s.body_fill_next(&a("anon")).unwrap(), None, "an account with no mail");
}
