//! Folder roles and display names. Roles come from SPECIAL-USE (RFC 6154) attributes when the server sends
//! them, else from well-known names (English plus a few common localisations), including Gmail's
//! `[Gmail]/...` and Proton Mail Bridge's layout (`Folders/...`, `Labels/...`, `All Mail`, `Starred`).

use base64::Engine as _;
use lumbridge_mail_core::MailboxRole;

/// One folder as LIST reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// The server's name, as sent (modified UTF-7). Used as the `MailboxId`.
    pub name: String,
    pub delimiter: Option<String>,
    /// Lower-cased attributes without the backslash, e.g. `sent`, `noselect`.
    pub attributes: Vec<String>,
    pub role: MailboxRole,
}

impl Folder {
    pub fn new(name: &str, delimiter: Option<&str>, attributes: Vec<String>) -> Self {
        let role = role_for(name, delimiter, &attributes);
        Self { name: name.to_owned(), delimiter: delimiter.map(str::to_owned), attributes, role }
    }

    pub fn selectable(&self) -> bool {
        !self.attributes.iter().any(|a| a == "noselect" || a == "nonexistent")
    }

    pub fn display(&self) -> String {
        display_name(&self.name, self.delimiter.as_deref())
    }
}

fn special_use(attributes: &[String]) -> Option<MailboxRole> {
    attributes.iter().find_map(|a| match a.as_str() {
        "sent" => Some(MailboxRole::Sent),
        "drafts" => Some(MailboxRole::Drafts),
        "trash" => Some(MailboxRole::Trash),
        "junk" => Some(MailboxRole::Spam),
        "archive" => Some(MailboxRole::Archive),
        "all" => Some(MailboxRole::All),
        "flagged" => Some(MailboxRole::Starred),
        "important" => Some(MailboxRole::Important),
        _ => None,
    })
}

/// The role for a folder.
pub fn role_for(name: &str, delimiter: Option<&str>, attributes: &[String]) -> MailboxRole {
    if name.eq_ignore_ascii_case("INBOX") {
        return MailboxRole::Inbox;
    }
    if let Some(r) = special_use(attributes) {
        return r;
    }
    let decoded = decode_mutf7(name);
    let lower = decoded.to_lowercase();
    // Gmail / Google Mail top-level container.
    let inner = lower.strip_prefix("[gmail]/").or_else(|| lower.strip_prefix("[google mail]/"));
    let leaf = match inner {
        Some(rest) => rest.to_owned(),
        None => {
            // `INBOX.Sent`, `INBOX/Trash` (Courier, some cPanel hosts) count as top level.
            let mut leaf = lower.clone();
            if let Some(d) = delimiter.filter(|d| !d.is_empty()) {
                let prefix = format!("inbox{d}");
                if let Some(rest) = leaf.strip_prefix(&prefix) {
                    leaf = rest.to_owned();
                }
                if leaf.contains(d) {
                    // Nested user folders never get a role by name.
                    return MailboxRole::Custom;
                }
            }
            leaf
        }
    };
    match leaf.as_str() {
        "sent" | "sent mail" | "sent items" | "sent messages" | "sent-mail" | "outbox sent" | "gesendet"
        | "gesendete elemente" | "envoyés" | "messages envoyés" | "enviados" | "elementos enviados" | "inviata"
        | "posta inviata" | "verzonden" | "verzonden items" | "skickat" => MailboxRole::Sent,
        "drafts" | "draft" | "entwürfe" | "brouillons" | "borradores" | "bozze" | "concepten" | "utkast" => {
            MailboxRole::Drafts
        }
        "trash" | "bin" | "deleted" | "deleted items" | "deleted messages" | "papierkorb" | "gelöschte elemente"
        | "corbeille" | "papelera" | "cestino" | "prullenbak" | "papperskorgen" => MailboxRole::Trash,
        "spam" | "junk" | "junk e-mail" | "junk email" | "junk mail" | "bulk mail" | "spamverdacht"
        | "indésirables" | "correo no deseado" | "posta indesiderata" | "ongewenste e-mail" | "skräppost" => {
            MailboxRole::Spam
        }
        "archive" | "archives" | "archiv" | "archivio" | "archivo" | "archief" | "arkiv" => MailboxRole::Archive,
        "all mail" | "alle nachrichten" => MailboxRole::All,
        "starred" | "flagged" | "markiert" => MailboxRole::Starred,
        "important" => MailboxRole::Important,
        _ => MailboxRole::Custom,
    }
}

/// What to show: decoded from modified UTF-7, the server's delimiter turned into `/`, `INBOX` as `Inbox`,
/// Gmail's `[Gmail]/` and Proton Bridge's `Folders/` prefixes dropped.
pub fn display_name(name: &str, delimiter: Option<&str>) -> String {
    if name.eq_ignore_ascii_case("INBOX") {
        return "Inbox".into();
    }
    let mut s = decode_mutf7(name);
    if let Some(d) = delimiter.filter(|d| !d.is_empty() && *d != "/") {
        s = s.replace(d, "/");
    }
    for prefix in ["[Gmail]/", "[Google Mail]/", "Folders/"] {
        if let Some(rest) = s.strip_prefix(prefix)
            && !rest.is_empty()
        {
            s = rest.to_owned();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("INBOX/") {
        s = format!("Inbox/{rest}");
    }
    s
}

/// Modified UTF-7 (RFC 3501 §5.1.3) to UTF-8. Invalid sequences are kept as written.
pub fn decode_mutf7(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(dash) = after.find('-') else {
            out.push_str(&rest[amp..]);
            return out;
        };
        let chunk = &after[..dash];
        if chunk.is_empty() {
            out.push('&');
        } else {
            match decode_chunk(chunk) {
                Some(text) => out.push_str(&text),
                None => out.push_str(&rest[amp..amp + 1 + dash + 1]),
            }
        }
        rest = &after[dash + 1..];
    }
    out.push_str(rest);
    out
}

fn decode_chunk(chunk: &str) -> Option<String> {
    let b64 = chunk.replace(',', "/");
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64.as_bytes()).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16(&units).ok()
}

/// Order folders are synced in: Inbox first, then the ones people look at most.
pub fn sync_rank(role: MailboxRole) -> u8 {
    match role {
        MailboxRole::Inbox => 0,
        MailboxRole::Sent => 1,
        MailboxRole::Drafts => 2,
        MailboxRole::Archive => 3,
        MailboxRole::Custom => 4,
        MailboxRole::Starred | MailboxRole::Important | MailboxRole::All => 5,
        MailboxRole::Spam => 6,
        MailboxRole::Trash => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, attrs: &[&str]) -> MailboxRole {
        role_for(name, Some("/"), &attrs.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn special_use_wins() {
        assert_eq!(f("Gesendet", &["sent"]), MailboxRole::Sent);
        assert_eq!(f("Odd Name", &["junk"]), MailboxRole::Spam);
        assert_eq!(f("[Gmail]/All Mail", &["all", "hasnochildren"]), MailboxRole::All);
        assert_eq!(f("[Gmail]/Starred", &["flagged"]), MailboxRole::Starred);
        assert_eq!(f("inbox", &[]), MailboxRole::Inbox);
        // A folder literally called Sent but marked as something else.
        assert_eq!(f("Sent", &["archive"]), MailboxRole::Archive);
    }

    #[test]
    fn names_are_a_fallback() {
        assert_eq!(f("Sent Items", &[]), MailboxRole::Sent);
        assert_eq!(f("Deleted Items", &[]), MailboxRole::Trash);
        assert_eq!(f("Junk E-mail", &[]), MailboxRole::Spam);
        assert_eq!(f("[Gmail]/Sent Mail", &[]), MailboxRole::Sent);
        assert_eq!(f("[Google Mail]/Bin", &[]), MailboxRole::Trash);
        assert_eq!(f("[Gmail]/Important", &[]), MailboxRole::Important);
        assert_eq!(role_for("INBOX.Drafts", Some("."), &[]), MailboxRole::Drafts);
        assert_eq!(f("Projects/Sent", &[]), MailboxRole::Custom);
        assert_eq!(f("Receipts", &[]), MailboxRole::Custom);
        // Proton Mail Bridge.
        assert_eq!(f("All Mail", &[]), MailboxRole::All);
        assert_eq!(f("Starred", &[]), MailboxRole::Starred);
        assert_eq!(f("Archive", &[]), MailboxRole::Archive);
        assert_eq!(f("Folders/Receipts", &[]), MailboxRole::Custom);
        assert_eq!(f("Labels/Travel", &[]), MailboxRole::Custom);
        // Localised, written in modified UTF-7.
        assert_eq!(f("Entw&APw-rfe", &[]), MailboxRole::Drafts);
    }

    #[test]
    fn display_names() {
        assert_eq!(display_name("INBOX", Some("/")), "Inbox");
        assert_eq!(display_name("[Gmail]/Sent Mail", Some("/")), "Sent Mail");
        assert_eq!(display_name("INBOX.Lists.Rust", Some(".")), "Inbox/Lists/Rust");
        assert_eq!(display_name("Folders/Receipts", Some("/")), "Receipts");
        assert_eq!(display_name("Labels/Travel", Some("/")), "Labels/Travel");
        assert_eq!(display_name("Entw&APw-rfe", Some("/")), "Entwürfe");
    }

    #[test]
    fn mutf7() {
        assert_eq!(decode_mutf7("Hello"), "Hello");
        assert_eq!(decode_mutf7("A&-B"), "A&B");
        assert_eq!(decode_mutf7("&ZeVnLIqe-"), "日本語");
        assert_eq!(decode_mutf7("&U,BTFw-"), "台北");
        assert_eq!(decode_mutf7("bad&!!!-x"), "bad&!!!-x");
        assert_eq!(decode_mutf7("open&abc"), "open&abc");
    }
}
