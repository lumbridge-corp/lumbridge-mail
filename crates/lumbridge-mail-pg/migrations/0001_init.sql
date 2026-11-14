-- Lumbridge Mail mirror, schema version 1.
--
-- Runs as mail_writer (the runner does SET LOCAL ROLE), inside the runner's transaction, with
-- search_path = mail, the vector extension's schema, pg_trgm's schema, pg_catalog. Placeholders filled
-- in by the runner: {{embedding_dim}}, {{vector_schema}}, {{trgm_schema}}. Never edit a shipped
-- migration: add a new file.
--
-- Keys are the daemon's own ids: an account id (`personal`) and the provider's message / thread /
-- mailbox ids, unique within an account. Times are timestamptz (UTC); a message without a date has
-- sent_at NULL. Removal on the server is a hard delete here (the row and everything hanging off it go).

-- Accounts. Anon accounts are never mirrored: the daemon does not send them, and this refuses them.
CREATE TABLE mail.accounts (
    id          text PRIMARY KEY,
    persona     text NOT NULL CHECK (persona <> 'anon'),
    label       text NOT NULL,
    email       text NOT NULL,
    name        text NOT NULL,
    provider    text NOT NULL,
    color       text,
    ai_policy   text NOT NULL,
    readonly    boolean NOT NULL DEFAULT false,
    -- Removed from the daemon's config, mail kept until purged.
    kept        boolean NOT NULL DEFAULT false,
    mirrored_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE mail.mailboxes (
    account     text NOT NULL REFERENCES mail.accounts(id) ON DELETE CASCADE,
    id          text NOT NULL,
    name        text NOT NULL,
    -- inbox | sent | drafts | archive | trash | spam | starred | important | all | custom
    role        text NOT NULL,
    mirrored_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account, id)
);

-- Message metadata. For sensitive mail (sensitive_kind set: codes, resets, bank, government) the
-- subject is the fixed one line ("A security code from Acme"), the snippet is empty, and no body,
-- attachment, embedding or parsed fact may exist (enforced by triggers below).
CREATE TABLE mail.messages (
    account         text NOT NULL REFERENCES mail.accounts(id) ON DELETE CASCADE,
    id              text NOT NULL,
    thread_id       text NOT NULL,
    -- Folders / labels it is in (mail.mailboxes ids).
    mailbox_ids     text[] NOT NULL DEFAULT '{}',
    subject         text NOT NULL DEFAULT '',
    snippet         text NOT NULL DEFAULT '',
    -- [{"name": "Ada", "email": "ada@example.com"}, ...]
    from_addrs      jsonb NOT NULL DEFAULT '[]',
    to_addrs        jsonb NOT NULL DEFAULT '[]',
    cc_addrs        jsonb NOT NULL DEFAULT '[]',
    bcc_addrs       jsonb NOT NULL DEFAULT '[]',
    from_email      text NOT NULL DEFAULT '',
    from_name       text,
    -- "Ada Lovelace ada@example.com, ..." of from, to and cc: for search.
    participants    text NOT NULL DEFAULT '',
    sent_at         timestamptz,
    seen            boolean NOT NULL DEFAULT false,
    flagged         boolean NOT NULL DEFAULT false,
    answered        boolean NOT NULL DEFAULT false,
    draft           boolean NOT NULL DEFAULT false,
    has_attachments boolean NOT NULL DEFAULT false,
    size_bytes      bigint NOT NULL DEFAULT 0,
    rfc_message_id  text,
    in_reply_to     text,
    refs            text[] NOT NULL DEFAULT '{}',
    sensitive_kind  text,
    mirrored_at     timestamptz NOT NULL DEFAULT now(),
    fts tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english'::regconfig, subject), 'A')
        || setweight(to_tsvector('simple'::regconfig, participants), 'B')
        || setweight(to_tsvector('english'::regconfig, snippet), 'C')
    ) STORED,
    PRIMARY KEY (account, id)
);
CREATE INDEX messages_thread ON mail.messages (account, thread_id, sent_at);
CREATE INDEX messages_sent ON mail.messages (sent_at DESC);
CREATE INDEX messages_from ON mail.messages (from_email);
CREATE INDEX messages_rfc ON mail.messages (rfc_message_id) WHERE rfc_message_id IS NOT NULL;
CREATE INDEX messages_mailboxes ON mail.messages USING gin (mailbox_ids);
CREATE INDEX messages_unread ON mail.messages (account, sent_at DESC) WHERE NOT seen;
CREATE INDEX messages_fts ON mail.messages USING gin (fts);
CREATE INDEX messages_subject_trgm ON mail.messages USING gin (subject gin_trgm_ops);
CREATE INDEX messages_participants_trgm ON mail.messages USING gin (participants gin_trgm_ops);

-- Bodies, never raw received HTML: `body_text` is the clean plain text of the new content and signature
-- (links kept as `text <url>`, tracking removed), `quoted_text` the quoted history, `blocks` the
-- sanitised block tree from lumbridge-mail-render (remote images as placeholders).
CREATE TABLE mail.bodies (
    account      text NOT NULL,
    message_id   text NOT NULL,
    body_text    text NOT NULL DEFAULT '',
    quoted_text  text NOT NULL DEFAULT '',
    blocks       jsonb NOT NULL DEFAULT '{}',
    -- Selected headers: List-Id, List-Unsubscribe, Reply-To, Authentication-Results, ...
    headers      jsonb NOT NULL DEFAULT '[]',
    -- sha256 of what is above: unchanged content is not rewritten, and embeddings know when they are stale.
    content_hash text NOT NULL,
    mirrored_at  timestamptz NOT NULL DEFAULT now(),
    fts tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english'::regconfig, left(body_text, 200000)), 'B')
        || setweight(to_tsvector('english'::regconfig, left(quoted_text, 100000)), 'D')
    ) STORED,
    PRIMARY KEY (account, message_id),
    FOREIGN KEY (account, message_id) REFERENCES mail.messages (account, id) ON DELETE CASCADE
);
CREATE INDEX bodies_fts ON mail.bodies USING gin (fts);

-- Attachments: metadata only, never the bytes.
CREATE TABLE mail.attachments (
    account     text NOT NULL,
    message_id  text NOT NULL,
    id          text NOT NULL,
    filename    text NOT NULL,
    mime        text NOT NULL,
    size_bytes  bigint NOT NULL DEFAULT 0,
    content_id  text,
    PRIMARY KEY (account, message_id, id),
    FOREIGN KEY (account, message_id) REFERENCES mail.messages (account, id) ON DELETE CASCADE
);
CREATE INDEX attachments_mime ON mail.attachments (mime);
CREATE INDEX attachments_filename_trgm ON mail.attachments USING gin (filename gin_trgm_ops);

-- AI triage, one per thread. For sensitive threads the summary is the fixed one line.
CREATE TABLE mail.triage (
    account     text NOT NULL REFERENCES mail.accounts(id) ON DELETE CASCADE,
    thread_id   text NOT NULL,
    category    text NOT NULL,
    priority    smallint NOT NULL CHECK (priority BETWEEN 0 AND 100),
    summary     text NOT NULL,
    needs_reply boolean NOT NULL DEFAULT false,
    due_at      timestamptz,
    -- Which model or rule produced it (`rules`, `local/my-model`).
    by          text NOT NULL,
    at          timestamptz NOT NULL,
    suspicious  boolean NOT NULL DEFAULT false,
    mirrored_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account, thread_id)
);
CREATE INDEX triage_category ON mail.triage (category, priority DESC);
CREATE INDEX triage_needs_reply ON mail.triage (account, due_at) WHERE needs_reply;

-- Action items and dates found in a thread (`ai.extract`). Not filled by the mirror yet: the daemon
-- does not keep them yet.
CREATE TABLE mail.extracted (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account     text NOT NULL REFERENCES mail.accounts(id) ON DELETE CASCADE,
    thread_id   text NOT NULL,
    message_id  text,
    kind        text NOT NULL CHECK (kind IN ('action', 'date')),
    text        text NOT NULL,
    -- action: me | them | unknown
    owner       text CHECK (owner IN ('me', 'them', 'unknown')),
    -- action: due; date: when.
    at          timestamptz,
    all_day     boolean NOT NULL DEFAULT false,
    by          text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX extracted_thread ON mail.extracted (account, thread_id);
CREATE INDEX extracted_at ON mail.extracted (at) WHERE at IS NOT NULL;

-- Structured facts parsed out of one message: receipts, invoices, travel, recruiter mail, ...
-- `kind` names the shape of `data`; one row per (message, kind, parser).
CREATE TABLE mail.parsed (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account        text NOT NULL,
    message_id     text NOT NULL,
    kind           text NOT NULL,
    data           jsonb NOT NULL,
    parser         text NOT NULL,
    parser_version text NOT NULL DEFAULT '',
    confidence     real,
    -- bodies.content_hash it was parsed from.
    content_hash   text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account, message_id, kind, parser),
    FOREIGN KEY (account, message_id) REFERENCES mail.messages (account, id) ON DELETE CASCADE
);
CREATE INDEX parsed_kind ON mail.parsed (kind, created_at DESC);
CREATE INDEX parsed_data ON mail.parsed USING gin (data jsonb_path_ops);

-- Embeddings of a message (chunk 0 = subject + start of the body, more chunks for long mail). The
-- model is data, not schema; the dimension was chosen when this migration ran.
CREATE TABLE mail.embeddings (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account      text NOT NULL,
    message_id   text NOT NULL,
    chunk        integer NOT NULL DEFAULT 0,
    model        text NOT NULL,
    -- What was embedded: a hash of the chunk's text (and bodies.content_hash for staleness).
    content_hash text NOT NULL,
    embedding    vector({{embedding_dim}}) NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account, message_id, model, chunk),
    FOREIGN KEY (account, message_id) REFERENCES mail.messages (account, id) ON DELETE CASCADE
);
CREATE INDEX embeddings_hnsw ON mail.embeddings USING hnsw (embedding vector_cosine_ops);
CREATE INDEX embeddings_model ON mail.embeddings (model);

-- Work for the next pipelines, filled by triggers when a body arrives or changes (never for
-- sensitive mail). Workers claim rows with FOR UPDATE SKIP LOCKED and delete them when done.
CREATE TABLE mail.jobs (
    kind         text NOT NULL CHECK (kind IN ('embed', 'parse')),
    account      text NOT NULL,
    message_id   text NOT NULL,
    enqueued_at  timestamptz NOT NULL DEFAULT now(),
    attempts     integer NOT NULL DEFAULT 0,
    last_error   text,
    locked_until timestamptz,
    PRIMARY KEY (kind, account, message_id),
    FOREIGN KEY (account, message_id) REFERENCES mail.messages (account, id) ON DELETE CASCADE
);
CREATE INDEX jobs_ready ON mail.jobs (kind, enqueued_at);

-- Rule: no content of sensitive mail, whoever writes it.
CREATE FUNCTION mail.refuse_sensitive_content() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, mail AS $fn$
BEGIN
    IF EXISTS (SELECT 1 FROM mail.messages m
               WHERE m.account = NEW.account AND m.id = NEW.message_id AND m.sensitive_kind IS NOT NULL) THEN
        RAISE EXCEPTION 'mail.%: % is sensitive mail; only its metadata is mirrored', TG_TABLE_NAME, NEW.message_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$fn$;
CREATE TRIGGER bodies_not_sensitive BEFORE INSERT OR UPDATE ON mail.bodies
    FOR EACH ROW EXECUTE FUNCTION mail.refuse_sensitive_content();
CREATE TRIGGER attachments_not_sensitive BEFORE INSERT OR UPDATE ON mail.attachments
    FOR EACH ROW EXECUTE FUNCTION mail.refuse_sensitive_content();
CREATE TRIGGER embeddings_not_sensitive BEFORE INSERT OR UPDATE ON mail.embeddings
    FOR EACH ROW EXECUTE FUNCTION mail.refuse_sensitive_content();
CREATE TRIGGER parsed_not_sensitive BEFORE INSERT OR UPDATE ON mail.parsed
    FOR EACH ROW EXECUTE FUNCTION mail.refuse_sensitive_content();
CREATE TRIGGER jobs_not_sensitive BEFORE INSERT OR UPDATE ON mail.jobs
    FOR EACH ROW EXECUTE FUNCTION mail.refuse_sensitive_content();

CREATE FUNCTION mail.refuse_sensitive_thread() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, mail AS $fn$
BEGIN
    IF EXISTS (SELECT 1 FROM mail.messages m
               WHERE m.account = NEW.account AND m.thread_id = NEW.thread_id AND m.sensitive_kind IS NOT NULL) THEN
        RAISE EXCEPTION 'mail.extracted: thread % has sensitive mail', NEW.thread_id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$fn$;
CREATE TRIGGER extracted_not_sensitive BEFORE INSERT OR UPDATE ON mail.extracted
    FOR EACH ROW EXECUTE FUNCTION mail.refuse_sensitive_thread();

-- A message that turns out to be sensitive (seen again with its body) loses its content here too.
CREATE FUNCTION mail.drop_sensitive_content() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, mail AS $fn$
BEGIN
    DELETE FROM mail.bodies WHERE account = NEW.account AND message_id = NEW.id;
    DELETE FROM mail.attachments WHERE account = NEW.account AND message_id = NEW.id;
    DELETE FROM mail.embeddings WHERE account = NEW.account AND message_id = NEW.id;
    DELETE FROM mail.parsed WHERE account = NEW.account AND message_id = NEW.id;
    DELETE FROM mail.jobs WHERE account = NEW.account AND message_id = NEW.id;
    DELETE FROM mail.extracted WHERE account = NEW.account AND thread_id = NEW.thread_id;
    RETURN NULL;
END
$fn$;
CREATE TRIGGER messages_sensitive AFTER UPDATE OF sensitive_kind ON mail.messages
    FOR EACH ROW WHEN (NEW.sensitive_kind IS NOT NULL AND OLD.sensitive_kind IS NULL)
    EXECUTE FUNCTION mail.drop_sensitive_content();

-- New or changed body: queue embedding and parsing.
CREATE FUNCTION mail.queue_body_jobs() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, mail AS $fn$
BEGIN
    INSERT INTO mail.jobs (kind, account, message_id)
    VALUES ('embed', NEW.account, NEW.message_id), ('parse', NEW.account, NEW.message_id)
    ON CONFLICT (kind, account, message_id)
    DO UPDATE SET enqueued_at = now(), attempts = 0, last_error = NULL, locked_until = NULL;
    RETURN NULL;
END
$fn$;
CREATE TRIGGER bodies_queue_insert AFTER INSERT ON mail.bodies
    FOR EACH ROW EXECUTE FUNCTION mail.queue_body_jobs();
CREATE TRIGGER bodies_queue_update AFTER UPDATE OF content_hash ON mail.bodies
    FOR EACH ROW WHEN (OLD.content_hash IS DISTINCT FROM NEW.content_hash)
    EXECUTE FUNCTION mail.queue_body_jobs();

-- One row per thread, with its triage.
CREATE VIEW mail.threads AS
SELECT
    m.account,
    m.thread_id,
    (array_agg(m.subject ORDER BY m.sent_at NULLS FIRST) FILTER (WHERE m.subject <> ''))[1] AS subject,
    min(m.sent_at) AS first_at,
    max(m.sent_at) AS last_at,
    count(*) AS message_count,
    count(*) FILTER (WHERE NOT m.seen) AS unread_count,
    bool_or(m.flagged) AS flagged,
    bool_or(m.has_attachments) AS has_attachments,
    bool_or(m.sensitive_kind IS NOT NULL) AS sensitive,
    t.category,
    t.priority,
    t.summary,
    t.needs_reply,
    t.due_at,
    t.suspicious
FROM mail.messages m
LEFT JOIN mail.triage t ON t.account = m.account AND t.thread_id = m.thread_id
GROUP BY m.account, m.thread_id, t.account, t.thread_id;

-- Search. Full text (subject, people, snippet, body, quoted history) and fuzzy matching on subject and
-- people, plus nearest embeddings when a query vector is given, fused by reciprocal rank (k = 60).
-- `q` takes web-search syntax: words, "a phrase", -not, or.
CREATE FUNCTION mail.search(
    q text,
    account_ids text[] DEFAULT NULL,
    max_rows integer DEFAULT 50,
    since timestamptz DEFAULT NULL,
    until timestamptz DEFAULT NULL,
    query_embedding vector DEFAULT NULL,
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
-- One typo in a short word is still a match ("quartrly" ~ "Quarterly" is 0.58).
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
        ORDER BY e.embedding <=> query_embedding
        LIMIT least(greatest(coalesce(max_rows, 50), 1), 500) * 8
    ) x
    JOIN mail.messages m ON m.account = x.account AND m.id = x.message_id
    WHERE (account_ids IS NULL OR m.account = ANY (account_ids))
      AND (since IS NULL OR m.sent_at >= since)
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
