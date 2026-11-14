# lumbridge-mail-gog

A `lumbridge_mail_core::Provider` for Gmail that drives the [`gog`](https://github.com/steipete/gogcli)
CLI and reads its JSON, instead of talking to Google itself. gog already holds the OAuth tokens and has the
guards we want (`--gmail-no-send`, `--enable-commands-exact`), so we never register a Google OAuth client or
touch a token.

One `GogProvider` serves one Gmail account. It holds no state the engine needs: the sync cursor is opaque
and handed back by the engine; the only memory is a cache (draft ids saved by this process, the labels it
last reported per message) that makes things cheaper, never more correct.

## Use

```rust
use lumbridge_mail_core::AccountId;
use lumbridge_mail_gog::{AccountLimits, GatedRunner, Gates, GogConfig, GogProvider, HostLimits, Mux, SshRunner};
use std::sync::Arc;

// gog on another machine (the one holding the tokens), with its env file sourced first. `~/` is the
// ssh user's home there (expanded by the remote shell). One shared ssh connection per host.
let ssh = SshRunner::new("gog-host", "~/.local/bin/gog")
    .with_env_file("~/.config/gog.env")
    .with_mux(Arc::new(Mux::new("/run/user/1000/lumbridge-mail/ssh")));
// Every call goes through the budget: the host's gate is shared by all accounts on that host.
let gates = Gates::new();
let runner = GatedRunner::new(Arc::new(ssh), gates.gate("ssh:gog-host", HostLimits::ssh()), AccountLimits::default());

let mut cfg = GogConfig::new(AccountId::new("personal-gmail"), "someone@example.com");
cfg.name = Some("Someone".into());
let provider = GogProvider::new(cfg, Arc::new(runner));
```

The daemon builds exactly this from `config.toml` (`[gog]` and the account's provider keys, see the
daemon's README).

`GogConfig` defaults: listings of 100, history pages of 100, a window of the last 90 days or 2,000
messages (whichever is smaller) for the first sync, the label list re-read at most every 5 minutes,
snippets from `--include-body`, a reconcile listing of the newest 50 of `in:anywhere newer_than:7d`, no
backfill unless asked (`request_backfill`, or `backfill_auto`), one backfill page at most every 60 s, 60 s
timeout (180 s for a send), compose mode `Auto`.

## Why it is careful

gog is a fresh ~40 MB Go process per call that decrypts its file keyring on every start (about a
CPU-second), and over ssh every call was also a new connection. The first live run (two accounts, read-only)
listed 100 ids and then started one `gog gmail raw` per message, six at a time per account: the gog host
(4 cores, live services) went to load 19 and its sshd began resetting connections
(`kex_exchange_identification: Connection reset by peer`). The daemon's triage then fetched bodies the same
way. So:

1. **Few processes.** A page of 100 messages is one `gmail messages search` that carries everything a list
   row needs (see "Calls per page"). Label changes come from the same listings, never from refetches. A
   body fetch brings the whole thread.
2. **A hard budget** (`budget::GatedRunner`), in front of every call.
3. **A window** instead of the whole mailbox on the first sync; older mail only by slow backfill.

## The budget

`GatedRunner` wraps any `Runner`. Every call passes two gates:

| Gate | Default (ssh / local) | |
|---|---|---|
| per host (`HostGate`, shared by every account on that ssh host, or by all local accounts) | 2 / 3 processes at once | |
| start rate per host | 120 per minute (token bucket, bursts of 2): at most ~2 starts per second | |
| per account | 2 at once (inside the host's limit) | |
| transport failure (ssh exit 255, connection reset, timeout) | wait 2 s, doubling per failure in a row, with jitter (50 to 100 %), up to 120 s; then retry, up to 3 times | the wait holds every account on that host |
| circuit breaker | 5 transport failures in a row: every call for that host fails at once as `Offline` for 300 s; then one probe call at a time until one works | the daemon shows those accounts offline and retries later |

gog's own failures (its exit codes: rate limited, 5xx, 404) are answers, not transport failures; the
provider maps them (table below) and gog itself retries Google's 429s.

A call that started is never killed because its caller went away (a cancelled sync, an account restarting on
a config reload): it runs in its own task, holding its slot, until it ends or hits its own timeout. So the
slot count is the real number of gog processes, and a reload never leaves a half-done gog behind.

`HostGate::load()` / `Provider::load()` report starts in the last minute, starts in total, processes running,
and any backoff or open breaker; the daemon logs that once a minute while gog is busy and `lbmail status`
prints it.

## Runners

`Runner` is an async trait: one `GogCall` (arguments, optional stdin, optional staged input files) in, one
`GogOutput` (exit code, stdout, stderr, optional output file) out.

- `LocalRunner`: spawns the binary (via `/bin/sh -c 'set -a; . "$1"; set +a; shift; exec "$@"'` when an env
  file is set, so neither the file name nor any argument is ever parsed as shell text). With `Niceness`
  (the daemon sets `nice -n 15` and `ionice -c3` when the tools exist), they go in front of gog.
- `SshRunner`: `ssh -o BatchMode=yes -o ConnectTimeout=8 -o ServerAliveInterval=15 -- <host> '<script>'`.
  Every argument is single-quoted; `~/` in `binary` and `env_file` becomes `"$HOME"/...` so it is expanded
  on the remote host (`~user` is refused: `runner::check_remote_path`). The script renices gog
  (`exec $lbm_n $lbm_i gog ...`, where `lbm_n='nice -n 15'` and `lbm_i='ionice -c3'` only if the remote host
  has them). Calls that need files (an `--attach`, an `--body-html-file`, an attachment download's `--out`)
  run in a remote `mktemp -d` that is filled from a tar stream on stdin, send `stdout` and the output file
  back as a tar stream, and remove it on exit. Other calls are a plain `exec gog ...`.
  - **One connection per host** (`Mux`): the first call starts a master on its own
    (`ssh -f -N -o ControlMaster=yes -o ControlPersist=300 -o ControlPath=<dir>/cm-<hash>`, stdio on
    /dev/null so it can never hold our pipes) in a private 0700 directory (the daemon's runtime directory,
    `ssh/`); calls are clients of it (`ControlMaster=no`). If the master died, ssh connects directly by
    itself and we drop the stale socket so the next call starts a new master; if the master refuses a
    session (MaxSessions) the call is retried once directly. If no master can be started (no multiplexing,
    a control path too long for a socket) calls go direct for 10 minutes, then try again. Sessions on one
    master never exceed the host's budget (2 by default; the daemon caps it at `ssh_max_sessions`, 8,
    below OpenSSH's default MaxSessions of 10). `SshRunner::close` (at daemon shutdown) runs `ssh -O exit`.
- `fake::FakeRunner`: answers from recorded JSON and logs every call, for tests (ours and the daemon's).

Every call has a deadline (`kill_on_drop` once it is over). Failures map to core `Error` kinds:

| What happened | `Error` |
|---|---|
| gog exit 4 (auth required, missing scope), 10 (config / credentials missing), stderr `invalid_grant`, `token has been expired or revoked`; ssh `Permission denied` | `Auth` (not retried) |
| gog exit 5 (Google 404) | `NotFound` |
| gog exit 7 (rate limited), 8 (5xx, timeouts), stderr `dial tcp`, `no such host`, `connection refused`, `i/o timeout`...; ssh exit 255; our own timeout | `Offline` (retried) |
| gog exit 2 with `is not enabled` / `blocked by` (a guard said no) | `Refused` |
| gog exit 2 otherwise (usage) | `Invalid` |
| gog exit 6 (permission denied) and anything else | `Other` |

## Versions

`gog version` is asked once per provider (a failure to reach gog is not remembered; an unreadable answer
counts as the oldest supported, 0.28). What changes with the version (checked against gogcli's source at
every tag from v0.28.0 to v0.48.0 and main):

| gog | `gmail messages search` gives | so a listed record has |
|---|---|---|
| 0.28 to 0.34.1 | id, threadId, labels (names), From, Subject, Date (`2006-01-02 15:04`, in `--timezone UTC`); with `--include-body`: the best text body and attachments | from, subject, date to the minute, labels/flags, snippet (made from the body), `has_attachments`; **no To/Cc, Message-ID or References** until the message's thread is fetched |
| 0.34.2 | + `internalDateIso` | Gmail's own date, to the second |
| 0.44.0 | + `--include-recipients` (To, Cc, Bcc) | + recipients |
| 0.47.0 | + `--headers a,b` (we ask for Message-Id, In-Reply-To, References, Content-Type) | everything but the size |
| 0.41.0 | (drafts and send take `--raw-file`) | compose mode `Raw` |

Listed records are partial (`MessageMeta::partial`): the store keeps what it already has for every field a
listing lacks, so a label change seen in a listing never wipes the recipients a thread fetch stored. There is
no batch get in gog at any version (`gog api` and `gog mcp` exist from 0.40 but run one Google call / one
gog subprocess per call), and `gmail search` (threads) has no per-message data, so the listing above is the
cheapest way at every version.

## Calls per page

Gog processes for a page of 100 messages (window or backfill), and around it:

| | before (any gog) | now, gog 0.28 to 0.46 | now, gog 0.47+ |
|---|---|---|---|
| a page of 100 | 1 search + 100 `gmail raw` + `labels list` + `labels get INBOX` = **103** | **1** search | **1** search |
| first page of a full sync, extra | 0 | + labels list + `version` + 1 `gmail raw --format minimal` (resume point) | same |
| last page, extra | 1 history probe | 1 history probe | same |
| a 2,000-message first sync | ~2,060 | **~24** | **~24** |
| a quiet sync round (every 5 min) | 2 label calls + history + reconcile search + 1 raw per changed message | history + 1 listing (+ labels list every 5 min) = **2 to 3** | same |
| new mail in a round | + 1 raw per new message | none if it is in the recent listing (the normal case); else 1 raw each | same |
| opening a message / a triage body | 1 `gmail raw --format full` per message | 1 `gmail thread get` per *thread* (every body in it cached) | same |

(Version detection is one `gog version` per provider per daemon run.)

## The gog commands each method runs

Every call is

    gog --no-input --color never --json --wrap-untrusted=false --account <email> \
        --enable-commands-exact <cmd> --gmail-no-send <cmd as words> <args...>

with exactly one enabled command per process. `--gmail-no-send` is on every call except `gmail.send`, the
only command that can send. Message, thread, draft and label ids are checked (`[A-Za-z0-9_-]`, no leading
`-`) before they reach a command line, and so are search queries (no leading `-`). A *listing* below is
`gmail.messages.search <q> --max <n> [--page <t>] --timezone UTC --include-body [--include-recipients]
[--headers Message-Id,In-Reply-To,References,Content-Type]` (the bracketed flags by version).

| Method | gog commands (dotted name = the `--enable-commands-exact` value) |
|---|---|
| `sync`, label list | `gmail.labels.list` on a full sync's first page and when the cached list is older than 5 minutes; `gmail.labels.get <id>` only for `count_labels` (none by default) |
| `sync(None)`, full sync page | a listing of `in:anywhere newer_than:90d` (the window; `in:anywhere` with no window), at most `window_max` messages over all pages. First page: `gmail.raw <newest> --format minimal` (resume point). After the last page: `gmail.history --since <h> --max 1` (probe) |
| `sync(cursor)`, incremental | `gmail.history --since <historyId> --max 100 [--page <token>]`; then one listing of the reconcile query (`in:anywhere newer_than:7d`, newest 50, or more when history named more): new messages' records and label changes come from it; `gmail.raw <id> --format metadata` only for new ids it did not have (404 = removed) |
| backfill page (after an incremental round, when asked) | a listing of `in:anywhere before:<oldest synced> [after:<since>]`, 100 per page, at most one page per round and per `backfill_every` |
| history too old (404) | a windowed full sync again (the store keeps older mail); for a "quiet" cursor a catch-up instead: one listing of the newest page, `gmail.raw <newest> --format minimal`, a probe |
| `fetch_body_bundle` (the daemon's body fetch) | `gmail.thread.get <threadId>` (format full: every message's body and full record); `gmail.raw <id> --format full` when the message is not in that thread |
| `fetch_body` | `gmail.raw <id> --format full` |
| `fetch_raw` | `gmail.raw <id> --format raw` |
| `fetch_attachment` | `gmail.raw <id> --format full`; if the part is not inline, `gmail.attachment <id> <attachmentId> --out <tmp file>` |
| `SetSeen`, `SetFlagged`, `Move`, `Archive`, `Trash` | `gmail.batch.modify <ids...> [--add <labels>] [--remove <labels>]`, in chunks of 1000 |
| `SaveDraft` (raw mode) | find: `gmail.messages.search "in:drafts rfc822msgid:<our id>" --max 5` + `gmail.drafts.list --max 100 --all`; reply parent: `gmail.raw <parent> --format metadata`; then `gmail.drafts.update <draftId> --raw-file - [--thread-id <t>]` or `gmail.drafts.create --raw-file - [--thread-id <t>]` |
| `SaveDraft` (flags mode) | `gmail.drafts.create` / `gmail.drafts.update <draftId>` with `--to --cc --bcc --subject --body-file - [--body-html-file <f>] [--reply-to-message-id <p>] [--attach <f>]... [--clear-attachments]` |
| `DeleteDraft` | find (as above), then `gmail.drafts.delete <draftId> --force` (gog would otherwise prompt) |
| `Send` (raw mode) | `gmail.raw <parent> --format metadata`, `gmail.messages.search "in:sent rfc822msgid:<our id>" --max 1`, then, if not already sent, `gmail.send --raw-file - [--thread-id <t>]`, then `DeleteDraft` |
| `Send` (flags mode) | `gmail.messages.search 'in:sent newer_than:1d subject:"<s>"' --max 10` + `gmail.raw <hit> --format metadata`, then `gmail.send` with the flags-mode compose flags, then `DeleteDraft` |
| compose mode `Auto`, listings | `version` once per provider |
| `wait_for_change` | none: sleeps `timeout_s` (gog has no push without Pub/Sub) |

## How things map

- **Mailboxes.** System labels get roles (`INBOX` Inbox, `SENT` Sent, `DRAFT` Drafts, `TRASH`, `SPAM`,
  `STARRED`, `IMPORTANT`); `CATEGORY_*` become custom mailboxes named `Categories/Social` etc.; user labels
  are custom mailboxes with their Gmail name (`/` nests). `UNREAD` and `CHAT` are state, not mailboxes.
  Gmail has no label for All Mail, so there is a synthetic mailbox `ALL` (role `All`) that every message
  outside Trash and Spam is in. `ALL` is never sent to Gmail: `Move` to `ALL` only removes `from`.
- **Messages.** Gmail ids and thread ids are used as is. `mailboxes` = all label ids that are mailboxes
  (several per message). Flags: seen = no `UNREAD`, flagged = `STARRED`, draft = `DRAFT`; `answered` is
  always false (Gmail has no such flag). `date` is Gmail's `internalDate`. Addresses are parsed from the
  headers (quoted names with commas, groups, RFC 2047 words). `has_attachments` comes from the
  `multipart/mixed` root (the metadata format sends no parts).
- **Bodies.** The `full` payload is walked: `text/plain` and `text/html` parts are base64url-decoded
  (padded or not) and converted from their charset (valid UTF-8 wins, except 7-bit stateful charsets);
  several inline text parts are joined. Attachments are identified by Gmail's `partId`, which is stable,
  not by `attachmentId`, which Gmail changes between fetches. Kept headers: `List-*`, `Reply-To`, `Sender`,
  `Return-Path`, `Precedence`, `Auto-Submitted`, `Authentication-Results`, `Received-SPF` and a few more.
- **Reset.** Every page of a full sync (the first sync, and the one started when history has expired) has
  `Delta::reset` and, with a window, `Delta::window_floor`: the store then drops only unreported messages
  dated at or after it, never older mail. The floor is the oldest date the run listed, but never earlier
  than one day inside `newer_than:<days>d` (on gog before 0.34.2 dates come from the Date header, and an
  imported message can claim 1999); a resync after a backfill carries the older floor for backfill only. A cursor gog cannot read (from
  another provider, corrupt) is `Error::ResyncNeeded`; the engine then syncs from `None`.
- **Mailboxes.** `Delta::mailboxes` is `Some` when the label list was read (a full sync's first page, or the
  cached list was older than `labels_ttl`), else `None` (unchanged).
- **Backfill.** `request_backfill(since)` (the daemon's `sync.backfill`) marks the cursor; after each
  incremental round (at most every `backfill_every`) the provider answers `more: true` and the next call is
  one listing of older mail, returned with `Delta::backfill` (stored, never announced as new mail). When
  the listing runs out the cursor forgets the floor: all mail (or all since `since`) is stored.
- **Cursor.** JSON: `{"k":"full","h":<historyId>,"page":<token>,"n":<listed>,"q":<window query>,"lo":<oldest
  date>}` while a full sync pages, `{"k":"hist","h":<historyId>,"page":<token or null>,"lo":<oldest date
  synced>,"bf":{"before","since","page"},"step":true}` after (new keys are left out when empty; cursors
  from before windows existed still read, as an unwindowed run). A bare number is accepted as a historyId.
  The resume point of a full sync is fixed on its first page (the historyId of the newest message, one
  `gmail raw --format minimal`), so nothing that changes during the pages is missed; the probe at the end moves it to the mailbox's current
  historyId when nothing happened in between. When even that is older than Gmail keeps history (a mailbox
  where nothing changed for a long time) the cursor is marked `"quiet":true` and a 404 means catch-up, not
  a full resync.
- **Label changes.** gog's `gmail history` asks Gmail for `messageAdded` records only (v0.28.0 to 0.48), so
  history alone never says "read on the phone" or "archived on the web". The reconcile listing re-lists
  the newest recent mail (one search; gog includes label names) and reports, as partial records, the
  messages whose labels differ from what this process last reported: no refetch. Changes to older mail
  outside the reconcile window are only seen on a full sync. `reconcile_query: None` turns it off.

## Sending and drafts

Nothing here decides to send: `Action::Send` arrives only from the daemon, which refuses it from any
caller that is not a person (`safety::SEND_NEEDS_A_PERSON`). This crate makes sure the send is the only
gog process ever started without `--gmail-no-send`, and that it is allowed exactly one command.

Two compose modes, picked by `ComposeMode::Auto` from `gog version`:

- **Raw (gog 0.41 and later).** We build the MIME ourselves with `mail-builder`: multipart/alternative for
  text + HTML, multipart/mixed with attachments, RFC 2047 headers, `In-Reply-To` and `References` (the
  parent's References plus its Message-ID, capped at 20), Bcc kept for Gmail to deliver and strip, and the
  Gmail thread id from the parent. The Message-ID is `lbm.<sha256(local_id)[..16] hex>@<sender domain>`,
  the same for every save of a draft, which gives exact lookups: the Gmail draft for a `local_id` is found
  with `rfc822msgid:`, and a send first checks `in:sent rfc822msgid:` so a send that timed out but was
  delivered is not sent twice.
- **Flags (gog 0.29, what is installed today).** gog builds the MIME from `--to/--cc/--bcc/--subject/
  --body-file -/--body-html-file/--attach/--reply-to-message-id` (gog sets In-Reply-To, References and the
  thread itself). We cannot set the Message-ID, so the Gmail draft id is reported in `ApplyOutcome` and comes
  back from the store as `Draft::server_id` / `DeleteDraft::server_id` (also after a restart); without it
  (a draft this account's store never saw saved) saving creates a new Gmail draft rather than guessing, and
  deleting is a no-op, and send de-duplication is by an exact subject + To set + first 60 snippet
  characters match among mail sent in the last `dedupe_window_s` (6 h). gog 0.29 also cannot save a draft
  without a subject or body; a fully blank draft is simply not saved yet.

## Tests

`cargo test -p lumbridge-mail-gog`. All gog output in the tests is
made up (`tests/fixtures/*.json`, written by `tests/fixtures/gen_fixtures.py` from gogcli v0.29.0's output
structs and the Gmail API structs it encodes, and rows built in `tests/sync.rs` in the shapes of v0.28 and
v0.48; addresses are `example.*`). The runners are tested against `/bin/sh` standing in for gog and for ssh,
so the staging protocol, the renice prefix, `~/` expansion and the shared ssh connection (master started
once, a dead one replaced, no multiplexing falling back to direct) are exercised end to end without a
network. `tests/sync.rs` counts the processes per page (250 messages = 7 processes, old gog and new),
the window limit, label changes from listings, backfill pacing, thread bundles and version caching.
`budget.rs` tests the host cap across accounts, start spacing, backoff timing, the breaker and "a dropped
caller does not free a running slot" on tokio's paused clock. Nothing in the tests or in development
touched a real account.

Not verified against a live account yet: the exact JSON gog prints (field types are parsed leniently,
numbers or strings; the listing fields per version are read from gogcli's source, not from a run),
whether Gmail keeps our Message-ID on drafts and sends (raw mode's lookups depend on it), the error text
heuristics for exit code 1, how long a 100-message `--include-body` listing takes on the gog host, and
OpenSSH's multiplexing on the real host (the tests use a stand-in for ssh).
