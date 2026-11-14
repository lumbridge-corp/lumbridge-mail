//! Connecting: the DSN lives in a private file (never in the config, argv or the environment of
//! child processes), TLS goes through rustls.
//!
//! `sslmode=disable` connects in plain text (fine over the tailnet, which is already encrypted).
//! Otherwise (`prefer`, the default, or `require`) the server's certificate is verified against the
//! Mozilla roots plus `ca_file` when given; a self-signed server needs its CA in `ca_file`.

use lumbridge_mail_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, Config, NoTls};

/// How to connect besides the DSN.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// Extra CA certificates (PEM) to trust, for a server with its own CA.
    pub ca_file: Option<PathBuf>,
    /// Name shown in `pg_stat_activity` (default `lumbridge-mail`).
    pub application_name: Option<String>,
}

/// Reads a DSN (`postgres://...` or `host=... user=...`) from `path`, which must be a regular file of
/// this user that nobody else can read (mode 0600 or 0400). The value is trimmed; comment lines
/// (`#`) and blank lines are skipped.
pub fn read_dsn_file(path: &Path) -> Result<String> {
    let meta =
        std::fs::metadata(path).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(Error::Invalid(format!(
            "{}: not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 {
            return Err(Error::Refused(format!(
                "{}: holds a database password but is mode {:o}; make it private: chmod 600 {}",
                path.display(),
                meta.mode() & 0o777,
                path.display()
            )));
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            return Err(Error::Refused(format!(
                "{}: belongs to uid {}, not to this user",
                path.display(),
                meta.uid()
            )));
        }
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    let dsn = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ");
    if dsn.is_empty() {
        return Err(Error::Invalid(format!("{}: empty", path.display())));
    }
    Ok(dsn)
}

/// Parses a DSN (without echoing it: it may hold a password).
pub fn parse_dsn(dsn: &str) -> Result<Config> {
    dsn.parse::<Config>()
        .map_err(|e| Error::Invalid(format!("the Postgres DSN does not parse: {e}")))
}

/// Opens a connection and drives it on the current tokio runtime. The connection closes when the
/// returned client is dropped.
pub async fn connect(dsn: &str, opts: &ConnectOptions) -> Result<Client> {
    let mut config = parse_dsn(dsn)?;
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(Duration::from_secs(10));
    }
    if config.get_application_name().is_none() {
        config.application_name(opts.application_name.as_deref().unwrap_or("lumbridge-mail"));
    }
    config.keepalives(true);
    if config.get_ssl_mode() == SslMode::Disable {
        let (client, conn) = config.connect(NoTls).await.map_err(crate::pg_err)?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        return Ok(client);
    }
    let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_config(opts.ca_file.as_deref())?);
    let (client, conn) = config.connect(tls).await.map_err(crate::pg_err)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(client)
}

fn tls_config(ca_file: Option<&Path>) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(path) = ca_file {
        use rustls_pki_types::pem::PemObject;
        let certs = rustls_pki_types::CertificateDer::pem_file_iter(path)
            .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
        if certs.is_empty() {
            return Err(Error::Invalid(format!(
                "{}: no certificate in it",
                path.display()
            )));
        }
        for c in certs {
            roots
                .add(c)
                .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
        }
    }
    // The provider is named, not taken from the process default: other crates in the workspace build
    // rustls with another provider too, and then there is no single default.
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Other(format!("tls: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsn_files_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lbmail-pg-dsn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pg.dsn");
        std::fs::write(
            &f,
            "# the mirror\npostgres://mail_writer:pw@127.0.0.1:5432/postgres\n",
        )
        .unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = read_dsn_file(&f).unwrap_err().to_string();
        assert!(e.contains("chmod 600") && !e.contains("pw@"), "{e}");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_dsn_file(&f).unwrap(),
            "postgres://mail_writer:pw@127.0.0.1:5432/postgres"
        );
        std::fs::write(&f, "\n# nothing\n").unwrap();
        assert!(read_dsn_file(&f).is_err());
        assert!(read_dsn_file(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_dsns_do_not_echo_the_password() {
        let e = parse_dsn("postgres://u:hunter2@h:notaport/db")
            .unwrap_err()
            .to_string();
        assert!(!e.contains("hunter2"), "{e}");
        assert!(
            parse_dsn("host=127.0.0.1 port=5432 user=mail_writer dbname=postgres sslmode=disable")
                .is_ok()
        );
    }

    #[test]
    fn tls_config_builds_with_and_without_a_ca_file() {
        assert!(tls_config(None).is_ok());
        let dir = std::env::temp_dir().join(format!("lbmail-pg-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("ca.pem");
        std::fs::write(&f, "not a certificate\n").unwrap();
        assert!(tls_config(Some(&f)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
