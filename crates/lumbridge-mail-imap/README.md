# lumbridge-mail-imap

The IMAP (read) + SMTP (send) backend of Lumbridge Mail: `ImapProvider` implements
`lumbridge_mail_core::Provider` for one account. It holds connections, not mail: the daemon's sync engine
owns the store and the cursor. Credentials are handed in by the caller and are never stored, printed or
logged (`Credentials`' `Debug` redacts them).

## Use

```rust
use lumbridge_mail_core::{AccountId, Provider};
use lumbridge_mail_imap::{discover, Credentials, Identity, ImapAccount, ImapProvider};

let found = discover::builtin("ada@fastmail.com").expect("known provider"); // or discover::lookup(..)
let mut account = ImapAccount::new(
    AccountId::new("personal-fastmail"),
    Identity { email: "ada@fastmail.com".into(), name: "Ada".into() },
    found.imap,
    Some(found.smtp),                     // None = read-only account
    Credentials::Password { username: found.username, password: keyring_secret },
);
account.options.server_saves_sent = Some(found.server_saves_sent);
let provider = ImapProvider::new(account);
provider.check().await?;                  // sign in + list folders now, to validate settings
let delta = provider.sync(None).await?;   // then sync(Some(&cursor)) while delta.more
```

OAuth: `Credentials::OAuth2 { username, token }` (IMAP `AUTHENTICATE XOAUTH2`, SMTP `AUTH XOAUTH2`).
Refreshing the token is the caller's job: `provider.set_credentials(new, None)`; the next connection uses it.

## Connections and TLS

- `Security::Tls` (implicit, 993/465, preferred), `Security::StartTls` (143/587; fails if the server does
  not offer STARTTLS, never falls back), `Security::Plain` only to a loopback address (tests, a local
  bridge). Plaintext to anything else is `Error::Refused` before a connection is made.
- rustls with the ring provider and webpki roots. `Server::accept_invalid_certs` is honoured only for
  loopback hosts (Proton Mail Bridge uses a self-signed certificate) and ignored everywhere else.
- SMTP `EHLO` says `[127.0.0.1]`, never the machine's host name.
- One connection for sync/reading/actions, one more for IDLE. Any `Error::Offline` drops the connection;
  the next call reconnects. Every command has a timeout (`Options::timeout`, default 60 s).

## Why async-imap

Checked on crates.io on 2026-11-10:

| Crate | Version | Notes |
|---|---|---|
| `async-imap` | 0.12.0 (2026-09) | ~1M recent downloads, maintained (Delta Chat). tokio runtime, any `AsyncRead + AsyncWrite` stream (so rustls or plain TCP for tests), IDLE with timeout, AUTHENTICATE (XOAUTH2), `run_command` + `read_response` for anything else. Parses CONDSTORE (`MODSEQ`, `HIGHESTMODSEQ`) and QRESYNC (`VANISHED`) through imap-proto 0.17. |
| `imap-next` / `imap-codec` | 0.3.4 / 1.0 (2.0 alpha) | Excellent, strict codec; but a lower-level state machine we would have to drive ourselves, and the 2.0 line is still alpha. |
| `io-imap` (Pimalaya) | 0.8.1 (2026-11-03) | Sans-I/O, young (weeks old at this version) and moving fast. |
| `imap` | 3.0.0-alpha | Blocking; what Overlook uses. Not for a tokio daemon. |

So: `async-imap` for the session, login, IDLE and the plain commands. One caveat found while testing:
async-imap's streaming parsers (`uid_fetch`, `uid_search`, `list`, `status`, `select`) end quietly when
the connection drops or the server answers NO, which would look like an empty folder. A sync that thinks
a folder is empty reports every message in it as removed, so this crate runs those commands through its
own loop (`Conn::run`): it reads to the tagged answer and turns EOF into `Offline` and NO/BAD into an
error. Tested with dropped connections and scripted NO answers.

Other libraries: `lettre` 0.11 for SMTP (rustls, PLAIN/LOGIN/XOAUTH2), `mail-parser` 0.11 for reading
(1.0 needs rustc 1.98; the workspace is on 1.97), `mail-builder` 2.0 for writing (drafts may have no
recipients, which lettre's builder refuses), `hickory-resolver` for SRV lookups.

## Ids

- `MessageId` = `<folder>:<UIDVALIDITY>:<UID>`, the folder as the server names it (modified UTF-7).
  Unique per account by IMAP's rules; parsed from the right, so folder names may contain `:`. A message
  moved to another folder gets a new id (the old one is reported removed, the new one upserted).
- `MailboxId` = the server's folder name. `Mailbox.name` is the display form: decoded, the delimiter shown
  as `/`, `INBOX` as `Inbox`, `[Gmail]/` and Proton's `Folders/` dropped.
- `ThreadId` = `t:` + FNV-1a of the root Message-ID: the first `References` entry, else `In-Reply-To`,
  else the message's own Message-ID. A reply (subject starts `Re:`, `Fwd:`, `AW:`, ...) with neither
  header falls back to the normalised subject. The store may regroup further; these ids only need to agree
  across messages of one account.

## Sync and the cursor

`SyncCursor` is `imap1:` + JSON: per folder `{validity, uidnext, modseq, known, floor, below, flags}`.
`known` is the set of UIDs already reported, written as an IMAP sequence set (`1:500,502:900`), so it stays
small. Each `sync` call:

1. `LIST`; folders that disappeared (or are now skipped) have all their known ids reported removed.
2. Per folder (Inbox, Sent, Drafts, Archive, custom, Spam, Trash) `STATUS`. A changed UIDVALIDITY removes
   every known id of the folder and starts it over. With CONDSTORE, a folder whose UIDNEXT, HIGHESTMODSEQ
   and message count are unchanged costs that one STATUS.
3. Otherwise `EXAMINE` (read-only, nothing marked seen) and:
   - new mail: `UID SEARCH UID <uidnext>:*`, then headers + preview of those (bursts are paged too);
   - flag changes: with CONDSTORE `UID FETCH 1:<n> (UID FLAGS) (CHANGEDSINCE <modseq>)`; without it, the
     flags of the newest `Options::flag_window` (1000) known messages are compared with a snapshot kept in
     the cursor. Older messages' flag changes are missed until the folder is resynced (documented limit);
   - expunges: with QRESYNC from `VANISHED (EARLIER)` on that same FETCH; otherwise, when the message count
     does not add up, `UID SEARCH UID <known>` and the difference;
   - changed messages are re-sent whole (`Delta.upserted`), because the engine stores `MessageMeta`.
4. First sync of a folder is newest first: `UID SEARCH UID 1:<floor-1>`, the newest `page_size` (250) of
   those, `floor` moves down. `Delta.more` is true until every folder is complete.

Headers, snippet and the attachment hint come from the first `preview_bytes` (16 KiB) of each message
(`BODY.PEEK[]<0.16384>`), parsed with mail-parser. Dates are INTERNALDATE (arrival), else `Date:`.
Folders with roles in `Options::skip_roles` (default All Mail, Starred, Important) are neither listed nor
synced, so Gmail's All Mail does not double every message.

An unreadable cursor is `Error::ResyncNeeded`; the engine drops it and syncs from `None`. Every page of a
first sync (from `None`) has `Delta::reset`; `Delta::mailboxes` is always `Some` (the folder list).

## Roles

SPECIAL-USE attributes first (`\Sent \Drafts \Trash \Junk \Archive \All \Flagged \Important`), then names:
English and common German/French/Spanish/Italian/Dutch/Swedish ones, `INBOX.Sent`-style children of
INBOX, Gmail's `[Gmail]/...` and `[Google Mail]/...`, Outlook's `Sent Items` / `Deleted Items` /
`Junk E-mail`, Proton Bridge's `All Mail` / `Starred` / `Archive` (its `Folders/x` and `Labels/x` are
custom). Nested user folders never get a role by name.

## Actions (all safe to apply twice)

| Action | How |
|---|---|
| `SetSeen`, `SetFlagged` | `UID STORE +/-FLAGS.SILENT`. Ids from an old UIDVALIDITY or a deleted folder are skipped. |
| `Move { from: None }` | Add to `to`, remove nothing: `UID COPY`, skipping messages whose Message-ID is already there. |
| `Move` | `UID MOVE`; without MOVE: `COPY`, `\Deleted`, `UID EXPUNGE` (or `EXPUNGE` without UIDPLUS). Before copying, the destination is searched for each Message-ID, so a retry after a COPY that worked does not copy twice. A message no longer in the source is already moved. |
| `Archive` | Move to the `\Archive` folder; else to All Mail (Gmail: removes the Inbox label); else creates `Archive`. |
| `Trash` | Move to Trash (created if missing). Trashing something already in Trash deletes it for good. |
| `SaveDraft` | `APPEND` to Drafts with `\Draft \Seen`, then delete older copies with the same Message-ID (the Message-ID is derived from the account and `local_id`). Keeps Bcc. The outcome's `server_id` is the new copy's `Drafts:<validity>:<uid>`. |
| `DeleteDraft` | Delete by that Message-ID. |
| `Send` | See below. |

## Sending

1. The Message-ID is derived from the account and the draft's `local_id`, the same one the draft had.
2. Before anything goes out, Sent is searched for that Message-ID. Found means an earlier attempt worked:
   only the tidy-up runs. This is what makes a retry after "sent, but the reply was lost" safe on servers
   that file sent mail themselves (Gmail, Microsoft 365, Proton Bridge).
3. SMTP via lettre, Bcc only in the envelope. 4xx and dropped connections are `Offline` (retried), 5xx
   is `Invalid`, 530/534/535 is `Auth`.
4. If the server does not file sent mail (`Options::server_saves_sent`, default: Gmail or Office 365
   hosts, or Gmail's `X-GM-EXT-1` capability), a copy with Bcc is appended to Sent (`\Seen`), creating
   Sent if needed.
5. The draft copy is deleted and the message replied to gets `\Answered` (best effort).

Known gap: on a server that does not file sent mail, if the connection drops after SMTP accepted the
message but before the Sent copy is appended, a retry sends again. Closing it needs a marker copy in Sent
before SMTP; not done yet.

## Waiting for changes

`wait_for_change(t)` runs IDLE on INBOX over its own connection (at most 25 minutes per call) and returns
on any server notice or on the timeout. Without IDLE it sleeps `t`. Other folders' changes are found by
the next `sync`.

## Autodiscovery

`discover::builtin(email)`: a table of common providers (Gmail, Outlook.com, iCloud, Fastmail, Yahoo,
AOL, Proton Bridge, Zoho, Posteo, mailbox.org, GMX, WEB.DE, Yandex, HEY), no network.
`discover::lookup(email, timeout)`: SRV (`_imaps`, `_imap`, `_submissions`, `_submission`, RFC 6186/8314),
then `https://autoconfig.<domain>/mail/config-v1.1.xml` and the `.well-known` path on the domain. Only
when the caller asks; it talks to DNS and the mail domain, never a third party (no ISPDB). Plaintext
servers are never suggested. `discover::discover` = builtin, else lookup.

## Tests

`cargo test -p lumbridge-mail-imap` and
`cargo clippy -p lumbridge-mail-imap --all-targets --no-deps -- -D warnings`.

`tests/common/fake_imap.rs` is a scripted IMAP server (adapted from Overlook, MIT) and
`tests/common/fake_smtp.rs` a scripted SMTP server, both plain TCP on 127.0.0.1 in the test process,
with made-up mail. They can drop the connection before or after a command, answer NO, refuse logins, run
with or without MOVE / UIDPLUS / CONDSTORE / QRESYNC / IDLE / SPECIAL-USE, reset UIDVALIDITY, and (SMTP)
fail RCPT, drop after DATA, or file delivered mail into the fake IMAP Sent folder like Gmail.

## Credits

See the top-level `THIRD-PARTY-NOTICES`.
