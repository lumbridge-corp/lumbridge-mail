//! The search language: free words and `"phrases"` plus Gmail-style operators, parsed into a
//! [`SearchQuery`], then turned into an FTS5 expression (with prefix matching and typo tolerance) and SQL
//! conditions on messages.
//!
//! Operators: `from:` `to:` (also `cc:` `bcc:`) `subject:` `filename:` `has:attachment` `is:unread`
//! `is:read` `is:starred` (`is:flagged`) `is:answered` `before:YYYY-MM-DD` `after:YYYY-MM-DD`
//! `newer_than:` / `older_than:` (`36h`, `3d`, `2w`, `6m`, `1y`), `category:` (a triage category such as
//! `newsletter`, or one of Gmail's tabs: `promotions`, `social`, `updates`, `forums`, `primary`) and
//! `in:<role|mailbox name|mailbox id>` (also `label:`). Values may be quoted: `subject:"weekly report"`.
//! Unknown operators are searched as plain words.
//!
//! All parts must match one message (AND). `-part` must not match (`-from:ci@example.com is:unread`).
//! `a OR b` matches either (`from:ada OR from:grace subject:report`: OR joins the parts next to it, and
//! binds tighter than the AND between parts). `AND` and parentheses are allowed and change nothing.

use crate::Db;
use lumbridge_mail_core::Result;
use rusqlite::Connection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// Matched as a prefix, with typo tolerance when nothing starts with it.
    Word(String),
    /// Matched exactly, in order.
    Phrase(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    pub terms: Vec<Term>,
    pub from: Vec<String>,
    /// To, Cc or Bcc.
    pub to: Vec<String>,
    pub subject: Vec<String>,
    pub filename: Vec<String>,
    pub has_attachment: bool,
    /// `is:unread` = Some(true), `is:read` = Some(false).
    pub unread: Option<bool>,
    pub starred: Option<bool>,
    pub answered: Option<bool>,
    /// Messages strictly before this time (Unix seconds): `before:` (a UTC day's start), `older_than:`.
    pub before: Option<i64>,
    /// Messages at or after this time: `after:` (a UTC day's start), `newer_than:`.
    pub after: Option<i64>,
    /// `in:` values: a role name (`inbox`, `sent`, `trash`, ...), a mailbox name or a mailbox id.
    pub in_mailboxes: Vec<String>,
    /// `category:` values (lower case).
    pub categories: Vec<String>,
    /// Groups of parts joined by `OR`: at least one part of each group must match.
    pub any_of: Vec<Vec<Alt>>,
    /// Parts given with `-`: none may match.
    pub none_of: Vec<SearchQuery>,
}

/// One part of an `OR` group: a single operator or word, maybe negated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alt {
    pub negated: bool,
    pub part: SearchQuery,
}

impl SearchQuery {
    /// True when only flags, dates or mailboxes are asked for (no full-text part).
    pub fn has_text(&self) -> bool {
        !(self.terms.is_empty() && self.from.is_empty() && self.to.is_empty() && self.subject.is_empty() && self.filename.is_empty())
    }

    /// Adds every part of `other` (AND).
    fn merge(&mut self, other: SearchQuery) {
        self.terms.extend(other.terms);
        self.from.extend(other.from);
        self.to.extend(other.to);
        self.subject.extend(other.subject);
        self.filename.extend(other.filename);
        self.has_attachment |= other.has_attachment;
        self.unread = other.unread.or(self.unread);
        self.starred = other.starred.or(self.starred);
        self.answered = other.answered.or(self.answered);
        self.before = match (self.before, other.before) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.after = match (self.after, other.after) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.in_mailboxes.extend(other.in_mailboxes);
        self.categories.extend(other.categories);
        self.any_of.extend(other.any_of);
        self.none_of.extend(other.none_of);
    }
}

const KEYS: &[&str] =
    &["from", "to", "cc", "bcc", "subject", "filename", "has", "is", "before", "after", "in", "label", "newer_than", "older_than", "category"];

/// Reads a quoted string starting after the opening quote; returns it and the index after the closing quote.
fn quoted(c: &[char], mut i: usize) -> (String, usize) {
    let start = i;
    while i < c.len() && c[i] != '"' {
        i += 1;
    }
    let s: String = c[start..i].iter().collect();
    (s, (i + 1).min(c.len()))
}

/// Parses the text of a search box. Never fails: anything it does not understand is searched as words.
/// `newer_than:` / `older_than:` count back from now.
pub fn parse_search(input: &str) -> SearchQuery {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    parse_search_at(input, now)
}

/// One token of the search text.
enum Token {
    Part { negated: bool, part: Box<SearchQuery> },
    Or,
}

/// [`parse_search`] with `now` (Unix seconds) for the relative operators.
pub fn parse_search_at(input: &str, now: i64) -> SearchQuery {
    let c: Vec<char> = input.chars().collect();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0;
    while i < c.len() {
        // Parentheses only group, and OR already binds the parts next to it.
        if c[i].is_whitespace() || c[i] == '(' || c[i] == ')' {
            i += 1;
            continue;
        }
        let negated = c[i] == '-' && c.get(i + 1).is_some_and(|n| !n.is_whitespace() && *n != '-' && *n != '(' && *n != ')');
        if negated {
            i += 1;
        }
        let mut part = SearchQuery::default();
        if c[i] == '"' {
            let (v, next) = quoted(&c, i + 1);
            i = next;
            if !v.trim().is_empty() {
                part.terms.push(Term::Phrase(v.trim().to_lowercase()));
            }
            tokens.push(Token::Part { negated, part: Box::new(part) });
            continue;
        }
        let start = i;
        while i < c.len() && !c[i].is_whitespace() && c[i] != ':' && c[i] != '"' {
            i += 1;
        }
        let head: String = c[start..i].iter().collect::<String>().to_lowercase();
        if i < c.len() && c[i] == ':' && KEYS.contains(&head.as_str()) {
            i += 1;
            let value = if i < c.len() && c[i] == '"' {
                let (v, next) = quoted(&c, i + 1);
                i = next;
                v
            } else {
                let s = i;
                while i < c.len() && !c[i].is_whitespace() && c[i] != ')' {
                    i += 1;
                }
                c[s..i].iter().collect()
            };
            let value = value.trim().to_lowercase();
            if !value.is_empty() {
                operator(&mut part, &head, value, now);
            }
            tokens.push(Token::Part { negated, part: Box::new(part) });
            continue;
        }
        while i < c.len() && !c[i].is_whitespace() && c[i] != ')' {
            i += 1;
        }
        let raw: String = c[start..i].iter().collect();
        match raw.as_str() {
            "OR" | "|" if !negated => tokens.push(Token::Or),
            "AND" if !negated => {}
            _ => {
                part.terms.push(Term::Word(raw.to_lowercase()));
                tokens.push(Token::Part { negated, part: Box::new(part) });
            }
        }
    }
    // Parts joined by OR form one group; an OR with nothing on one side is the word "or".
    let mut groups: Vec<Vec<Alt>> = Vec::new();
    let mut joined = false;
    for (n, t) in tokens.iter().enumerate() {
        match t {
            Token::Or => {
                let before = groups.last().is_some();
                let after = matches!(tokens.get(n + 1), Some(Token::Part { .. }));
                if before && after && !joined {
                    joined = true;
                } else if !joined {
                    groups.push(vec![Alt { negated: false, part: SearchQuery { terms: vec![Term::Word("or".into())], ..Default::default() } }]);
                }
            }
            Token::Part { negated, part } => {
                let alt = Alt { negated: *negated, part: (**part).clone() };
                match groups.last_mut() {
                    Some(g) if joined => g.push(alt),
                    _ => groups.push(vec![alt]),
                }
                joined = false;
            }
        }
    }
    let mut q = SearchQuery::default();
    for mut g in groups {
        g.retain(|a| a.part != SearchQuery::default());
        match g.len() {
            0 => {}
            1 => {
                let a = g.remove(0);
                if a.negated {
                    q.none_of.push(a.part);
                } else {
                    q.merge(a.part);
                }
            }
            _ => q.any_of.push(g),
        }
    }
    q
}

/// `36h`, `3d`, `2w`, `6m` (30 days), `1y` (365 days) in seconds.
fn relative(s: &str) -> Option<i64> {
    let unit = s.chars().last()?;
    let n: i64 = s[..s.len() - unit.len_utf8()].parse().ok().filter(|n| *n >= 0 && *n <= 100_000)?;
    let secs = match unit {
        'h' => 3600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        'm' => 30 * 86_400,
        'y' => 365 * 86_400,
        _ => return None,
    };
    Some(n * secs)
}

fn operator(q: &mut SearchQuery, key: &str, value: String, now: i64) {
    match key {
        "from" => q.from.push(value),
        "to" | "cc" | "bcc" => q.to.push(value),
        "subject" => q.subject.push(value),
        "filename" => q.filename.push(value),
        "in" | "label" => q.in_mailboxes.push(value),
        "category" => q.categories.push(value),
        "has" => match value.as_str() {
            "attachment" | "attachments" | "attach" | "file" => q.has_attachment = true,
            _ => q.terms.push(Term::Word(format!("has:{value}"))),
        },
        "is" => match value.as_str() {
            "unread" => q.unread = Some(true),
            "read" => q.unread = Some(false),
            "starred" | "flagged" => q.starred = Some(true),
            "unstarred" | "unflagged" => q.starred = Some(false),
            "answered" | "replied" => q.answered = Some(true),
            _ => q.terms.push(Term::Word(format!("is:{value}"))),
        },
        "before" | "after" => match parse_date(&value) {
            Some(t) if key == "before" => q.before = Some(q.before.map_or(t, |b| b.min(t))),
            Some(t) => q.after = Some(q.after.map_or(t, |a| a.max(t))),
            None => q.terms.push(Term::Word(format!("{key}:{value}"))),
        },
        "newer_than" | "older_than" => match relative(&value) {
            Some(d) if key == "newer_than" => q.after = Some(q.after.map_or(now - d, |a| a.max(now - d))),
            Some(d) => q.before = Some(q.before.map_or(now - d, |b| b.min(now - d))),
            None => q.terms.push(Term::Word(format!("{key}:{value}"))),
        },
        _ => q.terms.push(Term::Word(format!("{key}:{value}"))),
    }
}

/// `YYYY-MM-DD` or `YYYY/MM/DD` to the Unix time of that day's start, UTC.
pub(crate) fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split(['-', '/']).collect();
    let [y, m, d] = parts.as_slice() else { return None };
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn has_token(s: &str) -> bool {
    s.chars().any(char::is_alphanumeric)
}

/// Largest edit distance allowed for a word of `n` characters: none under 4, 1 up to 7, then 2.
fn max_typos(n: usize) -> usize {
    match n {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// Optimal string alignment distance (Levenshtein plus adjacent transpositions), or None if over `max`.
pub(crate) fn edit_distance(a: &[char], b: &[char], max: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let w = b.len() + 1;
    let mut prev2 = vec![0usize; w];
    let mut prev: Vec<usize> = (0..w).collect();
    let mut cur = vec![0usize; w];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    (d <= max).then_some(d)
}

fn upper_bound(prefix: &str) -> String {
    let mut s = prefix.to_owned();
    s.push(char::MAX);
    s
}

/// How many fuzzy candidates one misspelt word may expand to.
const MAX_CANDIDATES: usize = 8;

/// One free word as an FTS5 expression: a prefix match if any indexed word starts with it, else the
/// indexed words within the typo budget that share its first letter, else the word itself.
fn word_expression(conn: &Connection, word: &str) -> Result<Option<String>> {
    if !has_token(word) {
        return Ok(None);
    }
    let simple = word.chars().all(char::is_alphanumeric);
    let n = word.chars().count();
    if !simple || n < 2 {
        return Ok(Some(quote(word)));
    }
    let mut st = conn.prepare_cached("SELECT 1 FROM terms WHERE term >= ?1 AND term < ?2 LIMIT 1").db()?;
    if st.exists([word, &upper_bound(word)]).db()? {
        return Ok(Some(format!("{}*", quote(word))));
    }
    let max = max_typos(n);
    if max == 0 {
        return Ok(Some(quote(word)));
    }
    let first: String = word.chars().take(1).collect();
    let chars: Vec<char> = word.chars().collect();
    let mut st = conn.prepare_cached("SELECT term FROM terms WHERE term >= ?1 AND term < ?2").db()?;
    let mut found: Vec<(usize, String)> = Vec::new();
    let mut t: Vec<char> = Vec::new();
    for row in st.query_map([&first, &upper_bound(&first)], |r| r.get::<_, String>(0)).db()? {
        let term = row.db()?;
        if term.len().abs_diff(word.len()) > max * 4 {
            continue;
        }
        t.clear();
        t.extend(term.chars());
        if let Some(d) = edit_distance(&chars, &t, max) {
            found.push((d, term));
        }
    }
    if found.is_empty() {
        return Ok(Some(quote(word)));
    }
    found.sort();
    found.truncate(MAX_CANDIDATES);
    let alts: Vec<String> = found.iter().map(|(_, t)| quote(t)).collect();
    Ok(Some(format!("({})", alts.join(" OR "))))
}

fn column_expression(column: &str, value: &str) -> Option<String> {
    has_token(value).then(|| format!("{column} : {}*", quote(value)))
}

/// The FTS5 MATCH expression for the text parts of `q`, or None if it has none that can match.
pub(crate) fn fts_expression(conn: &Connection, q: &SearchQuery) -> Result<Option<String>> {
    let mut parts = Vec::new();
    for t in &q.terms {
        match t {
            Term::Word(w) => parts.extend(word_expression(conn, w)?),
            Term::Phrase(p) if has_token(p) => parts.push(quote(p)),
            Term::Phrase(_) => {}
        }
    }
    let columns = [("sender", &q.from), ("recipients", &q.to), ("subject", &q.subject), ("files", &q.filename)];
    for (column, values) in columns {
        parts.extend(values.iter().filter_map(|v| column_expression(column, v)));
    }
    Ok((!parts.is_empty()).then(|| parts.join(" AND ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operators_and_words_are_split() {
        let q = parse_search(r#"From:Ada@Example.com subject:"weekly report" has:attachment is:unread lunch "big plan" in:inbox"#);
        assert_eq!(q.from, vec!["ada@example.com"]);
        assert_eq!(q.subject, vec!["weekly report"]);
        assert!(q.has_attachment);
        assert_eq!(q.unread, Some(true));
        assert_eq!(q.terms, vec![Term::Word("lunch".into()), Term::Phrase("big plan".into())]);
        assert_eq!(q.in_mailboxes, vec!["inbox"]);
    }

    #[test]
    fn unknown_operators_and_bad_dates_are_words() {
        let q = parse_search("foo:bar before:yesterday is:weird has:pdf");
        assert_eq!(
            q.terms,
            vec![
                Term::Word("foo:bar".into()),
                Term::Word("before:yesterday".into()),
                Term::Word("is:weird".into()),
                Term::Word("has:pdf".into())
            ]
        );
        assert_eq!(q.before, None);
    }

    #[test]
    fn dates_are_utc_day_starts() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2024/02/29"), Some(1_709_164_800));
        assert_eq!(parse_date("2024-13-01"), None);
        let q = parse_search("after:2024-01-01 before:2024-02-01 before:2024-01-15");
        assert_eq!(q.after, parse_date("2024-01-01"));
        assert_eq!(q.before, parse_date("2024-01-15"));
        assert!(!q.has_text());
    }

    #[test]
    fn empty_values_and_stray_quotes_do_not_panic() {
        let q = parse_search(r#"from: "  "unterminated subject:"#);
        assert!(q.from.is_empty());
        assert_eq!(q.terms, vec![Term::Word("unterminated".into())]);
        let _ = parse_search("\"");
        let _ = parse_search("::: \"\" a:\"");
    }

    #[test]
    fn or_groups_negations_and_relative_dates() {
        let now = 1_800_000_000;
        let q = parse_search_at("-from:ci@example.com is:unread (from:ada OR -subject:report) AND newer_than:36h older_than:1w", now);
        assert_eq!(q.unread, Some(true));
        assert_eq!(q.none_of, vec![SearchQuery { from: vec!["ci@example.com".into()], ..Default::default() }]);
        assert_eq!(
            q.any_of,
            vec![vec![
                Alt { negated: false, part: SearchQuery { from: vec!["ada".into()], ..Default::default() } },
                Alt { negated: true, part: SearchQuery { subject: vec!["report".into()], ..Default::default() } },
            ]]
        );
        assert_eq!(q.after, Some(now - 36 * 3600));
        assert_eq!(q.before, Some(now - 7 * 86_400));
        assert!(q.terms.is_empty(), "AND and parentheses are not words");
        // OR with nothing on one side, lower-case or, and bad durations are words.
        let q = parse_search_at("OR lunch or newer_than:soon -", now);
        assert_eq!(q.terms, vec![Term::Word("or".into()), Term::Word("lunch".into()), Term::Word("or".into()), Term::Word("newer_than:soon".into()), Term::Word("-".into())]);
        let q = parse_search_at("a OR b OR c category:Promotions", now);
        assert_eq!(q.any_of[0].len(), 3);
        assert_eq!(q.categories, vec!["promotions"]);
        assert_eq!(parse_search_at("-\"weekly report\"", now).none_of[0].terms, vec![Term::Phrase("weekly report".into())]);
    }

    #[test]
    fn edit_distance_counts_typos_and_swaps() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(edit_distance(&c("invoice"), &c("invoice"), 1), Some(0));
        assert_eq!(edit_distance(&c("invocie"), &c("invoice"), 1), Some(1));
        assert_eq!(edit_distance(&c("invoce"), &c("invoice"), 1), Some(1));
        assert_eq!(edit_distance(&c("inxoxce"), &c("invoice"), 1), None);
        assert_eq!(edit_distance(&c("restaurnat"), &c("restaurant"), 2), Some(1));
    }

    #[test]
    fn quotes_inside_values_are_escaped() {
        assert_eq!(quote(r#"a"b"#), r#""a""b""#);
        assert_eq!(column_expression("sender", "ada"), Some(r#"sender : "ada"*"#.into()));
        assert_eq!(column_expression("sender", "--"), None);
    }
}
