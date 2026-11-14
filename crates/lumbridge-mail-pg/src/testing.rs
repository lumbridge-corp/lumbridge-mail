//! Helpers for tests that need a real Postgres (this crate's and the daemon's). They run only when
//! `LUMBRIDGE_MAIL_PG_TEST_URL` names a throwaway server (superuser DSN); each test gets its own
//! database, dropped at the end. Never point this at a real database: tests create roles and set
//! their passwords.

use crate::connect::{ConnectOptions, connect, parse_dsn};
use tokio_postgres::Client;

pub const TEST_URL_VAR: &str = "LUMBRIDGE_MAIL_PG_TEST_URL";

/// Roles are shared by the whole server: tests that touch them take this first.
pub static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The password the tests give `mail_writer` and `mail_reader` (this process only).
pub fn role_password() -> String {
    use std::hash::{BuildHasher, Hasher};
    static PW: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PW.get_or_init(|| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        format!("t{:016x}{:08x}", h.finish(), std::process::id())
    })
    .clone()
}

fn quote(v: &str) -> String {
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// A database of its own on the test server.
pub struct TestDb {
    pub name: String,
    host: String,
    port: u16,
    user: String,
    password: String,
    admin_db: String,
}

impl TestDb {
    /// None (and a line on stderr) when the test server is not configured.
    pub async fn create(label: &str) -> Option<TestDb> {
        let Ok(url) = std::env::var(TEST_URL_VAR) else {
            eprintln!("skipped: set {TEST_URL_VAR} to a throwaway Postgres to run this test");
            return None;
        };
        let cfg = parse_dsn(&url).expect("test URL");
        let host = match cfg.get_hosts().first() {
            Some(tokio_postgres::config::Host::Tcp(h)) => h.clone(),
            _ => "127.0.0.1".into(),
        };
        let port = cfg.get_ports().first().copied().unwrap_or(5432);
        let user = cfg.get_user().unwrap_or("postgres").to_owned();
        let password = cfg
            .get_password()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_default();
        let admin_db = cfg.get_dbname().unwrap_or("postgres").to_owned();
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let label: String = label
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(20)
            .collect::<String>()
            .to_lowercase();
        let name = format!("lbmail_t_{label}_{}_{n}", std::process::id());
        let db = TestDb {
            name,
            host,
            port,
            user,
            password,
            admin_db,
        };
        let admin = connect(
            &db.dsn_for(&db.user, &db.password, &db.admin_db),
            &ConnectOptions::default(),
        )
        .await
        .expect("connect to the test server");
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", db.name))
            .await
            .expect("drop old test db");
        admin
            .batch_execute(&format!("CREATE DATABASE {}", db.name))
            .await
            .expect("create test db");
        Some(db)
    }

    fn dsn_for(&self, user: &str, password: &str, db: &str) -> String {
        format!(
            "host={} port={} user={} password={} dbname={} sslmode=disable",
            quote(&self.host),
            self.port,
            quote(user),
            quote(password),
            quote(db)
        )
    }

    /// A superuser DSN into this database (what `lbmail pg migrate` would use).
    pub fn admin_dsn(&self) -> String {
        self.dsn_for(&self.user, &self.password, &self.name)
    }

    /// A DSN into this database as `role` with [`role_password`] (call [`TestDb::set_role_passwords`]).
    pub fn role_dsn(&self, role: &str) -> String {
        self.dsn_for(role, &role_password(), &self.name)
    }

    /// A DSN to a port where nothing listens (Postgres "down").
    pub fn dead_dsn(&self) -> String {
        format!(
            "host=127.0.0.1 port=9 user=mail_writer password={} dbname={} sslmode=disable connect_timeout=2",
            quote(&role_password()),
            quote(&self.name)
        )
    }

    pub async fn admin(&self) -> Client {
        connect(&self.admin_dsn(), &ConnectOptions::default())
            .await
            .expect("connect as admin")
    }

    /// Gives the mirror's roles [`role_password`] (they exist after a migration).
    pub async fn set_role_passwords(&self) {
        let c = self.admin().await;
        for r in [crate::migrate::WRITER_ROLE, crate::migrate::READER_ROLE] {
            c.batch_execute(&format!(
                "ALTER ROLE {r} PASSWORD {}",
                quote(&role_password())
            ))
            .await
            .expect("set role password");
        }
    }

    pub async fn drop_db(self) {
        if let Ok(c) = connect(
            &self.dsn_for(&self.user, &self.password, &self.admin_db),
            &ConnectOptions::default(),
        )
        .await
        {
            let _ = c
                .batch_execute(&format!(
                    "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                    self.name
                ))
                .await;
        }
    }
}
