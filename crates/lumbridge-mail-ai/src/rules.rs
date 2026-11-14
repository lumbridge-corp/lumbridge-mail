//! The rules stage: no model. Sensitive mail first (core's `safety::sensitive_sender_kind`, which holds
//! the account-security and money patterns), then sender and header rules for
//! newsletters, promotions, notifications and transactional mail. What the rules cannot decide goes to
//! the model; what they mark sensitive goes nowhere.

use crate::text::{self, who};
use crate::thread::{MailMessage, MailThread};
use lumbridge_mail_core::{Address, Category, Triage, safety};
use regex::Regex;
use std::sync::LazyLock;

/// Written into `Triage::by` for everything decided here.
pub const BY_RULES: &str = "rules";

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

/// What kind of sensitive mail one message is, if it is: core's rule first, then ours.
pub fn sensitive_message(m: &MailMessage) -> Option<&'static str> {
    let from = m.sender().cloned().unwrap_or_else(|| Address::new(None, ""));
    sensitive_parts(&from, &m.meta.subject, &m.text())
}

/// [`sensitive_message`] for loose parts (a search hit, a notification): core's rules
/// (`safety::sensitive_sender_kind`), which now hold the extra patterns that used to live here.
pub fn sensitive_parts(sender: &Address, subject: &str, body: &str) -> Option<&'static str> {
    safety::sensitive_sender_kind(sender.name.as_deref(), &sender.email, subject, body)
}

/// The one line shown for a sensitive thread ("A security code from Acme"), if any message in it is
/// sensitive. Nothing else about the thread is ever shown, quoted or given to a model.
pub fn sensitive_line(thread: &MailThread) -> Option<String> {
    thread.messages().iter().rev().find_map(|m| {
        let kind = sensitive_message(m)?;
        let x = m.sender().map(who).unwrap_or_else(|| "someone".into());
        Some(format!("{kind} from {x}"))
    })
}

static NOTIFY_DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)(^|\.)(github\.com|gitlab\.com|bitbucket\.org|sentry\.io|pagerduty\.com|opsgenie\.net|atlassian\.net|atlassian\.com|linear\.app|vercel\.com|netlify\.com|circleci\.com|buildkite\.com|travis-ci\.com|heroku\.com|datadoghq\.com|grafana\.net|statuspage\.io|uptimerobot\.com|betteruptime\.com|render\.com|fly\.io|cloudflare\.com|docker\.com|npmjs\.com|crates\.io|readthedocs\.org|slack\.com|notion\.so|figma\.com|calendly\.com|zoom\.us|trello\.com|asana\.com|gitea\.(com|io))$")
});
static NOREPLY: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)^(no-?reply|do-?not-?reply|donotreply|notifications?|notify|alerts?|mailer-daemon|postmaster|automated|bounce[s]?|system|robot|bot|builds?|ci|monitoring|updates?)([+._-].*)?$")
});
static TRANSACTIONAL_SENDER: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)^(receipts?|orders?|order-update|billing|invoices?|payments?|shipping|shipment|delivery|tickets?|bookings?|reservations?|confirmations?|auto-confirm|calendar-notification|store|support)([+._-].*)?$")
});
static TRANSACTIONAL_SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(receipt|your order|order (confirmation|#|number|no\.?)|has shipped|shipped|out for delivery|delivered|delivery (update|scheduled)|invoice|payment (received|confirmation|failed)|booking (confirmation|confirmed)|reservation|itinerary|e-?ticket|boarding pass|subscription (renewal|renewed|confirmed)|trial (ends|ending)|your (plan|bill|account) (has|is|was)|refund)\b|^(invitation|updated invitation|accepted|declined|tentatively accepted|canceled event|cancelled event):")
});
static PROMO_SENDER: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)^(promo(tions?)?|offers?|deals?|marketing|sales?|shop|news-?offers|specials?|rewards)([+._-].*)?$"));
static PROMO_SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\d{1,2}\s?% off|\b(sale|deal|deals|discount|coupon|promo code|free shipping|limited time|last chance|ends tonight|black friday|cyber monday|flash sale|save \$?\d+|buy one|bogo|exclusive offer|special offer|just dropped|new arrivals|don'?t miss)\b")
});

fn has_list_headers(m: &MailMessage) -> bool {
    m.header("list-id").is_some()
        || m.header("list-unsubscribe").is_some()
        || m.header("precedence").is_some_and(|p| matches!(p.trim().to_ascii_lowercase().as_str(), "bulk" | "list" | "junk"))
}

fn auto_submitted(m: &MailMessage) -> bool {
    m.header("auto-submitted").is_some_and(|v| !v.trim().eq_ignore_ascii_case("no"))
        || m.header("x-github-reason").is_some()
        || m.header("x-gitlab-project").is_some()
        || m.header("x-gitea-reason").is_some()
}

/// Category by headers and sender alone, for one message. None = ask the model.
fn category_of(m: &MailMessage) -> Option<Category> {
    let from = m.sender()?;
    let (local, domain) = from.email.split_once('@').unwrap_or((&from.email, ""));
    let subject = &m.meta.subject;
    if NOTIFY_DOMAIN.is_match(domain) && (NOREPLY.is_match(local) || auto_submitted(m) || has_list_headers(m)) {
        return Some(Category::Notification);
    }
    if auto_submitted(m) && !has_list_headers(m) {
        return Some(Category::Notification);
    }
    let promo = PROMO_SENDER.is_match(local) || PROMO_SUBJECT.is_match(subject);
    if TRANSACTIONAL_SUBJECT.is_match(subject) && !promo {
        if TRANSACTIONAL_SENDER.is_match(local) || NOREPLY.is_match(local) || has_list_headers(m) {
            return Some(Category::Transactional);
        }
        if subject.contains(':') && subject.to_ascii_lowercase().starts_with("invitation") {
            return Some(Category::Transactional);
        }
    }
    if has_list_headers(m) {
        return Some(if promo { Category::Promotion } else { Category::Newsletter });
    }
    if PROMO_SENDER.is_match(local) && PROMO_SUBJECT.is_match(subject) {
        return Some(Category::Promotion);
    }
    if TRANSACTIONAL_SENDER.is_match(local) {
        return Some(Category::Transactional);
    }
    if NOREPLY.is_match(local) {
        return Some(Category::Notification);
    }
    None
}

fn label(c: Category) -> &'static str {
    match c {
        Category::Newsletter => "Newsletter",
        Category::Promotion => "Promotion",
        Category::Notification => "Notification",
        Category::Transactional => "Receipt or account notice",
        _ => "Mail",
    }
}

/// Fixed priorities for what rules decide (0 ignore .. 100 now).
pub fn rule_priority(c: Category) -> u8 {
    match c {
        Category::Sensitive => 60,
        Category::Transactional => 30,
        Category::Notification => 25,
        Category::Newsletter => 15,
        Category::Promotion => 5,
        _ => 40,
    }
}

/// The rules stage for a whole thread. `me` is the account's own address: a thread the user wrote in is
/// a conversation, so bulk-mail rules do not apply to it (sensitive rules always do).
pub fn triage_by_rules(thread: &MailThread, me: &str, now: i64) -> Option<Triage> {
    if let Some(line) = sensitive_line(thread) {
        return Some(Triage {
            category: Category::Sensitive,
            priority: rule_priority(Category::Sensitive),
            summary: line,
            needs_reply: false,
            due: None,
            by: BY_RULES.into(),
            at: now,
            suspicious: false,
        });
    }
    let user_wrote = thread.messages().iter().any(|m| m.sender().is_some_and(|a| a.email.eq_ignore_ascii_case(me)));
    if user_wrote {
        return None;
    }
    let m = thread.latest();
    let c = category_of(m)?;
    let x = m.sender().map(who).unwrap_or_else(|| "someone".into());
    Some(Triage {
        category: c,
        priority: rule_priority(c),
        summary: text::one_line(&format!("{} from {x}", label(c)), 120),
        needs_reply: false,
        due: None,
        by: BY_RULES.into(),
        at: now,
        suspicious: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::*;

    #[test]
    fn sensitive_comes_first_and_says_one_line() {
        let t = thread1(msg("noreply@acme.test", Some("Acme"), "Your verification code", "Your code is 123456", &[]));
        let tr = triage_by_rules(&t, ME, 9).unwrap();
        assert_eq!(tr.category, Category::Sensitive);
        assert_eq!(tr.summary, "A security code from Acme");
        assert_eq!(tr.by, "rules");
        assert!(!tr.summary.contains("123456"));
    }

    #[test]
    fn local_security_patterns_and_banks() {
        let t = thread1(msg("accounts@example.test", None, "New sign-in from Chrome on Linux", "hi", &[]));
        assert_eq!(sensitive_line(&t).unwrap(), "An account-security mail from example.test");
        let t = thread1(msg("alerts@mercury.com", Some("Mercury"), "Hello", "", &[]));
        assert_eq!(sensitive_line(&t).unwrap(), "Bank or money mail from Mercury");
        let t = thread1(msg("info@dmv.ca.gov", None, "Renewal", "", &[]));
        assert_eq!(sensitive_line(&t).unwrap(), "Government mail from dmv.ca.gov");
    }

    #[test]
    fn any_sensitive_message_makes_the_thread_sensitive() {
        let a = msg("ada@example.com", Some("Ada"), "Lunch", "Noon?", &[]);
        let mut b = msg("ada@example.com", Some("Ada"), "Re: Lunch", "Oh and here is the reset your password link", &[]);
        b.meta.subject = "Reset your password".into();
        b.meta.date = 2;
        let t = thread(vec![a, b]);
        assert!(sensitive_line(&t).is_some());
    }

    #[test]
    fn header_rules() {
        let nl = msg("weekly@news.example.org", Some("The Weekly"), "Issue 42: rivers", "...", &[("List-Id", "<weekly.news.example.org>")]);
        assert_eq!(triage_by_rules(&thread1(nl), ME, 0).unwrap().category, Category::Newsletter);
        let promo = msg("offers@shop.example", None, "40% off everything", "...", &[("List-Unsubscribe", "<mailto:u@shop.example>")]);
        assert_eq!(triage_by_rules(&thread1(promo), ME, 0).unwrap().category, Category::Promotion);
        let gh = msg("notifications@github.com", Some("Octo Bot"), "[acme/app] Build failed", "...", &[("X-GitHub-Reason", "ci_activity")]);
        let tr = triage_by_rules(&thread1(gh), ME, 0).unwrap();
        assert_eq!((tr.category, tr.summary.as_str()), (Category::Notification, "Notification from Octo Bot"));
        let receipt = msg("receipts@coffee.example", None, "Your receipt from Bean Co", "...", &[]);
        assert_eq!(triage_by_rules(&thread1(receipt), ME, 0).unwrap().category, Category::Transactional);
        let shipped = msg("no-reply@store.example", None, "Your order has shipped", "...", &[]);
        assert_eq!(triage_by_rules(&thread1(shipped), ME, 0).unwrap().category, Category::Transactional);
        let auto = msg("monitor@ops.example", None, "Disk at 91%", "...", &[("Auto-Submitted", "auto-generated")]);
        assert_eq!(triage_by_rules(&thread1(auto), ME, 0).unwrap().category, Category::Notification);
    }

    #[test]
    fn people_go_to_the_model() {
        let p = msg("ada@example.com", Some("Ada"), "Lunch on Friday?", "Noon?", &[]);
        assert!(triage_by_rules(&thread1(p), ME, 0).is_none());
    }

    #[test]
    fn a_thread_the_user_wrote_in_is_a_conversation() {
        let a = msg("news@list.example", None, "Meetup", "...", &[("List-Id", "<l.example>")]);
        let mut b = msg(ME, None, "Re: Meetup", "I'll come", &[]);
        b.meta.date = 5;
        assert!(triage_by_rules(&thread(vec![a, b]), ME, 0).is_none());
    }
}
