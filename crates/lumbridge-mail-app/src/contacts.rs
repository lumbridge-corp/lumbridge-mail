//! Recipient suggestions in the composer, from the people in the mail the app has seen (and whatever the
//! backend knows, see `Backend::contacts`).
//!
//! The anon wall (CONTRIBUTING.md rule 4): every address is remembered with the account it was seen in. Writing
//! from an anon account suggests only people from that account; writing from any other account suggests
//! only people from the non-anon accounts. So an anon contact never shows up in a personal or work
//! draft, and the other way round.

use lumbridge_mail_core::{Account, AccountId, Address, Persona};
use std::collections::HashMap;

#[derive(Debug, Clone)]
struct Entry {
    addr: Address,
    count: u32,
    last: i64,
}

#[derive(Debug, Default, Clone)]
pub struct Contacts {
    by_account: HashMap<AccountId, HashMap<String, Entry>>,
}

/// Lower-case words a query can match from the start: each name word, the address, its local part.
fn keys(a: &Address) -> Vec<String> {
    let mut k: Vec<String> = a.name.iter().flat_map(|n| n.split_whitespace()).map(str::to_lowercase).collect();
    if let Some(n) = &a.name {
        k.push(n.to_lowercase());
    }
    k.push(a.email.clone());
    if let Some((local, _)) = a.email.split_once('@') {
        k.extend(local.split(['.', '_', '-', '+']).map(str::to_owned));
    }
    k
}

impl Contacts {
    pub fn learn(&mut self, account: &AccountId, addr: &Address, at: i64) {
        if !addr.email.contains('@') {
            return;
        }
        let book = self.by_account.entry(account.clone()).or_default();
        let e = book.entry(addr.email.clone()).or_insert_with(|| Entry { addr: addr.clone(), count: 0, last: at });
        e.count += 1;
        e.last = e.last.max(at);
        // Prefer a version with a name.
        if e.addr.name.is_none() && addr.name.is_some() {
            e.addr.name = addr.name.clone();
        }
    }

    pub fn len(&self) -> usize {
        self.by_account.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Up to `limit` people for `query`, written from `from`. `skip` are already on the draft.
    pub fn suggest(&self, accounts: &[Account], from: &AccountId, query: &str, skip: &[Address], limit: usize) -> Vec<Address> {
        let q = query.trim().trim_start_matches(['"', '<']).to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let anon = |id: &AccountId| accounts.iter().any(|a| &a.id == id && a.persona == Persona::Anon);
        let from_anon = anon(from);
        let own: Vec<&str> = accounts.iter().map(|a| a.email.as_str()).collect();
        let mut merged: HashMap<&str, (Address, u32, i64)> = HashMap::new();
        for (acct, book) in &self.by_account {
            // The wall: anon writes only to its own people; the rest never see anon's.
            let allowed = if from_anon { acct == from } else { !anon(acct) && accounts.iter().any(|a| &a.id == acct) };
            if !allowed {
                continue;
            }
            for e in book.values() {
                let m = merged.entry(e.addr.email.as_str()).or_insert((e.addr.clone(), 0, e.last));
                m.1 += e.count;
                m.2 = m.2.max(e.last);
                if m.0.name.is_none() {
                    m.0.name = e.addr.name.clone();
                }
            }
        }
        let mut hits: Vec<(Address, u32, i64)> = merged
            .into_values()
            .filter(|(a, ..)| !own.contains(&a.email.as_str()) && !skip.iter().any(|s| s.email == a.email))
            .filter(|(a, ..)| keys(a).iter().any(|k| k.starts_with(&q)))
            .collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.email.cmp(&b.0.email)));
        hits.into_iter().take(limit).map(|(a, ..)| a).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumbridge_mail_core::{AiPolicy, ProviderKind};

    fn account(id: &str, persona: Persona, email: &str) -> Account {
        Account { id: AccountId::new(id), persona, label: id.into(), email: email.into(), name: "Sam".into(),
                  provider: ProviderKind::Demo, color: None, ai: AiPolicy::LocalOnly, readonly: false }
    }

    fn setup() -> (Vec<Account>, Contacts) {
        let accounts = vec![
            account("personal", Persona::Personal, "sam@inkwell.example"),
            account("work", Persona::Work, "sam@northwind.example"),
            account("anon", Persona::Anon, "otter@relay.example"),
        ];
        let mut c = Contacts::default();
        let (p, w, a) = (AccountId::new("personal"), AccountId::new("work"), AccountId::new("anon"));
        c.learn(&p, &Address::new(Some("Maya Chen"), "maya.chen@postbox.example"), 10);
        c.learn(&p, &Address::new(Some("Maya Chen"), "maya.chen@postbox.example"), 20);
        c.learn(&p, &Address::new(Some("Marco Polo"), "marco@post.example"), 30);
        c.learn(&w, &Address::new(Some("Dana Okafor"), "dana@northwind.example"), 5);
        c.learn(&a, &Address::new(Some("kestrel_92"), "kestrel92@relay.example"), 5);
        c.learn(&a, &Address::new(Some("Maya Anon"), "maya.chen@postbox.example"), 5);
        c.learn(&p, &Address::new(Some("Sam"), "sam@inkwell.example"), 5);
        (accounts, c)
    }

    #[test]
    fn suggestions_match_names_and_addresses_most_used_first() {
        let (accounts, c) = setup();
        let p = AccountId::new("personal");
        let names: Vec<String> = c.suggest(&accounts, &p, "ma", &[], 5).into_iter().map(|a| a.email).collect();
        assert_eq!(names, vec!["maya.chen@postbox.example", "marco@post.example"]);
        assert_eq!(c.suggest(&accounts, &p, "chen", &[], 5).len(), 1);
        assert_eq!(c.suggest(&accounts, &p, "dana", &[], 5)[0].email, "dana@northwind.example", "work people in personal drafts");
        assert!(c.suggest(&accounts, &p, "sam", &[], 5).is_empty(), "never yourself");
        let skip = [Address::new(None, "maya.chen@postbox.example")];
        assert_eq!(c.suggest(&accounts, &p, "ma", &skip, 5).len(), 1, "not twice");
        assert!(c.suggest(&accounts, &p, "", &[], 5).is_empty());
    }

    #[test]
    fn the_anon_wall_holds_both_ways() {
        let (accounts, c) = setup();
        let (p, w, a) = (AccountId::new("personal"), AccountId::new("work"), AccountId::new("anon"));
        assert!(c.suggest(&accounts, &p, "kes", &[], 5).is_empty(), "anon contacts stay out of personal");
        assert!(c.suggest(&accounts, &w, "kes", &[], 5).is_empty(), "and out of work");
        assert_eq!(c.suggest(&accounts, &a, "kes", &[], 5).len(), 1);
        assert!(c.suggest(&accounts, &a, "dana", &[], 5).is_empty(), "work contacts stay out of anon");
        // Maya was seen in both; from anon she shows as anon saw her, not with personal's history.
        let m = c.suggest(&accounts, &a, "maya", &[], 5);
        assert_eq!(m[0].name.as_deref(), Some("Maya Anon"));
    }
}
