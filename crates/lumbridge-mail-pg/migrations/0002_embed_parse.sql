-- Lumbridge Mail mirror, schema version 2: what the embedding and parsing workers need.
--
-- Runs as mail_writer, like 0001. Placeholders filled in by the runner: {{embedding_dim}},
-- {{embedding_type}} (halfvec, the default, or vector), {{embedding_ops}} (the HNSW operator class
-- for cosine distance on that type), {{vector_schema}}, {{trgm_schema}}.
--
-- 1. mail.embeddings.embedding becomes {{embedding_type}}({{embedding_dim}}). pgvector's HNSW index
--    takes `vector` up to 2,000 dimensions and `halfvec` up to 4,000; our embedding model returns
--    2,048. Embeddings are derived data: rows that were there are dropped and their messages queued
--    to be embedded again.
-- 2. mail.bodies.structured: schema.org objects (JSON-LD, microdata) the sender put in the HTML
--    for machines (orders, flights, reservations, parcels, events), extracted by the daemon when it
--    mirrors the body. Data, never shown, never instructions.
-- 3. mail.jobs gets parked_at (a job that failed too often waits for `lbmail pg requeue`), and a
--    re-queued body clears it. mail.job_stats counts what the workers did per hour.
-- 4. mail.search takes the query vector as {{embedding_type}}.
-- 5. Views over mail.parsed: mail.facts (the best fact per message and kind) and one per kind.

-- 1. Embeddings.
-- Load pg_trgm into this session first. Until its library is loaded, `pg_trgm.*` is only a
-- placeholder setting, and a server such as Supabase refuses a function-level
-- `SET pg_trgm.word_similarity_threshold` from a role that is not a superuser (seen live: 42501).
-- 0001 never met this because creating its trigram indexes loaded the library.
SELECT {{trgm_schema}}.similarity('a', 'a');

DROP INDEX mail.embeddings_hnsw;
WITH gone AS (
    DELETE FROM mail.embeddings RETURNING account, message_id
)
INSERT INTO mail.jobs (kind, account, message_id)
SELECT DISTINCT 'embed', g.account, g.message_id FROM gone g
ON CONFLICT (kind, account, message_id) DO NOTHING;
ALTER TABLE mail.embeddings DROP COLUMN embedding;
ALTER TABLE mail.embeddings ADD COLUMN embedding {{embedding_type}}({{embedding_dim}}) NOT NULL;
CREATE INDEX embeddings_hnsw ON mail.embeddings USING hnsw (embedding {{embedding_ops}});

-- 2. Structured data from the HTML: a JSON array of schema.org objects (`@type` and properties),
-- size-capped by the daemon. '[]' when there is none.
ALTER TABLE mail.bodies ADD COLUMN structured jsonb NOT NULL DEFAULT '[]';

-- 3. Jobs. A claimed job has locked_until in the future (its lease); a failed one too (its backoff).
-- Workers claim the newest first, so new mail is searchable at once while a backfill drains.
ALTER TABLE mail.jobs ADD COLUMN parked_at timestamptz;
DROP INDEX mail.jobs_ready;
CREATE INDEX jobs_ready ON mail.jobs (kind, enqueued_at) WHERE parked_at IS NULL;

CREATE OR REPLACE FUNCTION mail.queue_body_jobs() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, mail AS $fn$
BEGIN
    INSERT INTO mail.jobs (kind, account, message_id)
    VALUES ('embed', NEW.account, NEW.message_id), ('parse', NEW.account, NEW.message_id)
    ON CONFLICT (kind, account, message_id)
    DO UPDATE SET enqueued_at = now(), attempts = 0, last_error = NULL, locked_until = NULL, parked_at = NULL;
    RETURN NULL;
END
$fn$;

-- What the workers did, per kind and hour (for `lbmail pg status`: done, parked, throughput).
CREATE TABLE mail.job_stats (
    kind      text NOT NULL CHECK (kind IN ('embed', 'parse')),
    hour      timestamptz NOT NULL,
    -- Jobs finished (something written, or nothing to write).
    done      bigint NOT NULL DEFAULT 0,
    -- Of those: finished without an endpoint call because the content had not changed.
    unchanged bigint NOT NULL DEFAULT 0,
    -- Of those: refused by a privacy rule (sensitive, AI off, local-only account and a hosted endpoint).
    skipped   bigint NOT NULL DEFAULT 0,
    -- Attempts that failed and were retried later, and jobs parked after too many.
    failed    bigint NOT NULL DEFAULT 0,
    parked    bigint NOT NULL DEFAULT 0,
    -- Requests to the embedding / chat endpoint, and rows written (embedding chunks, facts).
    calls     bigint NOT NULL DEFAULT 0,
    items     bigint NOT NULL DEFAULT 0,
    busy_ms   bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (kind, hour)
);

-- 4. Search, with the query vector as {{embedding_type}}. Same fusion as version 1; the vector
-- candidates are filtered by account before they are cut, so a one-account search still gets its
-- nearest messages.
DROP FUNCTION mail.search(text, text[], integer, timestamptz, timestamptz, vector, text);
CREATE FUNCTION mail.search(
    q text,
    account_ids text[] DEFAULT NULL,
    max_rows integer DEFAULT 50,
    since timestamptz DEFAULT NULL,
    until timestamptz DEFAULT NULL,
    query_embedding {{embedding_type}} DEFAULT NULL,
    embedding_model text DEFAULT NULL
) RETURNS TABLE (
    account text,
    message_id text,
    thread_id text,
    sent_at timestamptz,
    subject text,
    from_email text,
    snippet text,
    sensitive_kind text,
    score double precision,
    matched text[]
)
LANGUAGE sql STABLE
SET search_path = pg_catalog, mail, {{vector_schema}}, {{trgm_schema}}
SET pg_trgm.word_similarity_threshold = 0.5
AS $fn$
WITH p AS (
    SELECT websearch_to_tsquery('english'::regconfig, coalesce(q, '')) AS tq,
           least(greatest(coalesce(max_rows, 50), 1), 500) AS n
),
hits AS (
    SELECT m.account, m.id FROM mail.messages m, p WHERE m.fts @@ p.tq
    UNION
    SELECT b.account, b.message_id FROM mail.bodies b, p WHERE b.fts @@ p.tq
),
fts AS (
    SELECT x.account, x.id, row_number() OVER (ORDER BY x.rank DESC, x.sent_at DESC NULLS LAST) AS r
    FROM (
        SELECT m.account, m.id, m.sent_at,
               ts_rank_cd(m.fts, p.tq) * 2 + coalesce(ts_rank_cd(b.fts, p.tq), 0) AS rank
        FROM hits h
        JOIN mail.messages m ON m.account = h.account AND m.id = h.id
        LEFT JOIN mail.bodies b ON b.account = m.account AND b.message_id = m.id
        CROSS JOIN p
        WHERE (account_ids IS NULL OR m.account = ANY (account_ids))
          AND (since IS NULL OR m.sent_at >= since)
          AND (until IS NULL OR m.sent_at < until)
        ORDER BY rank DESC, m.sent_at DESC NULLS LAST
        LIMIT least(greatest(coalesce(max_rows, 50), 1), 500) * 4
    ) x
),
fuzzy AS (
    SELECT x.account, x.id, row_number() OVER (ORDER BY x.sim DESC, x.sent_at DESC NULLS LAST) AS r
    FROM (
        SELECT m.account, m.id, m.sent_at,
               greatest(word_similarity(q, m.subject), word_similarity(q, m.participants)) AS sim
        FROM mail.messages m
        WHERE length(trim(coalesce(q, ''))) >= 3
          AND (q <% m.subject OR q <% m.participants)
          AND (account_ids IS NULL OR m.account = ANY (account_ids))
          AND (since IS NULL OR m.sent_at >= since)
          AND (until IS NULL OR m.sent_at < until)
        ORDER BY sim DESC, m.sent_at DESC NULLS LAST
        LIMIT least(greatest(coalesce(max_rows, 50), 1), 500) * 4
    ) x
),
near AS (
    SELECT x.account, x.message_id AS id, row_number() OVER (ORDER BY min(x.d)) AS r
    FROM (
        SELECT e.account, e.message_id, e.embedding <=> query_embedding AS d
        FROM mail.embeddings e
        WHERE query_embedding IS NOT NULL
          AND (embedding_model IS NULL OR e.model = embedding_model)
          AND (account_ids IS NULL OR e.account = ANY (account_ids))
        ORDER BY e.embedding <=> query_embedding
        LIMIT least(greatest(coalesce(max_rows, 50), 1), 500) * 8
    ) x
    JOIN mail.messages m ON m.account = x.account AND m.id = x.message_id
    WHERE (since IS NULL OR m.sent_at >= since)
      AND (until IS NULL OR m.sent_at < until)
    GROUP BY x.account, x.message_id
),
fused AS (
    SELECT u.account, u.id, sum(1.0 / (60 + u.r))::double precision AS score, array_agg(u.src ORDER BY u.src) AS matched
    FROM (
        SELECT account, id, r, 'text' AS src FROM fts
        UNION ALL SELECT account, id, r, 'fuzzy' FROM fuzzy
        UNION ALL SELECT account, id, r, 'vector' FROM near
    ) u
    GROUP BY u.account, u.id
)
SELECT m.account, m.id, m.thread_id, m.sent_at, m.subject, m.from_email, m.snippet, m.sensitive_kind, f.score, f.matched
FROM fused f
JOIN mail.messages m ON m.account = f.account AND m.id = f.id
ORDER BY f.score DESC, m.sent_at DESC NULLS LAST
LIMIT least(greatest(coalesce(max_rows, 50), 1), 500)
$fn$;

-- 5. Parsed facts. A message may have a fact of one kind from more than one parser (rules and a
-- model): mail.facts keeps the most confident. Values are validated before they are written
-- (amounts are JSON numbers, dates `YYYY-MM-DD`, times `YYYY-MM-DDTHH:MM[:SS][offset]`); the views
-- still cast defensively so a bad row cannot break a query.
CREATE INDEX parsed_message ON mail.parsed (account, message_id);

CREATE FUNCTION mail.fact_num(v jsonb) RETURNS numeric
LANGUAGE sql IMMUTABLE SET search_path = pg_catalog AS $fn$
    SELECT CASE WHEN jsonb_typeof(v) = 'number' THEN v::text::numeric END
$fn$;

CREATE FUNCTION mail.fact_date(v jsonb) RETURNS date
LANGUAGE plpgsql IMMUTABLE SET search_path = pg_catalog AS $fn$
BEGIN
    IF jsonb_typeof(v) = 'string' AND (v #>> '{}') ~ '^\d{4}-\d{2}-\d{2}' THEN
        RETURN left(v #>> '{}', 10)::date;
    END IF;
    RETURN NULL;
EXCEPTION WHEN others THEN
    RETURN NULL;
END
$fn$;

CREATE VIEW mail.facts AS
SELECT DISTINCT ON (p.account, p.message_id, p.kind)
    p.account, p.message_id, m.thread_id, m.sent_at, m.subject, m.from_email,
    p.kind, p.data, p.parser, p.parser_version, p.confidence, p.updated_at
FROM mail.parsed p
JOIN mail.messages m ON m.account = p.account AND m.id = p.message_id
ORDER BY p.account, p.message_id, p.kind, p.confidence DESC NULLS LAST, p.updated_at DESC;

CREATE VIEW mail.receipts AS
SELECT account, message_id, thread_id, sent_at, subject, from_email,
       data ->> 'doc' AS doc, data ->> 'merchant' AS merchant, mail.fact_num(data -> 'total') AS total,
       data ->> 'currency' AS currency, mail.fact_date(data -> 'date') AS date, data ->> 'order_id' AS order_id,
       parser, confidence
FROM mail.facts WHERE kind = 'receipt';

CREATE VIEW mail.trips AS
SELECT account, message_id, thread_id, sent_at, subject, from_email,
       data ->> 'mode' AS mode, data ->> 'carrier' AS carrier, data ->> 'number' AS number,
       data ->> 'from' AS origin, data ->> 'to' AS destination,
       data ->> 'depart' AS depart, data ->> 'arrive' AS arrive, mail.fact_date(data -> 'depart') AS depart_date,
       data ->> 'confirmation' AS confirmation, parser, confidence
FROM mail.facts WHERE kind = 'travel';

CREATE VIEW mail.packages AS
SELECT account, message_id, thread_id, sent_at, subject, from_email,
       data ->> 'carrier' AS carrier, data ->> 'tracking_id' AS tracking_id, data ->> 'status' AS status,
       mail.fact_date(data -> 'expected') AS expected, parser, confidence
FROM mail.facts WHERE kind = 'package';

CREATE VIEW mail.subscriptions AS
SELECT account, message_id, thread_id, sent_at, subject, from_email,
       data ->> 'service' AS service, data ->> 'event' AS event, mail.fact_num(data -> 'amount') AS amount,
       data ->> 'currency' AS currency, data ->> 'period' AS period, mail.fact_date(data -> 'renews_on') AS renews_on,
       parser, confidence
FROM mail.facts WHERE kind = 'subscription';

CREATE VIEW mail.bills AS
SELECT account, message_id, thread_id, sent_at, subject, from_email,
       data ->> 'payee' AS payee, mail.fact_num(data -> 'amount') AS amount, data ->> 'currency' AS currency,
       mail.fact_date(data -> 'due_date') AS due_date, parser, confidence
FROM mail.facts WHERE kind = 'bill';

CREATE VIEW mail.recruiting AS
SELECT account, message_id, thread_id, sent_at, subject, from_email,
       data ->> 'company' AS company, data ->> 'role' AS role, data ->> 'recruiter' AS recruiter,
       data ->> 'stage' AS stage, data -> 'asks' AS asks, parser, confidence
FROM mail.facts WHERE kind = 'recruiting';

CREATE VIEW mail.events AS
SELECT account, message_id, thread_id, sent_at, subject, from_email,
       data ->> 'what' AS what, data ->> 'when' AS starts, data ->> 'end' AS ends, mail.fact_date(data -> 'when') AS day,
       data ->> 'where' AS place, data ->> 'source' AS source, parser, confidence
FROM mail.facts WHERE kind = 'event';

CREATE VIEW mail.newsletters AS
SELECT account, message_id, thread_id, sent_at, subject, from_email,
       data ->> 'list_id' AS list_id, data ->> 'unsubscribe_url' AS unsubscribe_url,
       data ->> 'unsubscribe_mailto' AS unsubscribe_mailto, coalesce(data -> 'one_click' = 'true'::jsonb, false) AS one_click,
       parser, confidence
FROM mail.facts WHERE kind = 'newsletter';
