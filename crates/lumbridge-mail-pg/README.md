# lumbridge-mail-pg

The one-way Postgres mirror of Lumbridge Mail. The daemon keeps SQLite as its working store and writes
what it holds into a schema `mail` in a Postgres you run (17, with `vector`, `pg_trgm`), so
every device and your agent tools can query, search and parse mail in one place. Only the daemon writes it
(daemon to Postgres); nothing reads it back into the daemon.

This crate has the schema (versioned SQL in `migrations/`), the migration runner with its roles and
grants, the row shapes, the writer, and the workers' SQL (`src/work.rs`: jobs, embeddings, facts,
stats, hybrid search, the parsed facts). The daemon side (outbox drain, privacy rules, config, `lbmail pg`)
is `crates/lumbridge-maild/src/mirror.rs`; the outbox is the store's schema V4
(`crates/lumbridge-mail-store/src/mirror.rs`). Embeddings and parsing (the workers in
`crates/lumbridge-maild/src/pgwork/`, the extractors in `crates/lumbridge-mail-index`) are described in
`docs/MAIL-POSTGRES.md`.

## Schema `mail` (version 2)

| Table | What | Key |
|---|---|---|
| `accounts` | id, persona (never `anon`: CHECK), label, email, name, provider, ai_policy, readonly, kept | `id` |
| `mailboxes` | name, role (inbox, sent, archive, ...) | `(account, id)` |
| `messages` | thread_id, mailbox_ids[], subject, snippet, from/to/cc/bcc (jsonb), from_email, from_name, participants (text for search), sent_at, seen/flagged/answered/draft, has_attachments, size_bytes, rfc_message_id, in_reply_to, refs[], `sensitive_kind`, `fts` (generated tsvector) | `(account, id)` |
| `bodies` | body_text (clean text of the new content + signature, links kept), quoted_text, blocks (the render crate's sanitised block tree), headers (List-Id, Reply-To, Authentication-Results, ...), structured (schema.org JSON-LD / microdata objects from the HTML, v2), content_hash, `fts` | `(account, message_id)` |
| `attachments` | filename, mime, size_bytes, content_id: metadata only, never bytes | `(account, message_id, id)` |
| `triage` | category, priority, summary, needs_reply, due_at, by, at, suspicious | `(account, thread_id)` |
| `extracted` | action items and dates of a thread (kind, text, owner, at, all_day, by): for a future parser | identity |
| `parsed` | structured facts per message: kind + jsonb data, parser, parser_version, confidence, content_hash | identity; unique `(account, message_id, kind, parser)` |
| `embeddings` | per message and chunk: model (data, not schema), content_hash (of the chunk's text), `embedding halfvec(2048)` (v2; `--embedding-dim`, `--embedding-type`); HNSW `halfvec_cosine_ops` | identity; unique `(account, message_id, model, chunk)` |
| `jobs` | work queue `embed` / `parse` per message, filled by a trigger when a body arrives or changes; attempts, last_error, locked_until (lease or backoff), parked_at (v2) | `(kind, account, message_id)` |
| `job_stats` | per kind and hour: done, unchanged, skipped, failed, parked, calls, items, busy_ms (v2) | `(kind, hour)` |
| `schema_version` | version, name, checksum, params (`embedding_dim`, `embedding_type`, extension schemas), applied_at/by | `version` |

Views and functions: `mail.threads` (one row per thread with its triage); `mail.facts` (the most confident
parsed fact per message and kind) and one view per kind: `mail.receipts`, `mail.trips`, `mail.packages`,
`mail.subscriptions`, `mail.bills`, `mail.recruiting`, `mail.events`, `mail.newsletters` (typed columns,
defensive casts); `mail.search(q, account_ids, max_rows, since, until, query_embedding halfvec,
embedding_model)`: web-search syntax over subject, people, snippet,
body and quoted text (`english` stemming, GIN), fuzzy matching on subject and people (`pg_trgm`, one typo
in a word still matches), and nearest embeddings when a query vector is given, fused by reciprocal rank;
returns `account, message_id, thread_id, sent_at, subject, from_email, snippet, sensitive_kind, score,
matched[]` (`text`, `fuzzy`, `vector`).

**Deletes.** A message removed on the server is deleted here (hard delete), with its body, attachments,
embeddings, parsed facts and jobs (FK cascades). An emptied thread's triage goes too. An account that is
purged, turns anon, or is opted out is deleted with everything under it. No tombstones: the mirror says
what the mailbox holds now, and deleted mail should not linger in a shared store. A body evicted from the
daemon's local cache stays here.

**Sensitive mail** (security codes, resets, bank, government; `safety::sensitive_kind`, or a thread
triaged `sensitive`): `messages.subject` is the fixed one line ("A security code from Acme"), snippet
empty, `sensitive_kind` set. Triggers refuse any body, attachment, embedding, parsed fact, job or
extracted item for it, whoever writes, and drop them if a message turns sensitive later.

## Roles and reach

- `mail_writer`: owns the schema and everything in it; the daemon logs in as this.
- `mail_reader`: `USAGE` + `SELECT` on everything, `EXECUTE` on the functions,
  `default_transaction_read_only = on`; agents and tools log in as this.
- Nothing for `PUBLIC` or Supabase's API roles (`anon`, `authenticated`, `service_role`, `authenticator`),
  now or for tables added later (default privileges revoked). The schema is not in PostgREST's exposed
  schemas, and even if it were added there these roles could not use it. `tests/pg.rs` asserts this.

Both roles are created `LOGIN` without a password. Set them out of band, as the admin, interactively
(`\password` sends a hash; the password is not in shell history or server logs):

    psql "$(cat ~/.config/lumbridge-mail/pg-admin.dsn)" -c '\password mail_writer'
    psql "$(cat ~/.config/lumbridge-mail/pg-admin.dsn)" -c '\password mail_reader'

Use the fully qualified names (`mail.messages`, `mail.search(...)`): the migrations do not change any
role's `search_path`.

## Commands

The DSNs live in files of this user, mode 0600 (refused otherwise); never on the command line.

    # admin DSN (a role that may create roles, extensions and schemas; Supabase: postgres)
    printf '%s\n' 'postgres://postgres:...@db-host:5432/postgres?sslmode=disable' > ~/.config/lumbridge-mail/pg-admin.dsn
    chmod 600 ~/.config/lumbridge-mail/pg-admin.dsn

    lbmail pg migrate --dry-run                                   # the plan for an empty database, offline
    lbmail pg migrate --dsn-file ~/.config/lumbridge-mail/pg-admin.dsn --dry-run   # reads what is there, read-only
    lbmail pg migrate --dsn-file ~/.config/lumbridge-mail/pg-admin.dsn             # apply (one transaction)
    lbmail pg migrate --dsn-file ... --embedding-dim 2048 --embedding-type halfvec   # the defaults; set when 0002 is applied

Version 2 (`0002_embed_parse`) on a version 1 database: the embedding column becomes `halfvec(2048)`
(rows there are dropped and their messages queued again; production had none), the rest is additive
(`bodies.structured`, `jobs.parked_at`, `job_stats`, the views, `mail.search` with a `halfvec` argument).
Apply it before running a daemon of this build: the writer refuses an older schema ("run lbmail pg
migrate"), and its outbox waits meanwhile.

Then the daemon, as `mail_writer`:

    printf '%s\n' 'postgres://mail_writer:...@db-host:5432/postgres?sslmode=disable' > ~/.config/lumbridge-mail/pg.dsn
    chmod 600 ~/.config/lumbridge-mail/pg.dsn

    # config.toml
    [pg]
    enabled = true
    dsn_file = "~/.config/lumbridge-mail/pg.dsn"
    # ca_file = "~/.config/lumbridge-mail/pg-ca.pem"   # for sslmode=prefer/require with our own CA
    # batch = 500, poll_ms = 1000, max_backoff_s = 300

    [[accounts]]
    ...
    mirror = false        # per account: keep it out (anon accounts are always out)

    pkill -HUP -x lumbridge-maild   # re-read config.toml (or restart it); the first enable queues everything the store holds
    lbmail pg status      # on/connected, outbox pending + lag, stuck, last error, rows per table
    lbmail pg backfill [--account personal] [--retry-stuck]

TLS: `sslmode=disable` is plain (fine over the tailnet, which is encrypted); `prefer` (the default) and
`require` verify the server certificate against the Mozilla roots plus `ca_file`. rustls with ring; no
OpenSSL.

## Tests

`cargo test -p lumbridge-mail-pg` runs the unit tests everywhere. The Postgres tests run only with
`LUMBRIDGE_MAIL_PG_TEST_URL` set to a superuser DSN of a throwaway server; each makes its own database and
drops it. They create the cluster-wide roles and set their passwords, so never point it at a real server.

    docker run -d --name lbmail-pg-test -p 127.0.0.1:55871:5432 -e POSTGRES_PASSWORD=<random> pgvector/pgvector:pg17
    LUMBRIDGE_MAIL_PG_TEST_URL='postgres://postgres:<random>@127.0.0.1:55871/postgres?sslmode=disable' \
        cargo test -p lumbridge-mail-pg -p lumbridge-maild

`tests/work.rs` (version 2): a version 1 database with data migrated to version 2 (data kept, old
embeddings dropped and re-queued, `halfvec` column and index, params recorded, the reader sees the views
and cannot write the stats), the workers refuse a version 1 schema, jobs claimed / leased / retried /
parked / requeued / deferred and finished only if not re-queued meanwhile, two workers never claiming the
same job (200 jobs), idempotent embedding and fact writes (unchanged rows untouched, other models and
extra chunks replaced, other parsers' facts kept), sensitive mail refused, requeue by model, semantic
ranking by vector with account filters, text+vector fusion, the views (most confident fact, bad values read
as NULL), `parsed` filters.

Covered in `tests/pg.rs`: migrate from empty and again (idempotent, dry run writes nothing), ownership, the grants
(Supabase roles and PUBLIC get nothing even with a hostile default privilege in place; the reader reads and
searches, cannot write; an anon account row is refused), writer round trip (upsert, unchanged rows not
rewritten, flags, membership, delete cascades, account delete), sensitive mail (content dropped and
refused), search (full text, stemming, people, typo, account/date filters, web syntax, vectors), no schema
/ Postgres down. The daemon's tests (`crates/lumbridge-maild/tests/pg_mirror.rs`) cover the mirror end
to end: anon and opted-out accounts, sensitive mail, bodies, triage, flags, archive, removal, opt-out and
back, the outbox across a Postgres outage and a restart, and a throughput run.
