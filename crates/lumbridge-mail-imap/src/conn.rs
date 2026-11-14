//! Connections: TCP, TLS (rustls, implicit or STARTTLS), and sign-in (LOGIN or AUTHENTICATE XOAUTH2).

use crate::config::{Credentials, Security, Server};
use lumbridge_mail_core::{Error, Result};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, pki_types::ServerName};

pub enum MailStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl std::fmt::Debug for MailStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MailStream::Plain(_) => "MailStream::Plain",
            MailStream::Tls(_) => "MailStream::Tls",
        })
    }
}

impl AsyncRead for MailStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MailStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

pub type Session = async_imap::Session<MailStream>;

/// Maps an async-imap error onto the workspace error. Connection trouble is `Offline` (retried); a
/// server saying NO or BAD is not.
pub fn imap_err(context: &str, e: async_imap::error::Error) -> Error {
    use async_imap::error::Error as E;
    match e {
        E::Io(e) => Error::Offline(format!("{context}: {e}")),
        E::ConnectionLost => Error::Offline(format!("{context}: connection lost")),
        E::No(m) => Error::Other(format!("{context}: server said NO: {}", trim_outcome(&m))),
        E::Bad(m) => Error::Other(format!("{context}: server said BAD: {}", trim_outcome(&m))),
        E::Parse(e) => Error::Other(format!("{context}: unreadable server response: {e}")),
        E::Validate(e) => Error::Invalid(format!("{context}: {e}")),
        E::Append => Error::Other(format!("{context}: append refused")),
        other => Error::Other(format!("{context}: {other}")),
    }
}

fn trim_outcome(s: &str) -> String {
    s.chars().take(200).collect()
}

pub async fn with_timeout<T>(t: Duration, what: &str, fut: impl Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(t, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::Offline(format!("{what}: timed out"))),
    }
}

/// A rustls client config: webpki roots, or (loopback only, see `Server::skip_verify`) no verification.
pub fn tls_config(skip_verify: bool) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions");
    let config = if skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(LoopbackVerifier(provider)))
            .with_no_client_auth()
    } else {
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Arc::new(config)
}

/// Accepts any certificate. Only ever built for loopback hosts (a local bridge with a self-signed cert).
#[derive(Debug)]
struct LoopbackVerifier(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for LoopbackVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub async fn tls_wrap(tcp: TcpStream, server: &Server) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let name = ServerName::try_from(server.host.trim_start_matches('[').trim_end_matches(']').to_owned())
        .map_err(|_| Error::Invalid(format!("not a valid server name: {}", server.host)))?;
    TlsConnector::from(tls_config(server.skip_verify()))
        .connect(name, tcp)
        .await
        .map_err(|e| match e.kind() {
            io::ErrorKind::InvalidData => Error::Other(format!("TLS with {} failed: {e}", server.host)),
            _ => Error::Offline(format!("TLS with {} failed: {e}", server.host)),
        })
}

pub async fn tcp(server: &Server) -> Result<TcpStream> {
    if server.security == Security::Plain && !server.is_loopback() {
        return Err(Error::Refused(format!("plaintext to {} is not allowed; use TLS or STARTTLS", server.host)));
    }
    let host = server.host.trim_start_matches('[').trim_end_matches(']');
    TcpStream::connect((host, server.port))
        .await
        .map_err(|e| Error::Offline(format!("cannot reach {}:{}: {e}", server.host, server.port)))
}

struct XOAuth2 {
    payload: String,
    sent: bool,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = Vec<u8>;
    fn process(&mut self, _challenge: &[u8]) -> Vec<u8> {
        // The first answer is the token; a second challenge is the server's JSON error, which gets an empty
        // answer so the server can finish with NO.
        if self.sent {
            Vec::new()
        } else {
            self.sent = true;
            self.payload.as_bytes().to_vec()
        }
    }
}

/// Reads the server greeting; `* BYE` or a closed connection means the server will not talk to us now.
macro_rules! greeting {
    ($client:expr, $host:expr) => {{
        use async_imap::imap_proto::{Response, Status};
        match $client.read_response().await.map_err(|e| Error::Offline(e.to_string()))? {
            None => return Err(Error::Offline(format!("{} closed the connection before greeting", $host))),
            Some(resp) => {
                if let Response::Data { status: Status::Bye, .. } = resp.parsed() {
                    return Err(Error::Offline(format!("{} said BYE", $host)));
                }
            }
        }
    }};
}

/// Opens a signed-in session.
pub async fn open(server: &Server, creds: &Credentials, timeout: Duration) -> Result<Session> {
    with_timeout(timeout, "connect", async {
        let tcp = tcp(server).await?;
        let client = match server.security {
            Security::Tls => {
                let tls = tls_wrap(tcp, server).await?;
                let mut c = async_imap::Client::new(MailStream::Tls(Box::new(tls)));
                greeting!(c, server.host);
                c
            }
            Security::Plain => {
                let mut c = async_imap::Client::new(MailStream::Plain(tcp));
                greeting!(c, server.host);
                c
            }
            Security::StartTls => {
                let mut c = async_imap::Client::new(MailStream::Plain(tcp));
                greeting!(c, server.host);
                c.run_command_and_check_ok("STARTTLS", None).await.map_err(|e| match e {
                    async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_) => {
                        Error::Refused(format!("{} does not offer STARTTLS", server.host))
                    }
                    e => imap_err("STARTTLS", e),
                })?;
                let MailStream::Plain(tcp) = c.into_inner() else {
                    return Err(Error::Other("STARTTLS on a TLS stream".into()));
                };
                let tls = tls_wrap(tcp, server).await?;
                async_imap::Client::new(MailStream::Tls(Box::new(tls)))
            }
        };
        let session = match creds {
            Credentials::Password { username, password } => client.login(username, password).await,
            Credentials::OAuth2 { username, token } => {
                let payload = format!("user={username}\x01auth=Bearer {token}\x01\x01");
                client.authenticate("XOAUTH2", XOAuth2 { payload, sent: false }).await
            }
        };
        session.map_err(|(e, _client)| match e {
            async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_) => {
                // Never include the server's text: some servers echo what we sent.
                Error::Auth(format!("{} refused the sign-in for {}", server.host, creds.username()))
            }
            e => imap_err("sign-in", e),
        })
    })
    .await
}
