//! The sync cursor: `rs1:` + JSON. Opaque to everyone else.
//!
//! Per source (sent, received): the newest creation time reported (`t`, ms) and the ids at exactly that
//! time (`edge`), so "new" is "created after `t`, or at `t` and not in `edge`". A run is a paging pass
//! through one list, newest first with `after=<last id>`, that stops at the floor (what an earlier pass
//! already reported), at the end of the list or after a page limit. Plus the delivery state of the newest
//! sent emails (to see them change) and the scheduled emails still waiting (to see them go out).

use lumbridge_mail_core::{Error, Result, SyncCursor};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const PREFIX: &str = "rs1:";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Run {
    /// None: the first page is next.
    #[serde(default)]
    pub after: Option<String>,
    /// Stop at items at or below this point (None: a full sync, everything is new).
    #[serde(default)]
    pub floor_t: Option<i64>,
    #[serde(default)]
    pub floor_edge: Vec<String>,
    #[serde(default)]
    pub pages: u32,
}

impl Run {
    pub fn is_new(&self, t: i64, id: &str) -> bool {
        match self.floor_t {
            None => true,
            Some(f) => t > f || (t == f && !self.floor_edge.iter().any(|e| e == id)),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Source {
    #[serde(default)]
    pub t: Option<i64>,
    #[serde(default)]
    pub edge: Vec<String>,
    #[serde(default)]
    pub run: Option<Run>,
}

impl Source {
    /// A new pass from the top, stopping at what is already known.
    pub fn start(&mut self) {
        self.run = Some(Run { after: None, floor_t: self.t, floor_edge: self.edge.clone(), pages: 0 });
    }

    /// Moves the resume point up to `(t, ids)` if that is newer (or adds ids at the same time).
    pub fn advance(&mut self, t: i64, ids: &[String]) {
        match self.t {
            Some(cur) if cur > t => {}
            Some(cur) if cur == t => {
                for id in ids {
                    if !self.edge.contains(id) {
                        self.edge.push(id.clone());
                    }
                }
            }
            _ => {
                self.t = Some(t);
                self.edge = ids.to_vec();
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    /// The pages of a full sync (`Delta::reset`) are still coming.
    #[serde(default)]
    pub full: bool,
    #[serde(default)]
    pub sent: Source,
    #[serde(default)]
    pub recv: Source,
    /// Resend id -> last_event, for the newest page of sent mail.
    #[serde(default)]
    pub states: BTreeMap<String, String>,
    /// Resend id -> scheduled_at (ms) of scheduled emails not yet seen to go out or be canceled.
    #[serde(default)]
    pub scheduled: BTreeMap<String, i64>,
}

impl Cursor {
    pub fn full_sync(receiving: bool) -> Cursor {
        let mut c = Cursor { full: true, ..Cursor::default() };
        c.sent.start();
        if receiving {
            c.recv.start();
        }
        c
    }

    /// None for no cursor; `ResyncNeeded` for one we cannot read.
    pub fn read(c: Option<&SyncCursor>) -> Result<Option<Cursor>> {
        let Some(c) = c else { return Ok(None) };
        let json = c.0.strip_prefix(PREFIX).ok_or_else(|| Error::ResyncNeeded("not a Resend sync cursor".into()))?;
        serde_json::from_str(json).map(Some).map_err(|_| Error::ResyncNeeded("unreadable Resend sync cursor".into()))
    }

    pub fn encode(&self) -> SyncCursor {
        SyncCursor(format!("{PREFIX}{}", serde_json::to_string(self).unwrap_or_default()))
    }

    pub fn running(&self) -> bool {
        self.sent.run.is_some() || self.recv.run.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_foreign_cursors() {
        let mut c = Cursor::full_sync(true);
        c.sent.advance(10, &["a".into()]);
        c.sent.advance(10, &["b".into()]);
        c.sent.advance(5, &["z".into()]);
        assert_eq!((c.sent.t, c.sent.edge.clone()), (Some(10), vec!["a".to_string(), "b".to_string()]));
        let back = Cursor::read(Some(&c.encode())).unwrap().unwrap();
        assert_eq!(back, c);
        assert!(matches!(Cursor::read(Some(&SyncCursor("imap1:{}".into()))), Err(Error::ResyncNeeded(_))));
        assert!(matches!(Cursor::read(Some(&SyncCursor("rs1:{nope".into()))), Err(Error::ResyncNeeded(_))));
        let mut s = Source::default();
        s.advance(7, &["x".into()]);
        s.start();
        let r = s.run.unwrap();
        assert!(!r.is_new(7, "x") && r.is_new(7, "y") && r.is_new(8, "x") && !r.is_new(6, "q"));
    }
}
