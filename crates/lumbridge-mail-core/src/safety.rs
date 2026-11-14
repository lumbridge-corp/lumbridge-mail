//! The hard rules about mail, as code every crate can call. Mail is hostile input: it is parsed,
//! trimmed and classified, never followed. These rules are not settings.

use crate::model::{Account, AiPolicy, Persona};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// Nothing sends mail except a person pressing Send (or approving a specific draft). The AI layer
/// writes drafts; the daemon refuses `Action::Send` from any caller that is not marked as a person.
pub const SEND_NEEDS_A_PERSON: &str = "sending needs a person";

/// Remote images, fonts and stylesheets are not loaded until the user asks, per message or per sender.
pub const REMOTE_CONTENT_BLOCKED_BY_DEFAULT: bool = true;

/// Where a model runs. On the wire and in config: `local` / `hosted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPlace {
    /// On our own machines.
    Local,
    /// Anyone else's API.
    Hosted,
}

/// May this account's mail be given to a model running at `place`? Anon accounts never leave our machines,
/// whatever their policy says.
pub fn ai_allowed(account: &Account, place: ModelPlace) -> bool {
    match (account.ai, place) {
        (AiPolicy::Off, _) => false,
        (_, ModelPlace::Local) => true,
        (AiPolicy::LocalOnly, ModelPlace::Hosted) => false,
        (AiPolicy::Any, ModelPlace::Hosted) => account.persona != Persona::Anon,
    }
}

/// The kinds [`sensitive_kind`] returns, for the one line that is shown ("A security code from X").
pub const SECURITY_CODE: &str = "A security code";
pub const SIGN_IN_LINK: &str = "A password or sign-in link";
pub const ACCOUNT_SECURITY: &str = "An account-security mail";
pub const GOVERNMENT: &str = "Government mail";
pub const MONEY: &str = "Bank or money mail";

/// How much of the text the rules read (subject and the start of the body).
const HEAD_CHARS: usize = 1500;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

// Every pattern matches whole words (`\b`), so "Hotpot dinner" is not an OTP and "otherwise.com" is not
// a bank. Where in doubt a pattern is wide: a false "sensitive" only hides a summary.
static CODE_SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(verification code|security code|one[- ]time|otp|2fa|two[- ]factor|log[- ]?in code|sign[- ]?in code|passcode|confirm your email|verify your email|confirmation code|access code|auth(entication)? code)\b")
});
static CODE_BODY: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(verification code|security code|one[- ]time password|one[- ]time code|your code is|your otp|confirmation code is|enter (this|the following) code)\b")
});
static LINK_SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(password reset|reset your password|reset password|password change[ds]?|change your password|magic link|sign[- ]?in link|log[- ]?in link|recover your account|account recovery)\b")
});
/// Account-security mail: new sign-ins, security alerts, 2-step changes.
static SECURITY: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\bnew sign[- ]?in\b|\bsecurity (alert|notice|notification|update)\b|\bunusual (sign[- ]?in|activity|login)\b|\bverify your (account|identity)\b|\blog(ged)?[- ]?in\b.{0,40}\b(attempt|new device|from a new|location)\b|\bnew (device|login|browser) (sign|log)|\b(2-step|two-step|2fa|two-factor|multi-factor|mfa)\b.{0,30}\b(turned|enabled|disabled|changed|added|removed)\b|\bapp password\b|\brecovery (email|phone|codes?)\b|\bpasskey (added|created|removed)\b|\baccount (was |has been )?(accessed|locked|suspended|compromised)\b|\bc[oó]digo\b")
});
static SECURITY_SENDER: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)^(security|account-?security|accounts?-?(protection|security)|no-?reply-?security|safety|trust|identity)([+.@-]|$)")
});
static GOV_DOMAIN: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(\.gov|\.mil|\.gov\.[a-z]{2}|\.gc\.ca)$|(^|\.)(irs|ssa|uscis|dmv|ftb|edd)\."));
/// A bank or money service as one label of the sender's domain (`alerts.chase.com`, `chase.co.uk`).
static BANK_DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)(^|\.)(chase|bankofamerica|bofa|wellsfargo|citi|citibank|capitalone|americanexpress|aexp|amex|discover|usbank|schwab|fidelity|vanguard|robinhood|paypal|venmo|sofi|ally|marcus|goldmansachs|synchrony|barclays|barclaycardus|hsbc|mercury|brex|ramp|wise|coinbase|interactivebrokers|ibkr|etrade|morganstanley|pnc|tdbank|truist|navyfederal|zelle|cash\.app|kraken|gemini|webull|plaid|creditkarma|experian|equifax|transunion)\.")
});
static MONEY_SUBJECT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(wire transfer|account statement|tax document|tax form|1099(-[a-z]+)?|w-2|w2 form)\b"));
static MONEY_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(statement is (ready|available)|account ending|card ending|direct deposit|available balance|transaction alert|fraud alert|routing number)\b")
});

/// Mail that is never given to any model and never quoted anywhere: security codes, password resets,
/// sign-in links, account-security alerts, bank and government mail. Plain rules on sender, subject and
/// the first lines of text, matched on word boundaries. Returns what kind it is, for the one line we do
/// show ("A security code from X"). See [`sensitive_sender_kind`] to include the sender's display name.
pub fn sensitive_kind(from_email: &str, subject: &str, text: &str) -> Option<&'static str> {
    sensitive_sender_kind(None, from_email, subject, text)
}

/// [`sensitive_kind`], also looking at the sender's display name ("Acme Security").
pub fn sensitive_sender_kind(from_name: Option<&str>, from_email: &str, subject: &str, text: &str) -> Option<&'static str> {
    let body: String = text.chars().take(HEAD_CHARS).collect();
    let from = from_email.trim().to_lowercase();
    let local = from.split('@').next().unwrap_or("");
    let domain = from.rsplit_once('@').map(|(_, d)| d).unwrap_or(&from);

    if CODE_SUBJECT.is_match(subject) || CODE_BODY.is_match(&body) {
        return Some(SECURITY_CODE);
    }
    if LINK_SUBJECT.is_match(subject) {
        return Some(SIGN_IN_LINK);
    }
    let head = format!("{subject}\n{body}");
    let name_says_security = from_name.is_some_and(|n| n.to_lowercase().contains("security"));
    if SECURITY.is_match(&head) || SECURITY_SENDER.is_match(local) || name_says_security {
        return Some(ACCOUNT_SECURITY);
    }
    if GOV_DOMAIN.is_match(domain) {
        return Some(GOVERNMENT);
    }
    if BANK_DOMAIN.is_match(domain) || MONEY_SUBJECT.is_match(subject) || MONEY_WORDS.is_match(&head) {
        return Some(MONEY);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn account(persona: Persona, ai: AiPolicy) -> Account {
        Account { id: AccountId::new("a"), persona, label: "A".into(), email: "a@b.c".into(), name: "A".into(),
                  provider: ProviderKind::Demo, color: None, ai, readonly: false }
    }

    #[test]
    fn anon_mail_never_goes_to_a_hosted_model() {
        assert!(!ai_allowed(&account(Persona::Anon, AiPolicy::Any), ModelPlace::Hosted));
        assert!(ai_allowed(&account(Persona::Anon, AiPolicy::Any), ModelPlace::Local));
        assert!(ai_allowed(&account(Persona::Work, AiPolicy::Any), ModelPlace::Hosted));
        assert!(!ai_allowed(&account(Persona::Work, AiPolicy::LocalOnly), ModelPlace::Hosted));
        assert!(!ai_allowed(&account(Persona::Work, AiPolicy::Off), ModelPlace::Local));
    }

    #[test]
    fn model_place_has_a_wire_name() {
        assert_eq!(serde_json::to_string(&ModelPlace::Hosted).unwrap(), "\"hosted\"");
        assert_eq!(serde_json::from_str::<ModelPlace>("\"local\"").unwrap(), ModelPlace::Local);
    }

    #[test]
    fn codes_resets_and_banks_are_sensitive() {
        assert_eq!(sensitive_kind("no-reply@x.com", "Your verification code", ""), Some(SECURITY_CODE));
        assert_eq!(sensitive_kind("a@x.com", "Hi", "Your code is 123456"), Some(SECURITY_CODE));
        assert_eq!(sensitive_kind("a@x.com", "Your OTP for login", ""), Some(SECURITY_CODE));
        assert_eq!(sensitive_kind("a@x.com", "Your one-time passcode", ""), Some(SECURITY_CODE));
        assert_eq!(sensitive_kind("a@x.com", "Reset your password", ""), Some(SIGN_IN_LINK));
        assert_eq!(sensitive_kind("alerts@chase.com", "Hello", ""), Some(MONEY));
        assert_eq!(sensitive_kind("alerts@email.chase.co.uk", "Hello", ""), Some(MONEY));
        assert_eq!(sensitive_kind("x@irs.gov", "Notice", ""), Some(GOVERNMENT));
        assert_eq!(sensitive_kind("info@dmv.ca.gov", "Renewal", ""), Some(GOVERNMENT));
        assert_eq!(sensitive_kind("a@x.com", "Your 1099-INT is ready", ""), Some(MONEY));
        assert_eq!(sensitive_kind("ada@example.com", "Lunch on Friday?", "See you at noon"), None);
    }

    #[test]
    fn patterns_carried_over_from_the_ai_rules() {
        assert_eq!(sensitive_kind("accounts@example.test", "New sign-in from Chrome on Linux", "hi"), Some(ACCOUNT_SECURITY));
        assert_eq!(sensitive_kind("security@example.test", "Hello", ""), Some(ACCOUNT_SECURITY));
        assert_eq!(sensitive_sender_kind(Some("Acme Security"), "x@acme.test", "Hello", ""), Some(ACCOUNT_SECURITY));
        assert_eq!(sensitive_kind("alerts@mercury.com", "Hello", ""), Some(MONEY));
        assert_eq!(sensitive_kind("a@x.com", "Update", "Your statement is ready for the card ending 1234"), Some(MONEY));
        assert_eq!(sensitive_kind("a@x.com", "2-step verification turned on", ""), Some(ACCOUNT_SECURITY));
    }

    #[test]
    fn whole_words_only() {
        assert_eq!(sensitive_kind("ada@example.com", "Hotpot dinner", "Bring chopsticks"), None);
        assert_eq!(sensitive_kind("ada@example.com", "Notes from the 2fast2furious watch party", ""), None);
        assert_eq!(sensitive_kind("bo@otherwise.com", "Hello", ""), None);
        assert_eq!(sensitive_kind("bo@purchase.example", "Hello", ""), None);
        assert_eq!(sensitive_kind("bo@discovery.example", "Field trip", ""), None);
        assert_eq!(sensitive_kind("bo@example.com", "Re: the otp", ""), Some(SECURITY_CODE));
    }
}
