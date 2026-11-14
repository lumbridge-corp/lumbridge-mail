# lumbridge-mail-store

The local mail store for Lumbridge Mail. A single SQLite file (rusqlite, bundled SQLite, WAL) holds:

- accounts (the whole `core::Account`, plus a sync cursor and the time of the last sync),
- mailboxes with unread/total counts kept from the local data,
- message metadata, grouped into threads by the provider's `ThreadId`,
- cached bodies and the attachments index (with where the daemon saved each file),
- drafts, AI triage per thread,
- the durable action queue,
- an FTS5 full-text index over subject, sender, recipients, snippet + cached body text, and attachment names.

There is no column for a password or a token anywhere (a test checks the schema). Secrets stay in the
keyring or in gog.

## Synchronous API

The API is plain synchronous rusqlite. The daemon calls it from blocking tasks
(`tokio::task::spawn_blocking`). A `Store` is `Send` but not `Sync`: keep one per thread (for example one
writer behind a mutex), and open more `Store`s on the same file for parallel readers. WAL lets readers run
while one writer writes; the busy timeout is 5 s.

Errors are `lumbridge_mail_core::Error`: SQLite and JSON problems come out as `Error::Other("store: ...")`,
unknown accounts / messages / threads as `Error::NotFound`, bad input as `Error::Invalid`, and a send from an
agent as `Error::Refused`.

## Use

```rust
use lumbridge_mail_core::{rpc::Caller, Action, Query, MailboxRole};
use lumbridge_mail_store::Store;

let mut store = Store::open(Store::default_path())?;     // or Store::open_in_memory()
store.upsert_account(&account)?;

// Sync: hand the stored cursor to the provider, apply what comes back.
let cursor = store.sync_cursor(&account.id)?;
let delta = provider.sync(cursor.as_ref()).await?;       // (in the daemon)
let applied = store.apply_delta(&account.id, &delta)?;   // changed_threads, new_messages, removed

// The unified inbox (anon accounts left out), one page at a time.
let page = store.list_threads(&Query { role: Some(MailboxRole::Inbox), limit: 50, ..Query::default() })?;
let next = store.list_threads(&Query { page: page.next.clone(), role: Some(MailboxRole::Inbox), ..Query::default() })?;

// Search.
let hits = store.list_threads(&Query { text: Some("from:ada invoice is:unread".into()), ..Query::default() })?;

// A thread, a body.
let messages = store.thread(&account.id, &thread_id)?;   // oldest first
store.put_body(&account.id, &message_id, &body)?;         // also indexes the body text
let body = store.body(&account.id, &message_id)?;         // None until fetched

// Local first: queue an action; it is applied locally in the same transaction.
let queued = store.enqueue(&account.id, Action::Archive { ids: vec![message_id] }, Caller::Person)?;

// The runner (in the daemon):
store.recover_interrupted()?;                              // at start
while let Some(job) = store.next_action(now)? {
    match provider.apply(&job.action).await {
        Ok(()) => store.action_done(job.id)?,
        Err(e) => { store.action_failed(job.id, &e, now)?; }  // Retry { at } or GaveUp { changed_threads }
    }
}
let wake_at = store.next_action_due(None)?;
```

## Public API

| Area | Methods |
|---|---|
| Open | `Store::open(path)`, `open_in_memory()`, `default_path()`, `schema_version()`, `set_retry_policy()` |
| Accounts | `upsert_account`, `account`, `accounts`, `remove_account` (cascades everything), `mailboxes(Option<&AccountId>)` |
| Sync | `apply_delta(account, &Delta) -> Applied`, `sync_cursor`, `reset_sync_cursor`, `last_sync`, `thread_for_references` (threading helper for IMAP), `assign_threads`, `learn_thread_headers`, `rethread` (providers without thread ids) |
| Read | `list_threads(&Query) -> ThreadPage`, `thread_summary`, `thread` (messages, oldest first), `message` |
| Bodies | `put_body`, `body`, `attachments`, `set_attachment_path`, `attachment_path`, `evict_bodies(fetched_before)` |
| Triage | `put_triage`, `triage`, `threads_needing_triage(limit)`, `category_counts(accounts, role)` (the `counts` method; anon wall applies) |
| Drafts | `save_draft`, `draft`, `drafts`, `delete_draft` (all local only) |
| Queue | `enqueue(account, Action, Caller) -> Enqueued`, `next_action(now)`, `next_action_for(account, now)`, `action_done`, `action_done_with(id, &ApplyOutcome)`, `action_failed(id, &Error, now) -> FailOutcome`, `cancel_action`, `recover_interrupted`, `next_action_due`, `queued_count`, `actions`, `clear_failed` |
| Search | `parse_search(&str) -> SearchQuery` (also used inside `list_threads`) |

## Behaviour worth knowing

**Deltas.** `apply_delta` is one transaction. `Delta::mailboxes: Some(list)` replaces the account's mailbox
list (mailboxes not in it are dropped; `Some(vec![])` drops them all); `None` means "unchanged".
`Delta::reset` marks the pages of a full sync: the store stamps every message such a page reports, and when
the run's last page (`more: false`) arrives it removes the account's messages no page reported. A run starts
with the first reset page after a non-reset one, or with any reset page while no cursor is stored (the
engine called `sync(None)`, e.g. after `Error::ResyncNeeded`). A message naming a mailbox the store
has not heard of gets a placeholder mailbox (name = id; Gmail's well-known label ids get their role). The
cursor is stored as given: `None` clears it, so the next sync is a full one. Messages that move to another
`ThreadId` move threads; a thread with no messages left is deleted.

**Providers without thread ids** (Zoho's listings): `assign_threads(account, &mut metas)` before
`apply_delta` sets each record's thread: a stored message keeps its thread unless new headers place it; a
new one joins by `In-Reply-To` / `References` (or a stored reply that names it), else by the normalised
subject (`core::threading`, as IMAP) when one side has a reply/forward prefix, they share someone besides
the account, and it is within `SUBJECT_WINDOW_S` (30 days) of the thread; else its own id is its thread. A
record whose thread is not its own id came with the provider's thread id and keeps it.
`learn_thread_headers(account, message, headers)` stores the ids a body brings and moves the message (or
replies that name it) accordingly; `rethread(account)` groups an account's stored mail again (an upgrade
from one message per thread).

**Threads.** A thread row is recomputed from its messages whenever one of them changes: subject of the first
message with one, snippet and date of the latest, participants (senders, oldest first, no repeats), counts,
flagged, attachments, mailboxes. A thread is listed in a mailbox if any of its messages is in it.

**Queries.** `accounts: None` means every account except anon ones (CONTRIBUTING.md rule 4), for lists and search
alike; an anon account is included only when its id is in `accounts`. `mailbox` matches the mailbox id in each
chosen account; `role` with no `mailbox` is "every mailbox with that role" (`Inbox` = the unified inbox).
`category` filters on stored triage; `unread_only` on the thread's unread count. Pages are keyset pages on
`(latest date, thread)`: tokens stay valid while mail arrives. `limit: 0` is 50; the most is 500.

**Search language.** Words match as prefixes in any field (`invoi` finds "invoice"). A word of 4+ letters
that matches nothing is replaced by the indexed words within 1 edit (4-7 letters) or 2 edits (8+), keeping
its first letter (typos and swapped letters: `invocie`, `resturant`). `"quoted phrases"` match exactly.
Operators: `from:` `to:` (`cc:` `bcc:` too, all recipients) `subject:` `filename:` `has:attachment`
`is:unread` `is:read` `is:starred` (`is:flagged`) `is:answered` `before:YYYY-MM-DD` `after:YYYY-MM-DD` (UTC
days; `before` is exclusive) `in:<role | mailbox name | mailbox id>` (`label:` too). Values can be quoted:
`subject:"weekly report"`. `newer_than:` / `older_than:` count back from now (`36h`, `3d`, `2w`, `6m`,
`1y`); `category:` is a triage category (`newsletter`, plurals fine) or one of Gmail's tabs by its label
(`promotions`, `social`, `updates`, `forums`, `primary`). Every part must match the same message; `-part`
must not (`-from:ci@example.com is:unread`); `a OR b` joins the parts next to it (`from:ada OR from:grace
subject:report`); `AND` and parentheses change nothing. Anything else (unknown operators, FTS syntax,
quotes, SQL) is searched as plain text; search input can't break the query.

`Query.since` (threads with mail from then on), `Query.needs_reply` and `Query.min_priority` (from the
stored triage; threads not triaged yet match neither) narrow any list or search.

**Body fill (schema 5).** `body_fill_next` hands out the next message without a body for the daemon's
background fill, newest first (new mail above the walk's top mark, then the walk down from its cursor),
leaving out drafts and messages set aside with `body_fill_unfetchable`; `body_fill_passed` moves the
cursor, `body_fill_rewind` starts again (with `since`, set-aside mail is looked at again),
`body_fill_progress` counts have/total. The marks live in the database, so a restart goes on where the
walk was.

**Action queue.** `enqueue` writes the action and applies it locally in one transaction (seen/flagged,
mailbox moves, archive = out of the inbox-role mailboxes and into the archive-role one if the account has
one, trash = into the trash-role mailbox, drafts saved or marked sending). It records what it changed.
`Action::Send` is refused unless the caller is `Caller::Person`. Actions go out in order: per account only
the oldest unfinished action is handed out, and while it runs or waits out a backoff nothing behind it does.
`action_failed` retries `Error::Offline` with exponential backoff (5 s doubling to 30 min, 12 attempts by
default, see `RetryPolicy`); any other error, or running out of attempts, fails the action for good: its
local change is rolled back, actions behind it on the same messages stay applied, and it is kept as `Failed`
until `clear_failed`. While actions are pending, a sync that brings the server's older state for those
messages does not undo them: pending actions are applied again on top. `recover_interrupted` (call it at
start) puts actions left running by a crash back in the queue with `interrupted = true`, so the daemon can
check Sent mail before it runs a `Send` again. `cancel_action` takes back an action that has not started
(e.g. undo send). A finished `Send` deletes its draft.

**Provider draft ids.** `action_done_with(id, outcome)` keeps a `SaveDraft`'s `outcome.server_id` on the
stored draft (`Draft::server_id`) and on queued actions for the same draft; `enqueue` fills it into
`SaveDraft`, `Send` and `DeleteDraft` when the caller left it out. Providers get the id back on every later
action, so they need not remember drafts in memory (gog's flags mode depends on this after a restart).

**Body index.** Bodies are indexed by `Body::text`; HTML-only bodies through `lumbridge-mail-render`'s plain
text (hidden text, scripts and styles are not indexed; quoted history and signature are).

## Benchmark

`tests/bench.rs`, an ignored test that generates made-up mail (syllable words, `example.*` addresses) and
times the main paths:

    cargo test --release -p lumbridge-mail-store --test bench -- --ignored --nocapture

`BENCH_MESSAGES=<n>` changes the size (default 500 000).

## Not done yet

- Negation (`-from:x`), `OR`, relative dates (`newer_than:7d`) and `larger:` in search.
- Ranking by relevance: results are newest first.
- A very common word on a big mailbox is the slow search (about 100 ms at 100k messages: the index is
  read whole for that word). Lists are about 1 ms at any size measured.
- Mailbox counts are what the store holds, not the server's totals for mail not synced yet.
