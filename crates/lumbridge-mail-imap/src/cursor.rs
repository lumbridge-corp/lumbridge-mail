//! The sync cursor: per folder, what we have told the engine about. Serialised as `imap1:` + JSON into
//! `SyncCursor`. The engine stores it and hands it back; nothing else reads it.

use crate::uidset::UidSet;
use lumbridge_mail_core::{Error, Result, SyncCursor};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const PREFIX: &str = "imap1:";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    #[serde(default)]
    pub folders: BTreeMap<String, FolderCursor>,
    /// This cursor belongs to a first full sync that has more pages to go (`Delta::reset` on each).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub initial: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderCursor {
    /// UIDVALIDITY the UIDs below belong to. A different value on the server means start the folder over.
    pub validity: u32,
    /// Every UID at or above this is new to us.
    pub uidnext: u32,
    /// HIGHESTMODSEQ at the last sync (CONDSTORE servers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modseq: Option<u64>,
    /// UIDs we have reported (IMAP sequence-set form).
    #[serde(default)]
    pub known: String,
    /// First sync, newest first: UIDs below this have not been fetched yet. 0 or 1 = the folder is complete.
    #[serde(default)]
    pub floor: u32,
    /// How many messages the server had below `floor` when we last looked (for the expunge check).
    #[serde(default)]
    pub below: u32,
    /// Without CONDSTORE: flags of the newest `flag_window` known messages, to find flag changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<FlagSnapshot>,
}

impl FolderCursor {
    pub fn known_set(&self) -> UidSet {
        UidSet::parse(&self.known)
    }
    pub fn complete(&self) -> bool {
        self.floor <= 1
    }
}

/// Flags of a window of UIDs, each as a set of the UIDs that have that flag.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagSnapshot {
    /// The window: known UIDs at or above this.
    pub from: u32,
    #[serde(default)]
    pub seen: String,
    #[serde(default)]
    pub flagged: String,
    #[serde(default)]
    pub answered: String,
    #[serde(default)]
    pub draft: String,
}

impl CursorState {
    pub fn decode(cursor: Option<&SyncCursor>) -> Result<Self> {
        let Some(c) = cursor else { return Ok(Self { initial: true, ..Self::default() }) };
        let Some(json) = c.0.strip_prefix(PREFIX) else {
            return Err(Error::ResyncNeeded("not an IMAP sync cursor".into()));
        };
        serde_json::from_str(json).map_err(|e| Error::ResyncNeeded(format!("bad IMAP sync cursor: {e}")))
    }

    pub fn encode(&self) -> SyncCursor {
        // Serialising plain structs of strings and numbers cannot fail.
        SyncCursor(format!("{PREFIX}{}", serde_json::to_string(self).unwrap_or_else(|_| "{}".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut s = CursorState::default();
        s.folders.insert(
            "INBOX".into(),
            FolderCursor { validity: 7, uidnext: 50, modseq: Some(900), known: "1:49".into(), ..Default::default() },
        );
        let c = s.encode();
        assert!(c.0.starts_with("imap1:"));
        assert_eq!(CursorState::decode(Some(&c)).unwrap(), s);
        assert_eq!(CursorState::decode(None).unwrap(), CursorState { initial: true, ..CursorState::default() });
        assert!(matches!(CursorState::decode(Some(&SyncCursor("12345".into()))), Err(Error::ResyncNeeded(_))));
        assert!(matches!(CursorState::decode(Some(&SyncCursor("imap1:{oops".into()))), Err(Error::ResyncNeeded(_))));
    }
}
