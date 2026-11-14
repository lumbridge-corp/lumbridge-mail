# lumbridge-mail-zoho

A core `Provider` for one Zoho Mail address, through Zoho's hosted MCP server: JSON-RPC 2.0
`tools/call` over HTTPS POST with a Bearer token. The server answers with a JSON body or an SSE stream
whose `data:` lines carry the JSON-RPC response; both are read. A tool's result is `content[0].text`
holding Zoho Mail's REST answer as JSON, wrapped twice: `{headers, status: "success", data: {requestId,
status: {code, description}, data: {<named key>: ...}}}`. `ZohoConn::call` checks each layer's status and
returns the innermost `data` (`{primaryAccount}`, `{folders}`, `{messages, nextPageToken}`,
`{originalContent}`, ...); a single wrapping is read too. A text that is not JSON is the server's error
message, even with `isError: false` (e.g. "Invalid keys found in path variable ..."), and becomes the
error's reason (first 200 characters).

A Zoho sign-in is usually one mailbox (a primary address plus aliases, maybe on several domains): one
Lumbridge account. Should a sign-in hold several mailboxes, each is its own Lumbridge account (`kind = "zoho"` in the daemon's config); accounts with the same `mcp_url`,
`token_url`, `client_id` and `token_file` share one `ZohoConn`: one HTTP client, one token, one refresh at
a time, one budget. Each account finds its Zoho account id with `ZohoMail_list_accounts` (by address;
once per connection) unless `zoho_account_id` is set, and passes it as `path_variables.accountId`.

## Only mail tools, only as their schemas say

The same MCP server has organisation-admin tools (delete users, reset passwords, domains, policies...).
`mcp::ALLOWED_TOOLS` is the complete list of tools this crate calls; `ZohoConn::call` refuses any other
name with `Error::Refused` before anything is sent (test: `admin_tools_are_refused_before_anything_is_sent`).
Every call's arguments are checked against that tool's own input schema (`src/tool-schemas.json`, the
server's `tools/list` entries for these tools): only declared sections and keys, required path variables,
declared JSON types. A mismatch is `Error::Invalid` and nothing is sent. The fake server checks the same.

    ZohoMail_list_accounts  list_folders  list_mails  get_mail  get_mail_html  get_mail_headers
    get_mail_original  list_mail_attachments  mark_mails_as_read  mark_mails_as_unread  apply_mail_flag
    move_mails  archive_mails  unarchive_mails  move_mails_to_trash  send_mail  reply_to_mail

## The tool calls each method makes

| Method | Calls |
|---|---|
| first use | `list_accounts` (once per connection, unless `zoho_account_id`) |
| `sync(None)` (full) | `list_folders`, then one `list_mails {folderId, start or pageToken, limit, sortBy: time, sortOrder: false}` per call (no `fields`: the default row has read state, `flagId` and summary; the next page comes from the answer's `nextPageToken` when there is one), folder by folder (Inbox, Sent, Drafts, the rest, then archived mail with `includeArchived: true`), newest first, until the window |
| `sync(cursor)` (round) | `list_mails {start: 1, limit: poll_size}` for Inbox, Sent and Drafts; every `cold_every` rounds also `list_folders` and every other folder and the archive; `get_mail` for at most `max_lookups` messages that left a page without turning up elsewhere |
| `sync` (burst / deep / backfill) | one `list_mails` page per call |
| `fetch_body` | `get_mail_html`, `get_mail_headers {rawFormat: true}`, `list_mail_attachments {includeInline: true}` |
| `fetch_raw` | `get_mail_original` |
| `fetch_attachment` | `list_mail_attachments`, `get_mail_original` (Zoho's tools have no attachment download: the part is cut out of the raw message, by Content-ID, else by file name and position) |
| `SetSeen` | `mark_mails_as_read` / `mark_mails_as_unread {messageIds}` (250 ids per call) |
| `SetFlagged` true | `apply_mail_flag {flagId, messageIds}` (`flag`: info 1, important 2, follow-up 3) |
| `SetFlagged` false | none: `Unsupported` (the tool's `flagId` has no "not set") |
| `Archive`, `Move` to `archive` | `archive_mails {messageIds}` |
| `Trash` | `move_mails_to_trash {messageIds}` |
| `Move` | `move_mails {destinationFolderId, messageIds}`, after `unarchive_mails` when moved out of `archive` |
| `SaveDraft`, `DeleteDraft` | none (local) |
| `Send` | `list_mails` on Sent (duplicate check), then `reply_to_mail {messageId: parent}` when the draft answers a Zoho message, else `send_mail {fromAddress, toAddress, ccAddress, bccAddress, subject, content, contentType, inReplyTo, referencesHeader, applicationMessageId}` |

`Send` is never retried by the connection on a transport failure (it may have gone out); the daemon's
queue retries it, and the retry first checks Sent for the same subject, recipients and opening text from
the last `dedupe_window_s` (6 h). Drafts with attachments are refused (`Unsupported`): the tools need an
upload step they do not offer.

## Capabilities

| | Where |
|---|---|
| send | server (person only, as always) |
| read state, flag (set), move, archive, trash | server |
| flag (clear) | not on the server: the action fails with `Unsupported` (never retried); clear it in Zoho Mail |
| drafts | local (no draft tool) |
| raw message | server (`get_mail_original`) |

## Cursor

`zo1:` + JSON. Zoho has no history or change feed, so the cursor keeps, per folder (and the archive),
the newest page it last listed: message id -> read/flag bits and date (`poll_size` entries, 50 by
default). An incremental round lists that page again:

- an id not on the old page, or with different bits, is reported (new mail, read or flagged elsewhere,
  or moved in);
- an id that left the page while its date is newer than the page's oldest row (or the page is no
  longer full) moved out or was deleted: if no other folder listed this round has it, it is looked up
  with `get_mail` (gone: `removed`; found: reported in its new folder);
- a full page with nothing known on it is a burst: extra pages follow, one per `sync` call, until mail
  at or before the old page's newest date (at most `max_new_pages`).

So a quiet round costs three calls (Inbox, Sent, Drafts). Folders other than those, and the folder list
itself, are looked at every `cold_every` (10) rounds. Changes to mail older than the polled page (read on
the phone a month later) come in through a **deep pass** every `deep_every` (60) rounds: `deep_pages`
pages per folder, one per call, everything reported as a partial record.

The cursor also holds the full sync's progress (folder, `start`, count, and the window floor so far),
the folder roles, the floor of what the store has (lowered by backfill) and a backfill in progress.
Every listing record is `partial`: the store keeps the addresses and ids it already has.

## The window and backfill

The first sync lists the last `window_days` (90) days and at most `window_max` (2000) messages **per
folder**. `Delta::window_floor` is the day limit, raised to just above the oldest message listed in any
folder the count limit cut short. `request_backfill(since)` (and `backfill = true`) fills in older mail
one page per round (at most every `backfill_every`), folder by folder, starting a little above where the
full sync stopped and skipping anything newer than the floor.

## The token

The token file is the `mcp-remote` format: a JSON object with `access_token`, `refresh_token`,
`expires_in`, `token_type`, `scope`, `api_domain`. `expires_in` counts from the file's mtime. Another
program may hold the same refresh token, so:

- refresh only when the access token has expired (60 s early) or a call came back 401 (or a tool result
  says the OAuth token is invalid) - never on a timer;
- before refreshing, read the file again: if it holds a different, unexpired token, use that;
- refresh = form POST to `token_url`: `grant_type=refresh_token`, `refresh_token`, `client_id`,
  `resource=<mcp_url>`; take `access_token` and, when present, the new `refresh_token`;
- write the file atomically (temporary file next to it, the old file's mode, rename), keeping every
  other key.

A refused refresh is `Error::Auth` (sign in again); a busy token endpoint is `Offline`. Token values never
appear in errors, logs or `Debug` output; the URLs and client id are not printed either.

## Budget

Per connection (`config::Limits`): at most `max_in_flight` (2) calls at once, `max_calls_per_min` (60)
with bursts of `burst` (4), exponential backoff with jitter after transport failures (connection,
timeout, 5xx), a circuit breaker after `breaker_failures` (5) in a row for `breaker_cool` (5 min), and a
429's Retry-After honoured. Reads and idempotent changes are retried `retries` (2) times after the
backoff. `Provider::load()` reports it (scope `zoho:<token file name>`).

An MCP session is opened only if the server asks for one (a 400 that mentions a session, or a 404 with a
session): `initialize`, `notifications/initialized`, then `mcp-session-id` on every request.

## Errors Zoho answers

An error answer's message carries Zoho's description and, wherever in the answer Zoho put them (a nested
`data`, a list, a JSON string), its `errorCode`, `location` and `locationType`: e.g. `Zoho said 400 to
ZohoMail_list_mails: Bad Request (errorCode EXTRA_PARAM_FOUND, location includeArchived, locationType
queryParam)`. These are server words (identifier characters only, 64 at most), never mail.

A 400 with an `errorCode` is about the call itself, so it is `Error::Unsupported` and permanent for that
call: the provider remembers it for the session (the parameter Zoho named, for every call of that tool
that carries it; else that shape of call: tool plus argument names) and does not send it again until the
daemon restarts. The sync goes on without it: a refused archive view (`includeArchived`) lists nothing,
a refused `fields` lists without it. Any other refused call fails as before with the full reason.

## Parsing

Written without a single sample answer from a real account (none may be read into a session), from Zoho
Mail's REST documentation (the tools wrap those endpoints). Every field is optional; ids and numbers
are accepted as strings or numbers; `null` lists are empty; unknown fields are ignored; HTML-escaped
addresses (`&lt;a@b&gt;`) are unescaped; `Not Provided` is no address. An answer with no recognisable
shape fails with an error that names its keys (logged the same way), never its values.

Checked against real answer shapes (keys and types only): the double wrapping; `list_accounts` ->
`{primaryAccount: {accountId (string), emailAddress (string), emailAlias: [..], ...}}`; `list_folders` ->
`{folders: [{folderId, folderName, folderType, path, isArchived}]}`; `list_mails` -> `{messages,
nextPageToken}` with every value a string, the flag as `flagId` and **no `threadId`** (see "Threads");
`get_mail_original` -> `{originalContent}`. Still
to confirm with `lbmail zoho probe`: `get_mail`, `get_mail_html`, `get_mail_headers`,
`list_mail_attachments`, `includeArchived` across folders, and `send_mail`'s answer.

## Threads

A `list_mails` row has no `threadId`, so the provider reports each message with its own id as its thread
and the daemon has the store group them (`Store::assign_threads`, the same scheme as IMAP's: headers, then
the normalised subject with a reply or forward prefix, someone besides the account on both, within 30 days;
see `lumbridge-mail-store`'s `threading.rs`). `fetch_body` also copies `Message-ID`, `In-Reply-To` and
`References` from `get_mail_headers` into `Body::headers`; the daemon hands them to
`Store::learn_thread_headers`, which moves the message (or the replies that name it) into the right
thread. Mail stored one message per thread by an older daemon is grouped once at start (`Store::rethread`,
noted in `data/threads-v1/`), without a resync.

`fields` on `list_mails` / `get_mail` would return `threadId` (it is in the schema's default list), but it is a
list of columns, not an addition: asked for, live Zoho drops the read state, flag and summary, and the
schema offers no way to keep the default set. So `list_fields` stays empty. If it is set and a row does
carry a `threadId`, that thread is kept as given.

## Probe

`lbmail zoho probe --account <id>` (in the CLI process, from the account's config; no daemon) calls the
read-only tools once each: `list_accounts`, `list_folders`, a 3-message Inbox page of `list_mails`, then
`get_mail`, `get_mail_html`, `get_mail_headers`, `list_mail_attachments` and `get_mail_original` for the
newest message. Per call it prints ok/error, Zoho's status code, the answer's shape (key names, JSON types
with digit strings marked `digits`, list lengths; keys that could be values masked) and whether the parser
accepted it (as counts). It prints no ids, addresses, subjects or bodies: only status codes, `folderType`
values and the server's error text. `--json` gives the same as JSON; the exit status is non-zero when a
call failed or the parser rejected an answer.

## Tests

`fake::FakeZoho` is an in-process fake MCP server and token endpoint (plain HTTP on 127.0.0.1, made-up
mail): JSON and SSE answers, a session-requiring mode, token refresh with rotation, 401 as HTTP or inside
the tool result, 429 with Retry-After, 5xx, `null` lists, and two accounts on one sign-in. The daemon's
`tests/zoho.rs` uses it too.

    cargo test -p lumbridge-mail-zoho
    cargo clippy -p lumbridge-mail-zoho --all-targets -- -D warnings
