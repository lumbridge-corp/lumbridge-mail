//! Adding an account the way a person does it (the app's first-run screen, `lbmail account add` with no
//! flags): server guesses from an address, an id made from the address, and a [`NewAccount`] turned into
//! a config entry. Pure, so it is tested without a daemon. Nothing here looks anything up on the network
//! (CONTRIBUTING.md: no new network calls); the person confirms the guess.

use crate::config::{AccountConfig, AuthKind, ProviderConfig};
use lumbridge_mail_core::rpc::{Discovered, NewAccount, NewProvider};
use lumbridge_mail_core::{AiPolicy, Error, Persona, Result};
use serde_json::json;

struct Known {
    domains: &'static [&'static str],
    name: &'static str,
    imap: (&'static str, u16, &'static str),
    smtp: (&'static str, u16, &'static str),
    note: Option<&'static str>,
}

const KNOWN: &[Known] = &[
    Known {
        domains: &["gmail.com", "googlemail.com"],
        name: "Gmail",
        imap: ("imap.gmail.com", 993, "tls"),
        smtp: ("smtp.gmail.com", 465, "tls"),
        note: Some("Gmail works best through gog (it already holds the sign-in). Over IMAP it needs an app password."),
    },
    Known {
        domains: &["fastmail.com", "fastmail.fm", "messagingengine.com"],
        name: "Fastmail",
        imap: ("imap.fastmail.com", 993, "tls"),
        smtp: ("smtp.fastmail.com", 465, "tls"),
        note: Some("Use an app password from Fastmail's settings, not your login password."),
    },
    Known {
        domains: &["icloud.com", "me.com", "mac.com"],
        name: "iCloud Mail",
        imap: ("imap.mail.me.com", 993, "tls"),
        smtp: ("smtp.mail.me.com", 587, "start_tls"),
        note: Some("iCloud needs an app-specific password (appleid.apple.com)."),
    },
    Known {
        domains: &["outlook.com", "hotmail.com", "live.com", "msn.com"],
        name: "Outlook.com",
        imap: ("outlook.office365.com", 993, "tls"),
        smtp: ("smtp-mail.outlook.com", 587, "start_tls"),
        note: Some("Microsoft turns password sign-in off for most accounts; it may need OAuth2, which is not built yet."),
    },
    Known {
        domains: &["yahoo.com", "ymail.com"],
        name: "Yahoo Mail",
        imap: ("imap.mail.yahoo.com", 993, "tls"),
        smtp: ("smtp.mail.yahoo.com", 465, "tls"),
        note: Some("Yahoo needs an app password."),
    },
    Known {
        domains: &["proton.me", "protonmail.com", "protonmail.ch", "pm.me"],
        name: "Proton Mail (Bridge)",
        imap: ("127.0.0.1", 1143, "start_tls"),
        smtp: ("127.0.0.1", 1025, "start_tls"),
        note: Some("Proton works through Proton Mail Bridge on this machine; use the password the Bridge shows."),
    },
    Known {
        domains: &["gmx.com", "gmx.net", "gmx.de"],
        name: "GMX",
        imap: ("imap.gmx.com", 993, "tls"),
        smtp: ("mail.gmx.com", 587, "start_tls"),
        note: None,
    },
    Known {
        domains: &["zoho.com", "zohomail.com"],
        name: "Zoho Mail",
        imap: ("imap.zoho.com", 993, "tls"),
        smtp: ("smtp.zoho.com", 465, "tls"),
        note: None,
    },
];

fn domain_of(email: &str) -> Result<String> {
    let email = email.trim().to_ascii_lowercase();
    match email.rsplit_once('@') {
        Some((local, domain)) if !local.is_empty() && domain.contains('.') && !domain.starts_with('.') => Ok(domain.to_owned()),
        _ => Err(Error::Invalid(format!("{email:?} is not an email address"))),
    }
}

/// The servers for `email`: a known provider's, else `imap.<domain>` (993, TLS) and `smtp.<domain>`
/// (587, STARTTLS), the most common setup.
pub fn discover(email: &str) -> Result<Discovered> {
    let domain = domain_of(email)?;
    if let Some(k) = KNOWN.iter().find(|k| k.domains.contains(&domain.as_str())) {
        return Ok(Discovered {
            imap_host: k.imap.0.into(),
            imap_port: k.imap.1,
            imap_security: k.imap.2.into(),
            smtp_host: k.smtp.0.into(),
            smtp_port: k.smtp.1,
            smtp_security: k.smtp.2.into(),
            provider: Some(k.name.into()),
            note: k.note.map(Into::into),
        });
    }
    Ok(Discovered {
        imap_host: format!("imap.{domain}"),
        imap_port: 993,
        imap_security: "tls".into(),
        smtp_host: format!("smtp.{domain}"),
        smtp_port: 587,
        smtp_security: "start_tls".into(),
        provider: None,
        note: Some(format!("A guess from {domain}: check it against your provider's help page.")),
    })
}

/// An account id from an address (`sam@work.example` -> `sam-work`), unique among `taken`.
pub fn make_id(email: &str, taken: &[String]) -> String {
    let email = email.trim().to_ascii_lowercase();
    let (local, domain) = email.split_once('@').unwrap_or((email.as_str(), ""));
    let first = domain.split('.').next().unwrap_or("");
    let raw = if first.is_empty() { local.to_owned() } else { format!("{local}-{first}") };
    let mut slug: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let mut slug: String = slug.trim_matches('-').chars().take(48).collect();
    if slug.is_empty() {
        slug = "account".into();
    }
    if !taken.contains(&slug) {
        return slug;
    }
    (2..).map(|n| format!("{slug}-{n}")).find(|c| !taken.contains(c)).expect("some number is free")
}

fn security(s: Option<&str>, default: &str) -> Result<lumbridge_mail_imap::Security> {
    let s = s.unwrap_or(default);
    serde_json::from_value(json!(s)).map_err(|_| Error::Invalid(format!("security must be tls, start_tls or plain, not {s:?}")))
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// The config entry for a new account. `taken` are the ids already in the config.
pub fn account_config(n: &NewAccount, taken: &[String]) -> Result<AccountConfig> {
    domain_of(&n.email)?;
    let id = match &n.id {
        Some(id) if !id.trim().is_empty() => id.trim().to_owned(),
        _ => make_id(&n.email, taken),
    };
    if taken.contains(&id) {
        return Err(Error::Invalid(format!("there is already an account {id}")));
    }
    let provider = match &n.provider {
        NewProvider::Demo { set } => ProviderConfig::Demo { set: set.clone() },
        NewProvider::Gog { binary, ssh_host } => ProviderConfig::Gog {
            binary: binary.clone().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| "gog".into()),
            ssh_host: ssh_host.clone().filter(|h| !h.trim().is_empty()),
            env_file: None,
            options: Default::default(),
        },
        NewProvider::Imap { imap_host, imap_port, imap_security, smtp_host, smtp_port, smtp_security, username } => {
            if imap_host.trim().is_empty() {
                return Err(Error::Invalid("the IMAP server is needed".into()));
            }
            let smtp_host = smtp_host.clone().filter(|h| !h.trim().is_empty());
            ProviderConfig::Imap {
                imap_host: imap_host.trim().to_owned(),
                imap_port: *imap_port,
                imap_security: security(imap_security.as_deref(), "tls")?,
                // A Proton Bridge on this machine has a self-signed certificate.
                accept_invalid_certs: is_loopback(imap_host.trim()),
                smtp_host,
                smtp_port: *smtp_port,
                smtp_security: security(smtp_security.as_deref(), "tls")?,
                username: username.clone().filter(|u| !u.trim().is_empty()),
                auth: AuthKind::Password,
                password_env: None,
                password_command: None,
                server_saves_sent: None,
            }
        }
        NewProvider::Resend { addresses, receiving } => {
            for a in addresses {
                if lumbridge_mail_resend::AddressPattern::parse(a).is_none() {
                    return Err(Error::Invalid(format!("{a:?} is not an address or @domain")));
                }
            }
            ProviderConfig::Resend {
                base_url: None,
                addresses: addresses.iter().map(|a| a.trim().to_ascii_lowercase()).filter(|a| !a.is_empty()).collect(),
                receiving: receiving.unwrap_or(true),
                reply_to: None,
                api_key_env: None,
                api_key_command: None,
                requests_per_second: None,
            }
        }
    };
    let ai = if n.persona == Persona::Anon { AiPolicy::LocalOnly } else { AiPolicy::default() };
    let label = if n.label.trim().is_empty() {
        match n.persona {
            Persona::Personal => "Personal".to_owned(),
            Persona::Work => "Work".to_owned(),
            _ => id.clone(),
        }
    } else {
        n.label.trim().to_owned()
    };
    Ok(AccountConfig {
        id,
        persona: n.persona.clone(),
        label,
        email: n.email.trim().to_ascii_lowercase(),
        name: n.name.trim().to_owned(),
        color: None,
        ai,
        readonly: n.readonly,
        mirror: true,
        body_fill: None,
        body_fill_per_min: None,
        provider,
    })
}

/// The three made-up accounts "Try with demo mail" adds (personal, work, anon).
pub fn demo_accounts() -> Vec<NewAccount> {
    let one = |id: &str, persona: Persona, label: &str, email: &str, name: &str, set: &str| NewAccount {
        id: Some(id.into()),
        persona,
        label: label.into(),
        email: email.into(),
        name: name.into(),
        readonly: false,
        provider: NewProvider::Demo { set: Some(set.into()) },
        password: None,
    };
    vec![
        one("demo-personal", Persona::Personal, "Personal (demo)", "sam@example.com", "Sam Example", "personal"),
        one("demo-work", Persona::Work, "Work (demo)", "sam@work.example", "Sam Example", "work"),
        one("demo-anon", Persona::Anon, "Forum (demo)", "otter@anon.example", "Otter", "anon"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_and_a_guess() {
        let f = discover("Sam@Fastmail.com").unwrap();
        assert_eq!((f.imap_host.as_str(), f.imap_port, f.imap_security.as_str()), ("imap.fastmail.com", 993, "tls"));
        assert_eq!(f.provider.as_deref(), Some("Fastmail"));
        let p = discover("otter@proton.me").unwrap();
        assert_eq!(p.imap_host, "127.0.0.1");
        let g = discover("sam@work.example").unwrap();
        assert_eq!(g.imap_host, "imap.work.example");
        assert_eq!((g.smtp_host.as_str(), g.smtp_port, g.smtp_security.as_str()), ("smtp.work.example", 587, "start_tls"));
        assert!(g.provider.is_none() && g.note.is_some());
        assert!(discover("not-an-address").is_err());
        assert!(discover("a@nodot").is_err());
    }

    #[test]
    fn ids_come_from_the_address_and_are_unique() {
        assert_eq!(make_id("sam@work.example", &[]), "sam-work");
        assert_eq!(make_id("Sam.Example+x@mail.example", &[]), "sam-example-x-mail");
        assert_eq!(make_id("sam@work.example", &["sam-work".into()]), "sam-work-2");
        assert_eq!(make_id("sam@work.example", &["sam-work".into(), "sam-work-2".into()]), "sam-work-3");
    }

    #[test]
    fn new_accounts_become_config_entries() {
        let gog = NewAccount {
            id: None,
            persona: Persona::Personal,
            label: String::new(),
            email: "sam@example.com".into(),
            name: "Sam Example".into(),
            readonly: true,
            provider: NewProvider::Gog { binary: None, ssh_host: Some("gog-host".into()) },
            password: None,
        };
        let c = account_config(&gog, &[]).unwrap();
        assert_eq!(c.id, "sam-example");
        assert_eq!(c.label, "Personal");
        assert!(c.readonly);
        assert_eq!(c.provider, ProviderConfig::Gog { binary: "gog".into(), ssh_host: Some("gog-host".into()), env_file: None, options: Default::default() });
        assert!(account_config(&gog, &["sam-example".into()]).is_ok(), "a free id is picked");
        let taken = NewAccount { id: Some("work".into()), ..gog.clone() };
        assert!(account_config(&taken, &["work".into()]).is_err());
        let imap = NewAccount {
            provider: NewProvider::Imap {
                imap_host: "127.0.0.1".into(),
                imap_port: Some(1143),
                imap_security: Some("start_tls".into()),
                smtp_host: Some(String::new()),
                smtp_port: None,
                smtp_security: None,
                username: None,
            },
            persona: Persona::Anon,
            readonly: false,
            ..gog.clone()
        };
        let c = account_config(&imap, &[]).unwrap();
        assert_eq!(c.ai, AiPolicy::LocalOnly);
        match c.provider {
            ProviderConfig::Imap { accept_invalid_certs, smtp_host, imap_security, .. } => {
                assert!(accept_invalid_certs, "a loopback Bridge");
                assert!(smtp_host.is_none(), "an empty SMTP host means no sending");
                assert_eq!(imap_security, lumbridge_mail_imap::Security::StartTls);
            }
            other => panic!("{other:?}"),
        }
        let bad = NewAccount { provider: NewProvider::Imap { imap_host: " ".into(), imap_port: None, imap_security: None, smtp_host: None, smtp_port: None, smtp_security: None, username: None }, ..gog };
        assert!(account_config(&bad, &[]).is_err());
        assert_eq!(demo_accounts().len(), 3);
        let resend = NewAccount {
            id: None,
            persona: Persona::Work,
            label: "Shop".into(),
            email: "hello@shop.example".into(),
            name: "Shop".into(),
            readonly: false,
            provider: NewProvider::Resend { addresses: vec![" @Shop.example ".into()], receiving: Some(false) },
            password: None,
        };
        match account_config(&resend, &[]).unwrap().provider {
            ProviderConfig::Resend { addresses, receiving, api_key_env, .. } => {
                assert_eq!(addresses, vec!["@shop.example"]);
                assert!(!receiving && api_key_env.is_none(), "the key goes to the keyring, not the config");
            }
            other => panic!("{other:?}"),
        }
        let bad = NewAccount { provider: NewProvider::Resend { addresses: vec!["@".into()], receiving: None }, ..resend };
        assert!(account_config(&bad, &[]).is_err());
    }
}
