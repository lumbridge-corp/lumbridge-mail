# lumbridge-mail-resend

A `lumbridge_mail_core::Provider` for [Resend](https://resend.com)'s HTTP API. One `ResendProvider` is
one account: a sending domain (or some addresses on it) of a Resend team. It shows up in the sidebar
like any other account:

| Mailbox | Role | What is in it |
|---|---|---|
| Inbox | Inbox | Mail received on the domain (when receiving is turned on for it) |
| Sent | Sent | Mail the team sent from the account's addresses, with Resend's delivery state |
| Scheduled | custom | Scheduled emails not yet sent. Trash cancels them at Resend |
| Bounced | custom | A view of Sent: `bounced`, `complained`, `failed`, `suppressed` |
| Archive | Archive | What was archived here (local only) |
| Trash | Trash | What was deleted here (local only), and canceled emails |

It holds a connection (a `reqwest` client, a rate limiter), not mail: the daemon's engine owns the store
and the cursor. The API key is handed in by the caller (the daemon reads it from the OS keyring, an
environment variable or a command) and is never stored, logged or put in an error (`ApiKey`'s `Debug`
is redacted; Resend's error messages are scrubbed of anything key-shaped).

## Use

```rust
use lumbridge_mail_core::{AccountId, Provider};
use lumbridge_mail_resend::{AddressPattern, ApiKey, ResendConfig, ResendProvider};

let mut cfg = ResendConfig::new(AccountId::new("shop"), "hello@shop.example", ApiKey::new(key_from_keyring));
cfg.name = Some("Shop".into());
cfg.addresses = vec![AddressPattern::parse("@shop.example").unwrap()]; // the default: the email's domain
cfg.receiving = true;                                                   // false: no Inbox
let provider = ResendProvider::new(cfg)?;
let first = provider.sync(None).await?;                                 // then sync(Some(&cursor)) while more
```

Defaults: 4 requests per second, pages of 100, a full sync reads at most 10 pages per list (1000 sent,
1000 received, newest first), at most 50 received emails read for their snippet per sync call, polls
every 60 s, downloads capped at 50 MB, 30 s timeout per request, up to 4 retries after a 429.

## Endpoints used

Source of truth: the official OpenAPI spec, [resend/resend-openapi](https://github.com/resend/resend-openapi)
(`resend.yaml`, version 1.5.1, commit `0b2a456`, 2026-10-30), checked against the API reference on
2026-11-10. Base URL `https://api.resend.com`, `Authorization: Bearer <key>`.

| Method | Endpoint | Used for | Docs |
|---|---|---|---|
| `GET` | `/emails?limit=100[&after=<id>]` | Sent and Scheduled (list, newest first; no bodies) | [list-emails](https://resend.com/docs/api-reference/emails/list-emails) |
| `GET` | `/emails/{id}` | a sent email's body and state; scheduled emails that are due | [retrieve-email](https://resend.com/docs/api-reference/emails/retrieve-email) |
| `GET` | `/emails/{id}/attachments` | a sent email's attachments (with the body) | [list-email-attachments](https://resend.com/docs/api-reference/emails/list-email-attachments) |
| `GET` | `/emails/{id}/attachments/{attachment_id}` | one sent attachment's download link | [retrieve-email-attachment](https://resend.com/docs/api-reference/emails/retrieve-email-attachment) |
| `POST` | `/emails` + `Idempotency-Key` | Send | [send-email](https://resend.com/docs/api-reference/emails/send-email), [idempotency keys](https://resend.com/docs/dashboard/emails/idempotency-keys) |
| `POST` | `/emails/{id}/cancel` | Trash on a scheduled email | [cancel-email](https://resend.com/docs/api-reference/emails/cancel-email) |
| `GET` | `/emails/receiving?limit=100[&after=<id>]` | Inbox (list; no bodies, no headers) | [list-received-emails](https://resend.com/docs/api-reference/emails/list-received-emails) |
| `GET` | `/emails/receiving/{id}?html_format=cid` | a received email's text, HTML, headers, attachments, raw link, SPF/DKIM/DMARC | [retrieve-received-email](https://resend.com/docs/api-reference/emails/retrieve-received-email) |
| `GET` | `/emails/receiving/{id}/attachments/{attachment_id}` | one received attachment's download link | [retrieve-received-email-attachment](https://resend.com/docs/api-reference/emails/retrieve-received-email-attachment) |
| `GET` | `/domains` | the From check before a send; `lbmail resend domains` | [list-domains](https://resend.com/docs/api-reference/domains/list-domains) |
| `GET` | `/api-keys` | `lbmail resend keys` (names and dates only) | [list-api-keys](https://resend.com/docs/api-reference/api-keys/list-api-keys) |
| `GET` | the signed `download_url` / `raw.download_url` | attachment bytes, the raw received message; no `Authorization` header, fetched at once (links last about an hour) | [receiving attachments](https://resend.com/docs/dashboard/receiving/attachments) |

Paging ([pagination](https://resend.com/docs/api-reference/pagination)): lists are newest first;
`after=<id>` is the next (older) page, `has_more` says whether there is one; `limit` is 1..=100 (default
20). Limits ([rate limit](https://resend.com/docs/api-reference/rate-limit)): 10 requests per second **per
team** (every key, every website sending through it), with `ratelimit-*` and `retry-after` headers on a
429; daily (free plan) and monthly quotas, where received mail counts too. Errors:
[errors](https://resend.com/docs/api-reference/errors).

Not used: `POST /emails/batch` (one draft is one email), `PATCH /emails/{id}` (rescheduling), the
`/inboxes` API (see "Not covered"), webhooks.

## Sync and the cursor

`SyncCursor` is `rs1:` + JSON. Per list (sent, received): the newest creation time reported and the ids
at exactly that time; a paging run in progress (`after`, and the floor it stops at); the delivery state of
the newest page of sent mail; the scheduled emails still waiting.

- **Full sync** (`sync(None)`): both lists are paged newest first, one page each per call, every page with
  `Delta::reset`, until the end or `max_full_pages`. Older mail is not listed (Resend keeps sent mail for a
  limited time anyway).
- **Incremental**: one request per list from the top. Items newer than the stored point are new; the run
  pages on only while a whole page is new (a burst), up to `max_new_pages`. With nothing new a sync costs
  **two requests** (one with receiving off).
- **Delivery state**: the newest page of sent mail is compared with the states in the cursor; a sent email
  whose `last_event` changed is reported again (new snippet, Bounced membership). Changes to sent mail
  older than that page are not seen until a full sync.
- **Scheduled**: scheduled emails are remembered in the cursor and read again (`GET /emails/{id}`) once
  they are due, or after this process canceled them, so they move to Sent (or Trash) even when they are
  far down the list.
- **Received mail** new since the last sync is read one by one (`GET /emails/receiving/{id}`, at most
  `max_details_per_sync` per call) for the snippet and the threading headers; the rest is listed without a
  snippet and read when opened.
- **Filtering**: Resend's lists are team-wide (all domains, all websites). Only mail from (Sent) or to
  (Inbox: To, Cc, Bcc, `received_for`) the account's `addresses` is reported.
- A cursor from elsewhere is `Error::ResyncNeeded`. `Delta::mailboxes` is always `Some`.
- `wait_for_change` sleeps `poll_interval` (60 s) or the engine's timeout, whichever is shorter.

## How things map

- **Ids.** `MessageId` = `s:<resend id>` (sent) or `r:<resend id>` (received). Resend ids are UUIDs and are
  checked before they reach a URL. Attachment ids are Resend's attachment ids.
- **Threads.** Resend has no threads. A thread is one normalised subject (`Re:`, `Fwd:`, `AW:`... dropped)
  with one other party: the first To of mail we sent, the sender of mail we received. A question and our
  reply meet; the same receipt sent to a thousand customers is a thousand threads.
- **Delivery state.** `last_event` (`queued`, `sent`, `delivered`, `delivery_delayed`, `opened`,
  `clicked`, `bounced`, `complained`, `failed`, `suppressed`, `scheduled`, `canceled`) becomes
  `core::DeliveryState`. It shows in three places: the list row's snippet ("Delivered", "Bounced",
  "Scheduled for 2026-11-12 09:00 UTC"; Resend's list has no text to show instead), the mailboxes
  (Scheduled, Sent + Bounced, Trash), and `X-Lumbridge-Delivery` (`core::DELIVERY_HEADER`) in the body, read
  with `Body::delivery()`. The header is as of the body fetch (bodies are cached); the row and the
  mailboxes follow every sync.
- **Bodies.** Received: text and HTML as Resend parsed them (`html_format=cid`, so inline images stay
  `cid:` references to attachments), every header in `core::BODY_HEADERS` the message has (one entry per
  occurrence) plus Message-ID, In-Reply-To, References, Date, Sender, Return-Path, Received-SPF; when the
  message has no `Authentication-Results`, one is made from Resend's SPF/DKIM/DMARC results
  (`resend.com; spf=pass; dkim=pass; dmarc=pass`). Sent: text, HTML, Reply-To, Message-ID and the delivery
  header; Resend returns no other headers of sent mail.
- **Raw.** Received mail: the original message through `raw.download_url`. Sent mail: `Unsupported`
  (Resend does not keep it).
- **Attachments.** The attachment endpoint gives a signed link that is fetched at once, without the API key
  (only `https`, or `http` to loopback in tests), refused past `max_download_bytes`.

## Sending

Nothing in this crate sends on its own: `Action::Send` arrives only from the daemon's queue, which takes it
only from a person (`safety::SEND_NEEDS_A_PERSON`); read-only accounts never reach `apply`.

1. From is always the account's address and name. It must match the account's `addresses`, and its domain
   must be in `GET /domains` as `verified` (or `partially_verified`) with sending not disabled (cached 10
   minutes). A sending-only key cannot list domains; then Resend's own check applies (403).
2. To (at most 50), Cc, Bcc, subject, text, HTML, the account's `reply_to`, and the draft's attachments
   (read from disk, base64, at most 40 MB in all, Resend's limit). Names are quoted, control characters
   dropped, addresses with spaces, brackets or commas refused.
3. Replies: `In-Reply-To` and `References` from the draft; when the draft only names the message it
   answers (`reply_to`), they are taken from that received message. Sent as custom `headers`.
4. **No double sends.** The `Idempotency-Key` is `lbmail-` + the first 20 bytes of
   sha256(account id, draft `local_id`), the same on every attempt. Resend answers a repeat within 24 hours
   with the first send's id and does not send again; a repeat with different content is `409
   invalid_idempotent_request`, which becomes `Refused` (never retried); a concurrent repeat is `409
   concurrent_idempotent_requests`, `Offline` (retried). Limit: an attempt more than 24 hours after the
   first would send again; the daemon's retries stop well before that (about 3 hours), but an action left
   in the queue for days (machine off) is not protected.
5. The outcome's `server_id` is the sent email's `s:<id>`; the next sync lists it in Sent.

## Capabilities: what is local only

`ResendProvider::capabilities()` (core `Capabilities`, served by the daemon as `accounts.capabilities`):

| Action | Where | Why |
|---|---|---|
| Send | server | `POST /emails` |
| Trash | local, and **server for scheduled emails** (canceled) | the emails API has no delete |
| Mark read / unread, star | local | Resend keeps no flags for received or sent mail |
| Archive, move between Inbox / Archive / Trash | local | no folders |
| Save / delete draft | local | no drafts; `apply` returns an empty `ApplyOutcome` and the store keeps the draft |
| Move to Sent / Scheduled / Bounced | refused (`Unsupported`) | those are Resend's state |

Local changes are kept by the store: an incremental sync never reports a received email twice, so its
read state, star and folder stay. A full resync (a lost or unreadable cursor) reports everything again
and so resets them; a sent email whose delivery state changes is reported again and loses a local star or
folder.

## Errors

| Resend | `Error` |
|---|---|
| 401 (`missing_api_key`, `validation_error`), 400/403 `validation_error` about the API key, 403 `suspended_api_key`, `invalid_permission` | `Auth` (the daemon waits for the person) |
| 401 `restricted_api_key` (a sending-only key reading) | `Unsupported`; `sync` then reports nothing and stops listing (`is_send_only`), sending still works |
| 403 other `validation_error` (domain not verified, testing mode) | `Refused` |
| 403 `email_above_quota` | `Unsupported` (no content kept) |
| 404 | `NotFound` |
| 409 `concurrent_idempotent_requests`, `resource_locked` | `Offline` |
| 409 `invalid_idempotent_request` | `Refused` |
| 429 `rate_limit_exceeded` | wait `retry-after` (else `ratelimit-reset`, else 1 s; at most 60 s), retry up to 4 times, then `Offline` |
| 429 `daily_quota_exceeded`, `monthly_quota_exceeded` | `Offline` at once (no in-process wait) |
| 5xx, timeouts, connection errors | `Offline` |
| other 4xx | `Invalid` |

Messages are Resend's text, scrubbed (the key, anything `re_...`), at most 300 characters. The base URL
must be `https` (plain `http` only to loopback), so the key never crosses a network in clear text.

## Later: a webhook receiver

Resend can push `email.received` and the delivery events (`email.delivered`, `email.bounced`, ...) to a
webhook, signed with Svix headers (`svix-id`, `svix-timestamp`, `svix-signature`; the payload is metadata
only, the content is still fetched through the API). That would make `wait_for_change` return at once
instead of polling. It needs an endpoint Resend can reach from the internet, which the daemon does not have
and should not open by itself (CONTRIBUTING.md: no new network calls). The way to add it: a small relay on a
public host (or a tunnel) that verifies the Svix signature and forwards only "something changed for
<domain>" to the daemon over the tailnet; the daemon then calls `sync.now` for that account. Not built.

## Not covered

- Audiences, contacts, segments, topics, broadcasts, templates, automations, suppressions, metrics, logs.
- Creating or deleting domains, API keys or webhooks (only listing domains and key names).
- **Resend Inboxes** (`/inboxes`, new in the 2026 spec): per-address inboxes with real threads, folders
  (inbox/archive/spam/sent/trash), read state, labels, drafts, reply and forward. Where an account uses an
  Inbox, read state, folders and drafts could live on the server. That would be a second mode of this
  provider (same crate), built once the owner uses Inboxes; it needs a full-access key for everything but
  sending drafts and replies.
- Scheduling a send from the composer (`Draft` has no send time), rescheduling (`PATCH /emails/{id}`),
  tags, batch sends.
- Mail older than the full sync's page limit, and delivery changes to sent mail older than the newest page
  (until a full sync).

## Tests

`cargo test -p lumbridge-mail-resend` and
`cargo clippy -p lumbridge-mail-resend --all-targets --no-deps -- -D warnings`.

`fake::FakeResend` is a fake Resend API on 127.0.0.1 in the test process (also used by the daemon's
`tests/resend.rs`), with made-up mail and a made-up key. It pages like Resend, checks the bearer key,
keeps idempotency keys, can answer 429 with Retry-After, drop the connection after accepting a send,
behave as a sending-only key, and serves download links that record whether the key was sent along.
Covered: paging and the incremental cost, 429 backoff and giving up, a send retried after a lost answer
going out once, a changed payload under the same key refused, scheduled cancel, due scheduled mail,
delivery-state changes and the Bounced view, inbound mail with attachments, inline images and raw, the
download cap, auth failures (no key in the message), sending-only keys, the From check, local-only actions
touching nothing. Nothing in development or the tests talked to resend.com.

Not verified against the live API yet: the exact JSON (fields are read leniently), whether `/emails` lists
scheduled emails with `last_event: scheduled`, whether `GET /emails/{id}/attachments` answers for an email
without attachments, the `html_format` query parameter, and how Resend orders items created in the same
millisecond.
