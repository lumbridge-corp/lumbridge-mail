//! The sync cursor: `zo1:` + JSON. Opaque to everyone else. See the crate README, "Cursor".
//!
//! Zoho has no history or change feed, so the cursor remembers, per folder, the newest page it last
//! saw: message id -> (read/flag bits, date). An incremental round lists that page again and reports
//! only what is new or changed; a message that left the page while newer than the page's oldest row
//! moved or was deleted (looked up once to tell which). The rest is runs that span several `sync` calls,
//! one `list_mails` page per call: the full sync, extra pages of a burst of new mail, a deep pass over
//! older pages, and backfill.

use lumbridge_mail_core::{Error, MailboxRole, Result, SyncCursor};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const PREFIX: &str = "zo1:";

/// The id of the Archive view (archived mail is out of its folder's listing in Zoho). Not a Zoho id:
/// those are all digits.
pub const ARCHIVE: &str = "archive";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Seen {
    /// Read = 1, flagged = 2.
    pub b: u8,
    /// Unix seconds.
    pub t: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FolderState {
    /// The folder's newest page as last listed.
    #[serde(default)]
    pub seen: BTreeMap<String, Seen>,
    /// How many messages the full sync listed here (where backfill starts).
    #[serde(default)]
    pub listed: u32,
}

/// One synced folder: id and role (`inbox`, `sent`, ...), so rounds know which are hot without asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Box_ {
    pub id: String,
    pub role: String,
}

impl Box_ {
    pub fn role(&self) -> MailboxRole {
        MailboxRole::parse(&self.role).unwrap_or(MailboxRole::Custom)
    }
    /// Polled every round.
    pub fn hot(&self) -> bool {
        matches!(
            self.role(),
            MailboxRole::Inbox | MailboxRole::Sent | MailboxRole::Drafts
        )
    }
}

/// A paging pass over several folders, one page per `sync` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Run {
    /// Folder ids in order.
    pub order: Vec<String>,
    pub idx: usize,
    /// 1-based `start` of the next page in the current folder.
    pub start: u32,
    /// Pages read in the current folder.
    pub pages: u32,
    /// Messages listed in the current folder.
    pub count: u32,
    /// The first id of the last page (a server that ignores `start` would hand back the same page).
    #[serde(default)]
    pub last: Option<String>,
    /// A burst run stops at mail at or before this date (what the last round already knew).
    #[serde(default)]
    pub stop_t: Option<i64>,
    /// Zoho's `nextPageToken` from the last page (used instead of `start` when present).
    #[serde(default)]
    pub token: Option<String>,
}

impl Run {
    pub fn new(order: Vec<String>, start: u32) -> Self {
        Self {
            order,
            idx: 0,
            start: start.max(1),
            pages: 0,
            count: 0,
            last: None,
            token: None,
            stop_t: None,
        }
    }
    pub fn folder(&self) -> Option<&str> {
        self.order.get(self.idx).map(String::as_str)
    }
    pub fn next_folder(&mut self, start: u32) {
        self.idx += 1;
        self.start = start.max(1);
        self.pages = 0;
        self.count = 0;
        self.last = None;
        self.token = None;
    }
    pub fn done(&self) -> bool {
        self.idx >= self.order.len()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Full {
    pub run: Run,
    /// The day limit (Unix seconds), if any.
    pub day_floor: Option<i64>,
    /// What the run has really covered so far: the day limit, raised to just above the oldest message
    /// listed in any folder cut short by the count limit.
    pub floor: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Backfill {
    pub run: Run,
    /// Fill in back to here (None = everything).
    pub since: Option<i64>,
    /// Only mail older than this is new to the store.
    pub before: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    /// A full sync in progress (`Delta::reset` on each of its pages).
    #[serde(default)]
    pub full: Option<Full>,
    #[serde(default)]
    pub boxes: Vec<Box_>,
    #[serde(default)]
    pub folders: BTreeMap<String, FolderState>,
    /// Incremental rounds so far (cold folders and deep passes go by it).
    #[serde(default)]
    pub round: u64,
    /// Extra pages of a burst of new mail: (folder, next start, pages so far).
    #[serde(default)]
    pub burst: Option<Run>,
    /// A deep pass in progress.
    #[serde(default)]
    pub deep: Option<Run>,
    /// The oldest date the store has from us (the window floor, lowered by backfill). None = all mail.
    #[serde(default)]
    pub floor: Option<i64>,
    #[serde(default)]
    pub backfill: Option<Backfill>,
    /// The next call is a backfill page.
    #[serde(default)]
    pub backfill_next: bool,
}

impl Cursor {
    /// None for no cursor; `ResyncNeeded` for one we cannot read.
    pub fn read(c: Option<&SyncCursor>) -> Result<Option<Cursor>> {
        let Some(c) = c else { return Ok(None) };
        let json =
            c.0.strip_prefix(PREFIX)
                .ok_or_else(|| Error::ResyncNeeded("not a Zoho sync cursor".into()))?;
        serde_json::from_str(json)
            .map(Some)
            .map_err(|_| Error::ResyncNeeded("unreadable Zoho sync cursor".into()))
    }

    pub fn encode(&self) -> SyncCursor {
        SyncCursor(format!(
            "{PREFIX}{}",
            serde_json::to_string(self).unwrap_or_default()
        ))
    }

    pub fn role_of(&self, id: &str) -> MailboxRole {
        if id == ARCHIVE {
            return MailboxRole::Archive;
        }
        self.boxes
            .iter()
            .find(|b| b.id == id)
            .map(Box_::role)
            .unwrap_or(MailboxRole::Custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_foreign_cursors() {
        let mut c = Cursor {
            round: 3,
            floor: Some(100),
            ..Cursor::default()
        };
        c.boxes.push(Box_ {
            id: "1".into(),
            role: "inbox".into(),
        });
        c.folders
            .entry("1".into())
            .or_default()
            .seen
            .insert("5".into(), Seen { b: 1, t: 9 });
        c.full = Some(Full {
            run: Run::new(vec!["1".into()], 1),
            day_floor: Some(50),
            floor: Some(60),
        });
        let back = Cursor::read(Some(&c.encode())).unwrap().unwrap();
        assert_eq!(back, c);
        assert!(back.boxes[0].hot());
        assert_eq!(back.role_of(ARCHIVE), MailboxRole::Archive);
        assert!(matches!(
            Cursor::read(Some(&SyncCursor("rs1:{}".into()))),
            Err(Error::ResyncNeeded(_))
        ));
        assert!(matches!(
            Cursor::read(Some(&SyncCursor("zo1:{nope".into()))),
            Err(Error::ResyncNeeded(_))
        ));
        assert_eq!(
            Cursor::read(Some(&SyncCursor("zo1:{}".into())))
                .unwrap()
                .unwrap(),
            Cursor::default()
        );
    }
}
