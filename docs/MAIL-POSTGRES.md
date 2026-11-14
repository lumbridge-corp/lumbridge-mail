# Mail in Postgres

Mail can go into a Postgres database you run, so every device and your agent tools can query, search
and parse it in one place. Decided: the daemon keeps SQLite as its working store; Postgres is a one-way
mirror written only by the daemon (daemon to Postgres), the shared record others read.

    providers ─ sync ─ lumbridge-maild ─ SQLite store ─┬─ (triggers) outbox ─ mirror task ─ Postgres `mail`
                                                      └─ app, lbmail, agents (JSON-RPC)      │
                                                                     agents, tools (mail_reader) ┘

Code: `crates/lumbridge-mail-pg` (schema, roles, writer; its README has every command),
`crates/lumbridge-mail-store/src/mirror.rs` (outbox, store schema V4),
`crates/lumbridge-maild/src/mirror.rs` (drain task, privacy rules, `lbmail pg`).

## The outbox

- Triggers on the store's tables write one entry per changed thing, in the same SQLite transaction as the
  change: `account`, `mailbox`, `message` (metadata, flags, mailbox membership, removal), `body`,
  `triage`. An entry names the thing, not its data. Repeated changes bump a `gen` counter on the same
  entry instead of adding rows, so the outbox is bounded by the number of changed things.
- The mirror task (its own SQLite connection, its own Postgres connection) takes the oldest 500 entries,
  reads the current rows, applies the privacy rules, and writes one Postgres transaction of idempotent
  upserts and deletes (pipelined). Then it deletes the entries whose `gen` did not move meanwhile.
- Postgres down: nothing is marked sent; the task backs off (1 s doubling to `max_backoff_s`) and the
  outbox keeps growing on disk. A restart picks it up. Sync and the app never wait for any of this.
- A batch Postgres refuses is retried entry by entry; an entry refused 10 times is set aside ("stuck" in
  `lbmail pg status`, `lbmail pg backfill --retry-stuck` tries again).
- Turning the mirror on the first time (or after it was off: nothing is recorded while off) queues
  everything once. On every (re)connect the task reconciles accounts: those Postgres must not have are
  deleted, allowed accounts missing there are queued in full.
- Cost to sync: a few small indexed writes per changed message, only while `[pg] enabled`. A full resync
  that lists unchanged mail records nothing (the triggers compare old and new values; mailbox membership
  rewrites are recognised by parking removed pairs until the transaction ends).

## What is mirrored

| | Mirrored | Not mirrored |
|---|---|---|
| Anon accounts | nothing | not even the account row (triggers skip them, the task skips them, the schema refuses `persona = 'anon'`) |
| `mirror = false` accounts | nothing | and deleted from Postgres if they were there |
| Sensitive mail | metadata (addresses, dates, flags, mailboxes, ids, size) with the one line as subject and `sensitive_kind` | snippet, body, attachments, triage summary (replaced by the line), embeddings, parsed facts, extracted items (refused by triggers too) |
| Other mail | metadata, snippet, body as clean text + sanitised block tree, selected headers, attachment metadata, triage | received HTML, attachment bytes, all other headers, drafts, the action queue, sync cursors, local file paths |

Removal on the server is a hard delete in Postgres (cascading). A body dropped from the local cache stays.

## Embeddings and parsing (schema version 2)

    mirror ─ bodies (trigger) ─ mail.jobs ─┬─ embed worker ─ /v1/embeddings ─ mail.embeddings ─ mail.search(q, vector)
                                           └─ parse worker ─ rules, then [ai] chat ─ mail.parsed ─ mail.receipts, trips, ...

Code: `crates/lumbridge-mail-pg/migrations/0002_embed_parse.sql` and `src/work.rs` (the SQL),
`crates/lumbridge-mail-index` (chunking, the embeddings client, schema.org extraction, rule and model
extractors, validation), `crates/lumbridge-maild/src/pgwork/` (the workers, `search.semantic`, `lbmail`).

**Where they run.** In the daemon, each worker one task on its own Postgres connection, as `mail_writer`
(the DSN of `[pg] dsn_file`). No `mail_worker` role: the same process holds the writer's DSN, so a second
role would not separate anything. The schema's triggers are the second wall (no embedding, fact or job for
sensitive mail, whoever writes). Two daemons (or a restart mid-batch) never double work: jobs are claimed
with `FOR UPDATE SKIP LOCKED` and leased (`locked_until`); a dead worker's jobs come back when the lease
ends. A job is identified by its row and the time it was queued, so a body that changes while its job is
being worked on is queued again rather than lost. Newest jobs first: new mail is searchable at once while
a backfill drains.

**Schema version 2** (`lbmail pg migrate`): `mail.embeddings.embedding` is `halfvec(2048)` (our embedding
model returns 2,048 dimensions; pgvector's HNSW takes `vector` up to 2,000 and `halfvec` up to 4,000;
`--embedding-dim`, `--embedding-type halfvec|vector` choose it; rows that were there are dropped and their
messages queued again), HNSW `halfvec_cosine_ops`; `mail.search` takes the query vector as `halfvec` and
filters vector candidates by account before cutting them; `bodies.structured` (schema.org objects from the
HTML, extracted by the daemon when it mirrors a body, since the HTML itself is never mirrored);
`jobs.parked_at`; `mail.job_stats` (per kind and hour: done, unchanged, skipped, failed, parked, calls,
rows, busy time); views `mail.facts` (the most confident fact per message and kind), `mail.receipts`,
`mail.trips`, `mail.packages`, `mail.subscriptions`, `mail.bills`, `mail.recruiting`, `mail.events`,
`mail.newsletters`. Same grants discipline: `mail_reader` selects everything and runs the functions;
PUBLIC and Supabase's API roles get nothing (`tests/pg.rs` checks every object of version 2 too).

### Embedding worker (`[pg.embed]`)

Per round: claim up to `batch_jobs` jobs; for each, refuse it if a privacy rule says so (below); build the
chunks: `Subject: ...\nFrom: <sender name>\n\n` + windows of the clean new body text (quoted history
removed), 3,000 characters with 300 overlap, at most 4 per message; if their sha256s equal what is stored
for this model, finish without a call. The rest go to the endpoint in requests of up to `batch_inputs`
texts (a message's chunks stay together), paced by `max_per_minute` (spread evenly) and `concurrency`
(1: the embedding server is a shared GPU box). A request's messages and their jobs are written in one
transaction. A failed request retries its jobs after `retry_s` doubling to `max_retry_s`, and parks a job
after `max_attempts` with `last_error` (status codes only); the worker then pauses before its next round.
Vectors of the wrong size are a configuration error: the worker stops and says so in `lbmail pg status`.

A model change: set `[pg.embed] model`, reload, `lbmail pg reembed`; messages without the new model's
embeddings are queued, and each message's old rows are replaced when its new ones are written. Searches
use the configured model only, so they are right during the change (with fewer vector hits).

### Parsing worker (`[pg.parse]`): rules first, model second

| Kind (`mail.parsed.kind`) | Rules (no model) | Model (`[ai]` endpoint) |
|---|---|---|
| receipt | schema.org `Order`, paid `Invoice` | merchant, total, currency, date, order id |
| travel | `FlightReservation`, `TrainReservation`, `BusReservation`, `LodgingReservation`, `RentalCarReservation` | carrier, number, from/to, depart/arrive, confirmation |
| package | `ParcelDelivery`; UPS / USPS tracking numbers in shipping mail | carrier, tracking id, status, expected |
| subscription | | service, event, amount, period, renews on |
| recruiting | | company, role, recruiter, stage, asks |
| bill | unpaid `Invoice` with a due date | payee, amount, currency, due date |
| event | `EventReservation`, `FoodEstablishmentReservation`, `*Event`; calendar invites (a `text/calendar` part) | what, when, end, where |
| newsletter | `List-Id`, `List-Unsubscribe` (https, mailto), one-click (`List-Unsubscribe-Post`) | never |

The thread's triage category picks which kinds a model may be asked about (transactional: all but
recruiting; notification: package, event; people's mail: recruiting, event, travel; newsletters,
promotions, spam, sensitive: none), kinds the rules found are skipped, and the words of the subject and
body pick at most one: so most mail costs no model call. A message whose thread is not triaged yet waits
up to `triage_wait_s`. Mail that talks to an assistant gets the rules only. The call is the AI crate's
fence and preamble plus a JSON schema; every fact, from rules or model, is validated (types, bounds,
real dates in a window around the mail's date, amounts 0..1,000,000 with two decimals, ISO currencies; from
a model, amounts and identifiers must appear in the mail) and a fact that fails is not stored. Facts are
stored with `parser` (`rules/jsonld`, `rules/headers`, `rules/invite`, `rules/text`, `model/<label>`),
`parser_version`, `confidence` (rules 0.7-0.95, a model's own rating capped at 0.9, below 0.5 not stored)
and the body's `content_hash`. `lbmail pg reparse` parses everything again (after a parser update).

### Privacy rules (each tested in `crates/lumbridge-maild/tests/pg_work.rs` and `src/pgwork/mod.rs`)

- Mail is data: prompts fence it as untrusted data, mail that addresses an assistant never reaches a model,
  answers are schema-constrained and validated; nothing in a mail changes what the workers do.
- Sensitive mail is never embedded or parsed: it has no body in Postgres, the workers refuse it before
  reading, and the schema refuses its rows.
- AI `off` accounts are neither embedded nor parsed (not even by rules). Anon accounts are not in Postgres
  (and are refused if one were). `local_only` accounts go only to endpoints configured `place = "local"`
  (`safety::ai_allowed`; Postgres' copy of the policy is checked too).
- Every request carries the configured headers (production: the untraced header, so the tracing proxy
  does not archive mail). Logs and job errors carry ids, counts and status codes, never mail content.

### Search

`lbmail search --semantic "<question>"` (RPC `search.semantic {q, accounts?, limit?, since?, until?}`):
the question is embedded (`input_type: query` / `query_prefix` when configured) and `mail.search` fuses
vector neighbours with full text and fuzzy matching. If the mirror is off, Postgres or the embedding
endpoint does not answer, or a named account is not in Postgres (anon, `mirror = false`), the local
store's full-text search answers and the note says why (`mode: local`). Without an embedding endpoint
configured it is a Postgres text search, and says so. Either way `threads` holds the matching threads
from the local store, best first, each once (what `lbmail search --semantic` lists, as the caller may see
them): in Postgres mode each hit is placed by its message, so a thread the store regrouped since
(`Store::rethread`, headers learnt from a body) shows as it is now even before the mirror has caught up,
and the hit's `thread_id` is corrected to it. A regrouping changes `messages.tid`, which the outbox
records like any change, so `mail.messages.thread_id` follows (`tests/pg_mirror.rs`).

### Cost per 1,000 typical messages

Measured against the fakes (`worker_throughput_on_a_typical_mix`: 45% people, 30% newsletters and
promotions, 10% notifications, 10% receipts, shipping and bills, 5% recruiters): see the PR for the
numbers of the last run. Embedding: about 1.05 chunks per message, 16 per request, so ~65-95 requests per
1,000 messages. Parsing: ~120-150 model calls per 1,000 (receipts, bills, recruiters, the odd shipping
mail whose tracking number the rules already read but which also reads like an order); newsletters,
promotions, notifications and people's mail cost none.

### Not done

- `mail.extracted` is still empty: the daemon does not keep `ai.extract` results (planned).
- Messages without a cached body are not embedded (only bodies queue jobs). The daemon's background body
  fill (`[daemon] body_fill`, `crates/lumbridge-maild/src/fill.rs`) fetches the missing bodies slowly at the
  lowest priority, and each filled body is mirrored and queued like any other; a subject + snippet sweep
  for mail whose body cannot be fetched is still not built.
- Calendar invites: the `.ics` part is not mirrored, so an invite's time comes only from a model or not at
  all.
- pgmq / pg_cron are not used; `mail.jobs` is enough for these two workers.
