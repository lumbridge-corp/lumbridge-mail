//! Schema migrations for the `mail` schema, run with an admin DSN (`lbmail pg migrate`).
//!
//! One transaction, under an advisory lock:
//! 1. **Prelude** (as the admin, idempotent): the roles `mail_writer` (owns the schema; the daemon)
//!    and `mail_reader` (select only; agents and tools), both `LOGIN` without a password (set it out
//!    of band, see the crate README); the `vector` and `pg_trgm` extensions; schema `mail` owned by
//!    `mail_writer`, closed to `PUBLIC`.
//! 2. **Migrations** (as `mail_writer`): every file in `migrations/` newer than
//!    `mail.schema_version`, in order, each recorded with its checksum and parameters.
//! 3. **Grants** (as `mail_writer`, idempotent): `mail_reader` may select every table and run the
//!    functions; nothing at all for `PUBLIC` or Supabase's API roles (`anon`, `authenticated`,
//!    `service_role`, `authenticator`), now or for tables added later. The schema is not exposed
//!    through PostgREST.
//!
//! [`Plan::render`] prints all of it without changing anything (`--dry-run`).

use crate::pg_err;
use lumbridge_mail_core::{Error, Result};
use sha2::{Digest, Sha256};
use tokio_postgres::Client;

/// A migration file: version, name, SQL with `{{...}}` placeholders.
struct Migration {
    version: i32,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_init",
        sql: include_str!("../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "0002_embed_parse",
        sql: include_str!("../migrations/0002_embed_parse.sql"),
    },
];

/// The newest schema version this build knows.
pub const LATEST: i32 = 2;

/// Roles the migrations create.
pub const WRITER_ROLE: &str = "mail_writer";
pub const READER_ROLE: &str = "mail_reader";
/// Supabase's API roles (PostgREST, GraphQL): nothing in `mail` is ever granted to them.
pub const API_ROLES: &[&str] = &["anon", "authenticated", "service_role", "authenticator"];

/// The column type of `mail.embeddings.embedding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmbeddingType {
    /// Half-precision floats: half the space, HNSW up to 4,000 dimensions. The default.
    #[default]
    Halfvec,
    /// Single-precision floats: HNSW up to 2,000 dimensions.
    Vector,
}

impl EmbeddingType {
    pub fn as_str(self) -> &'static str {
        match self {
            EmbeddingType::Halfvec => "halfvec",
            EmbeddingType::Vector => "vector",
        }
    }

    /// The HNSW operator class for cosine distance.
    pub fn cosine_ops(self) -> &'static str {
        match self {
            EmbeddingType::Halfvec => "halfvec_cosine_ops",
            EmbeddingType::Vector => "vector_cosine_ops",
        }
    }

    /// The most dimensions pgvector's HNSW index takes for this type.
    pub fn max_indexed_dim(self) -> u32 {
        match self {
            EmbeddingType::Halfvec => 4000,
            EmbeddingType::Vector => 2000,
        }
    }

    pub fn parse(s: &str) -> Option<EmbeddingType> {
        match s {
            "halfvec" => Some(EmbeddingType::Halfvec),
            "vector" => Some(EmbeddingType::Vector),
            _ => None,
        }
    }
}

/// Migration parameters.
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// Dimension of `mail.embeddings.embedding`, set by version 2 (version 1 made a `vector` column
    /// that version 2 replaces). Changing it later takes a new migration.
    pub embedding_dim: u32,
    /// Its type: `halfvec` (default) or `vector`.
    pub embedding_type: EmbeddingType,
    /// Stop after this version (tests, and staged upgrades); `None` = the newest.
    pub up_to: Option<i32>,
}

/// What our embedding model returns (2,048 dimensions); `halfvec` because HNSW on `vector` stops at
/// 2,000.
pub const DEFAULT_EMBEDDING_DIM: u32 = 2048;

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            embedding_dim: DEFAULT_EMBEDDING_DIM,
            embedding_type: EmbeddingType::Halfvec,
            up_to: None,
        }
    }
}

impl MigrateOptions {
    /// `dim` dimensions as `halfvec`.
    pub fn dim(dim: u32) -> Self {
        Self {
            embedding_dim: dim,
            ..Self::default()
        }
    }

    /// The dimension and type must make an indexable column.
    pub fn check(&self) -> Result<()> {
        let max = self.embedding_type.max_indexed_dim();
        if self.embedding_dim == 0 || self.embedding_dim > max {
            return Err(Error::Invalid(format!(
                "embedding dimension {} as {} (1..={max}: pgvector's HNSW index takes no more{})",
                self.embedding_dim,
                self.embedding_type.as_str(),
                if self.embedding_type == EmbeddingType::Vector {
                    "; use halfvec for up to 4000"
                } else {
                    ""
                }
            )));
        }
        Ok(())
    }
}

/// What a migration run would do (and, after [`migrate`], did).
#[derive(Debug, Clone)]
pub struct Plan {
    /// Where we connected (user@host/db), or "not connected" for an offline plan.
    pub target: String,
    /// The applied version (0 = no `mail` schema yet).
    pub current: i32,
    pub vector_schema: String,
    pub trgm_schema: String,
    /// Whether the extensions are installed already.
    pub vector_installed: bool,
    pub trgm_installed: bool,
    pub prelude: String,
    /// (version, name, rendered SQL) still to apply.
    pub pending: Vec<(i32, String, String)>,
    pub grants: String,
    /// Applied migrations whose file changed since (never edit a shipped migration).
    pub warnings: Vec<String>,
    pub options: MigrateOptions,
}

/// What [`migrate`] did.
#[derive(Debug, Clone)]
pub struct Report {
    pub from: i32,
    pub to: i32,
    pub applied: Vec<String>,
    pub warnings: Vec<String>,
}

fn checksum(sql: &str) -> String {
    let d = Sha256::digest(sql.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// `"name"`, quoted for SQL (schema names come from the catalog, but quote anyway).
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn render(
    version: i32,
    sql: &str,
    plan_vector: &str,
    plan_trgm: &str,
    opts: &MigrateOptions,
) -> String {
    // Version 1 made a `vector(N)` column that version 2 replaces with the real one; on a new
    // database it only has to be creatable (HNSW on `vector` takes at most 2,000 dimensions).
    let dim = if version == 1 {
        opts.embedding_dim.min(EmbeddingType::Vector.max_indexed_dim())
    } else {
        opts.embedding_dim
    };
    sql.replace("{{embedding_dim}}", &dim.to_string())
        .replace("{{embedding_type}}", opts.embedding_type.as_str())
        .replace("{{embedding_ops}}", opts.embedding_type.cosine_ops())
        .replace("{{vector_schema}}", &quote_ident(plan_vector))
        .replace("{{trgm_schema}}", &quote_ident(plan_trgm))
}

fn prelude(vector_schema: &str, trgm_schema: &str) -> String {
    let v = quote_ident(vector_schema);
    let t = quote_ident(trgm_schema);
    let mut usage = vec![v.clone()];
    if t != v {
        usage.push(t.clone());
    }
    let usage: String = usage
        .iter()
        .map(|s| format!("GRANT USAGE ON SCHEMA {s} TO {WRITER_ROLE}, {READER_ROLE};\n"))
        .collect();
    format!(
        r#"-- Roles: LOGIN without a password (set one out of band; see the crate README).
DO $roles$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{WRITER_ROLE}') THEN
        BEGIN
            CREATE ROLE {WRITER_ROLE} LOGIN NOINHERIT;
        EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
        END;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{READER_ROLE}') THEN
        BEGIN
            CREATE ROLE {READER_ROLE} LOGIN NOINHERIT;
        EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
        END;
    END IF;
    -- The admin running this must be able to act as mail_writer to create its objects.
    IF NOT (SELECT rolsuper FROM pg_roles WHERE rolname = current_user)
       AND NOT pg_has_role(current_user, '{WRITER_ROLE}', 'SET') THEN
        EXECUTE format('GRANT {WRITER_ROLE} TO %I WITH INHERIT FALSE, SET TRUE', current_user);
    END IF;
END
$roles$;
-- Agents read; they never change anything.
ALTER ROLE {READER_ROLE} SET default_transaction_read_only = on;

CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA {v};
CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA {t};

CREATE SCHEMA IF NOT EXISTS mail AUTHORIZATION {WRITER_ROLE};
ALTER SCHEMA mail OWNER TO {WRITER_ROLE};
REVOKE ALL ON SCHEMA mail FROM PUBLIC;
{usage}"#
    )
}

fn grants() -> String {
    let api = API_ROLES
        .iter()
        .map(|r| format!("'{r}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"-- As {WRITER_ROLE}, the owner of everything in mail.
GRANT USAGE ON SCHEMA mail TO {READER_ROLE};
GRANT SELECT ON ALL TABLES IN SCHEMA mail TO {READER_ROLE};
REVOKE ALL ON ALL TABLES IN SCHEMA mail FROM PUBLIC;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA mail FROM PUBLIC;
REVOKE ALL ON ALL FUNCTIONS IN SCHEMA mail FROM PUBLIC;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA mail TO {READER_ROLE};
ALTER DEFAULT PRIVILEGES IN SCHEMA mail GRANT SELECT ON TABLES TO {READER_ROLE};
ALTER DEFAULT PRIVILEGES IN SCHEMA mail REVOKE ALL ON FUNCTIONS FROM PUBLIC;
ALTER DEFAULT PRIVILEGES IN SCHEMA mail GRANT EXECUTE ON FUNCTIONS TO {READER_ROLE};
-- Not reachable through PostgREST / GraphQL: Supabase's API roles get nothing, now or later.
DO $api$
DECLARE r text;
BEGIN
    FOREACH r IN ARRAY ARRAY[{api}] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r) THEN
            EXECUTE format('REVOKE ALL ON SCHEMA mail FROM %I', r);
            EXECUTE format('REVOKE ALL ON ALL TABLES IN SCHEMA mail FROM %I', r);
            EXECUTE format('REVOKE ALL ON ALL SEQUENCES IN SCHEMA mail FROM %I', r);
            EXECUTE format('REVOKE ALL ON ALL FUNCTIONS IN SCHEMA mail FROM %I', r);
            EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA mail REVOKE ALL ON TABLES FROM %I', r);
            EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA mail REVOKE ALL ON SEQUENCES FROM %I', r);
            EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA mail REVOKE ALL ON FUNCTIONS FROM %I', r);
        END IF;
    END LOOP;
END
$api$;
"#
    )
}

const SCHEMA_VERSION_TABLE: &str = "CREATE TABLE IF NOT EXISTS mail.schema_version (
    version    integer PRIMARY KEY,
    name       text NOT NULL,
    checksum   text NOT NULL,
    params     jsonb NOT NULL DEFAULT '{}',
    applied_at timestamptz NOT NULL DEFAULT now(),
    applied_by text NOT NULL DEFAULT session_user
)";

/// The plan for a database we cannot look at (no DSN): as if it were empty, with the extensions in
/// `extensions` (Supabase's convention; `public` elsewhere).
pub fn plan_offline(opts: &MigrateOptions) -> Plan {
    build_plan(
        "not connected (assuming an empty database)".into(),
        0,
        "extensions",
        "extensions",
        false,
        false,
        &[],
        opts,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_plan(
    target: String,
    current: i32,
    vector_schema: &str,
    trgm_schema: &str,
    vector_installed: bool,
    trgm_installed: bool,
    applied: &[(i32, String)],
    opts: &MigrateOptions,
) -> Plan {
    let mut warnings = Vec::new();
    for m in MIGRATIONS {
        if let Some((_, sum)) = applied.iter().find(|(v, _)| *v == m.version)
            && *sum != checksum(m.sql)
        {
            warnings.push(format!(
                "migration {} was applied from a different file (checksum changed)",
                m.name
            ));
        }
    }
    if current > LATEST {
        warnings.push(format!(
            "the database is at version {current}, newer than this build ({LATEST})"
        ));
    }
    Plan {
        target,
        current,
        vector_schema: vector_schema.into(),
        trgm_schema: trgm_schema.into(),
        vector_installed,
        trgm_installed,
        prelude: prelude(vector_schema, trgm_schema),
        pending: MIGRATIONS
            .iter()
            .filter(|m| m.version > current && opts.up_to.is_none_or(|u| m.version <= u))
            .map(|m| {
                (
                    m.version,
                    m.name.to_owned(),
                    render(m.version, m.sql, vector_schema, trgm_schema, opts),
                )
            })
            .collect(),
        grants: grants(),
        warnings,
        options: opts.clone(),
    }
}

/// Reads what is there, changing nothing (inside a read-only transaction).
pub async fn plan(client: &mut Client, opts: &MigrateOptions) -> Result<Plan> {
    let tx = client
        .build_transaction()
        .read_only(true)
        .start()
        .await
        .map_err(pg_err)?;
    let plan = read_plan(&tx, opts).await?;
    tx.rollback().await.map_err(pg_err)?;
    Ok(plan)
}

async fn read_plan(tx: &tokio_postgres::Transaction<'_>, opts: &MigrateOptions) -> Result<Plan> {
    let row = tx
        .query_one("SELECT current_user::text, coalesce(host(inet_server_addr()), 'local socket'), current_database()::text", &[])
        .await
        .map_err(pg_err)?;
    let target = format!(
        "{}@{}/{}",
        row.get::<_, String>(0),
        row.get::<_, String>(1),
        row.get::<_, String>(2)
    );
    let ext_schema = |name: &'static str| async move {
        tx.query_opt(
            "SELECT n.nspname::text FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = $1",
            &[&name],
        )
        .await
        .map(|r| r.map(|r| r.get::<_, String>(0)))
        .map_err(pg_err)
    };
    let default_schema: String = tx
        .query_one("SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'extensions') THEN 'extensions' ELSE 'public' END", &[])
        .await
        .map_err(pg_err)?
        .get(0);
    let vector = ext_schema("vector").await?;
    let trgm = ext_schema("pg_trgm").await?;
    let vector_schema = vector.clone().unwrap_or_else(|| default_schema.clone());
    let trgm_schema = trgm.clone().unwrap_or_else(|| default_schema.clone());
    for s in [&vector_schema, &trgm_schema] {
        if !ident_ok(s) {
            return Err(Error::Invalid(format!(
                "extension schema {s:?}: expected a plain lower-case name"
            )));
        }
    }
    let has_table: bool = tx
        .query_one("SELECT to_regclass('mail.schema_version') IS NOT NULL", &[])
        .await
        .map_err(pg_err)?
        .get(0);
    let applied: Vec<(i32, String)> = if has_table {
        tx.query(
            "SELECT version, checksum FROM mail.schema_version ORDER BY version",
            &[],
        )
        .await
        .map_err(pg_err)?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
    } else {
        Vec::new()
    };
    let current = applied.iter().map(|(v, _)| *v).max().unwrap_or(0);
    Ok(build_plan(
        target,
        current,
        &vector_schema,
        &trgm_schema,
        vector.is_some(),
        trgm.is_some(),
        &applied,
        opts,
    ))
}

impl Plan {
    /// The whole plan as text: what is there, then every statement that would run, in order.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "-- Lumbridge Mail: migrate schema `mail` on {}\n",
            self.target
        ));
        s.push_str(&format!(
            "-- applied version: {}, this build: {LATEST}\n",
            self.current
        ));
        s.push_str(&format!(
            "-- extensions: vector in {} ({}), pg_trgm in {} ({})\n",
            self.vector_schema,
            if self.vector_installed {
                "installed"
            } else {
                "to install"
            },
            self.trgm_schema,
            if self.trgm_installed {
                "installed"
            } else {
                "to install"
            }
        ));
        s.push_str(&format!(
            "-- embeddings: {}({}) (set by 0002_embed_parse; an applied 0002 is not changed)\n",
            self.options.embedding_type.as_str(),
            self.options.embedding_dim
        ));
        for w in &self.warnings {
            s.push_str(&format!("-- WARNING: {w}\n"));
        }
        if self.pending.is_empty() {
            s.push_str(
                "-- no migrations to apply; the prelude and grants are re-checked (idempotent)\n",
            );
        } else {
            let names: Vec<&str> = self.pending.iter().map(|(_, n, _)| n.as_str()).collect();
            s.push_str(&format!("-- to apply: {}\n", names.join(", ")));
        }
        s.push_str("\nBEGIN;\nSELECT pg_advisory_xact_lock(hashtext('lumbridge-mail-migrate'));\n\n-- 1. prelude (as the admin)\n");
        s.push_str(&self.prelude);
        s.push_str(&format!(
            "\n-- 2. migrations (as {WRITER_ROLE})\nSET LOCAL ROLE {WRITER_ROLE};\nSET LOCAL search_path = mail, {}, {}, pg_catalog;\n{SCHEMA_VERSION_TABLE};\n",
            quote_ident(&self.vector_schema),
            quote_ident(&self.trgm_schema)
        ));
        for (v, name, sql) in &self.pending {
            s.push_str(&format!("\n-- ---- {name} (version {v}) ----\n{sql}\n"));
            s.push_str(&format!(
                "INSERT INTO mail.schema_version (version, name, checksum, params) VALUES ({v}, '{name}', '<sha256 of the file>', '{{\"embedding_dim\": {}, \"embedding_type\": \"{}\"}}');\n",
                self.options.embedding_dim,
                self.options.embedding_type.as_str()
            ));
        }
        s.push_str("\n-- 3. grants\n");
        s.push_str(&self.grants);
        s.push_str("COMMIT;\n");
        s
    }
}

/// Applies the plan in one transaction. Safe to run again: it only applies what is missing and
/// re-checks roles, ownership and grants.
pub async fn migrate(client: &mut Client, opts: &MigrateOptions) -> Result<Report> {
    opts.check()?;
    let tx = client.transaction().await.map_err(pg_err)?;
    tx.batch_execute("SELECT pg_advisory_xact_lock(hashtext('lumbridge-mail-migrate'))")
        .await
        .map_err(pg_err)?;
    let plan = read_plan(&tx, opts).await?;
    if plan.current > LATEST {
        return Err(Error::Refused(format!(
            "the mail schema is at version {}, newer than this build ({LATEST}); update Lumbridge Mail",
            plan.current
        )));
    }
    tx.batch_execute(&plan.prelude)
        .await
        .map_err(|e| step("prelude", e))?;
    tx.batch_execute(&format!(
        "SET LOCAL ROLE {WRITER_ROLE}; SET LOCAL search_path = mail, {}, {}, pg_catalog; {SCHEMA_VERSION_TABLE};",
        quote_ident(&plan.vector_schema),
        quote_ident(&plan.trgm_schema)
    ))
    .await
    .map_err(|e| step("schema_version", e))?;
    let mut applied = Vec::new();
    for (version, name, sql) in &plan.pending {
        tx.batch_execute(sql).await.map_err(|e| step(name, e))?;
        let source = MIGRATIONS
            .iter()
            .find(|m| m.version == *version)
            .map(|m| m.sql)
            .unwrap_or_default();
        let dim = if *version == 1 {
            opts.embedding_dim
                .min(EmbeddingType::Vector.max_indexed_dim())
        } else {
            opts.embedding_dim
        };
        let params = serde_json::json!({
            "embedding_dim": dim,
            "embedding_type": if *version == 1 { "vector" } else { opts.embedding_type.as_str() },
            "vector_schema": plan.vector_schema,
            "trgm_schema": plan.trgm_schema,
        });
        tx.execute(
            "INSERT INTO mail.schema_version (version, name, checksum, params) VALUES ($1, $2, $3, $4)",
            &[version, name, &checksum(source), &params],
        )
        .await
        .map_err(|e| step(name, e))?;
        applied.push(name.clone());
    }
    tx.batch_execute(&plan.grants)
        .await
        .map_err(|e| step("grants", e))?;
    tx.commit().await.map_err(pg_err)?;
    let to = plan.pending.last().map(|p| p.0).unwrap_or(plan.current);
    Ok(Report {
        from: plan.current,
        to,
        applied,
        warnings: plan.warnings,
    })
}

fn step(what: &str, e: tokio_postgres::Error) -> Error {
    match crate::pg_err(e) {
        Error::Other(m) => Error::Other(format!("migration step {what}: {m}")),
        other => other,
    }
}

/// The applied schema version (None when there is no `mail` schema), for status and the writer.
pub async fn current_version(client: &Client) -> Result<Option<i32>> {
    let has: bool = client
        .query_one("SELECT to_regclass('mail.schema_version') IS NOT NULL", &[])
        .await
        .map_err(pg_err)?
        .get(0);
    if !has {
        return Ok(None);
    }
    let v: Option<i32> = client
        .query_one("SELECT max(version) FROM mail.schema_version", &[])
        .await
        .map_err(pg_err)?
        .get(0);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offline_plan_shows_every_statement() {
        let p = plan_offline(&MigrateOptions {
            embedding_dim: 768,
            embedding_type: EmbeddingType::Vector,
            up_to: None,
        });
        let text = p.render();
        for want in [
            "CREATE ROLE mail_writer LOGIN",
            "CREATE ROLE mail_reader LOGIN",
            "CREATE SCHEMA IF NOT EXISTS mail AUTHORIZATION mail_writer",
            "CREATE TABLE mail.messages",
            "vector(768)",
            "USING hnsw (embedding vector_cosine_ops)",
            "CREATE FUNCTION mail.search(",
            "REVOKE ALL ON SCHEMA mail FROM %I",
            "'anon', 'authenticated', 'service_role', 'authenticator'",
            "SET LOCAL ROLE mail_writer",
            "COMMIT;",
        ] {
            assert!(text.contains(want), "plan lacks {want:?}");
        }
        assert!(!text.contains("{{"), "every placeholder is filled");
        assert!(
            !text.to_lowercase().contains("password '"),
            "no password is ever set by a migration"
        );
        assert_eq!(p.pending.len(), MIGRATIONS.len());
    }

    #[test]
    fn the_default_plan_makes_a_2048_halfvec_column_and_0001_stays_creatable() {
        let p = plan_offline(&MigrateOptions::default());
        let text = p.render();
        assert_eq!(
            p.pending
                .iter()
                .map(|(_, n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["0001_init", "0002_embed_parse"]
        );
        // 0001's placeholder column is indexable; 0002 replaces it.
        assert!(p.pending[0].2.contains("vector(2000)"), "0001 capped");
        let v2 = &p.pending[1].2;
        for want in [
            "ADD COLUMN embedding halfvec(2048) NOT NULL",
            "USING hnsw (embedding halfvec_cosine_ops)",
            "query_embedding halfvec DEFAULT NULL",
            "CREATE TABLE mail.job_stats",
            "CREATE VIEW mail.receipts",
            "CREATE VIEW mail.recruiting",
            "ADD COLUMN parked_at",
            "ADD COLUMN structured jsonb",
        ] {
            assert!(v2.contains(want), "0002 lacks {want:?}");
        }
        assert!(text.contains("-- embeddings: halfvec(2048)"));
        assert!(text.contains("\"embedding_type\": \"halfvec\""));
        assert!(!text.contains("{{"), "every placeholder is filled");
        let staged = plan_offline(&MigrateOptions {
            up_to: Some(1),
            ..MigrateOptions::default()
        });
        assert_eq!(staged.pending.len(), 1);
    }

    #[test]
    fn dimensions_must_fit_the_index() {
        assert!(MigrateOptions::default().check().is_ok());
        assert!(MigrateOptions::dim(4000).check().is_ok());
        assert!(MigrateOptions::dim(4001).check().is_err());
        assert!(MigrateOptions::dim(0).check().is_err());
        let v = |d| MigrateOptions {
            embedding_dim: d,
            embedding_type: EmbeddingType::Vector,
            up_to: None,
        };
        assert!(v(2000).check().is_ok());
        let e = v(2048).check().unwrap_err().to_string();
        assert!(e.contains("halfvec"), "{e}");
        assert_eq!(EmbeddingType::parse("halfvec"), Some(EmbeddingType::Halfvec));
        assert_eq!(EmbeddingType::parse("bit"), None);
    }

    #[test]
    fn migrations_are_numbered_in_order() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(m.version, i as i32 + 1);
            assert!(m.name.starts_with(&format!("{:04}_", m.version)));
        }
        assert_eq!(MIGRATIONS.last().unwrap().version, LATEST);
    }

    #[test]
    fn extension_schema_names_are_checked() {
        assert!(ident_ok("extensions") && ident_ok("public") && ident_ok("ext_2"));
        assert!(!ident_ok("Ext") && !ident_ok("a;b") && !ident_ok("") && !ident_ok("2x"));
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
