//! A scripted SMTP submission server for the tests: plain TCP on 127.0.0.1, `AUTH PLAIN`, `LOGIN` and
//! `XOAUTH2`, and switches for the failures the provider must survive. Made-up mail only.

#![allow(dead_code)]

use base64::Engine as _;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Delivered {
    pub from: String,
    pub to: Vec<String>,
    pub data: Vec<u8>,
}

pub struct SmtpState {
    pub user: String,
    pub password: String,
    pub token: String,
    /// What `EHLO` advertises after `AUTH`.
    pub mechanisms: String,
    pub delivered: Vec<Delivered>,
    /// Answer the next `RCPT TO` with this line (e.g. `451 4.3.0 Try later`), once.
    pub fail_rcpt: Option<String>,
    /// Take the next message, then close without answering (the "it went out, we never heard" case).
    pub drop_after_data: bool,
    /// Like Gmail: every delivered message is also filed into this fake IMAP folder.
    pub file_into: Option<(Arc<Mutex<super::fake_imap::FakeState>>, String)>,
    /// Every command line received, with AUTH payloads blanked.
    pub log: Vec<String>,
}

impl Default for SmtpState {
    fn default() -> Self {
        Self {
            user: "jason@example.com".into(),
            password: "hunter2".into(),
            token: "made-up-oauth-token".into(),
            mechanisms: "PLAIN LOGIN XOAUTH2".into(),
            delivered: Vec::new(),
            fail_rcpt: None,
            drop_after_data: false,
            file_into: None,
            log: Vec::new(),
        }
    }
}

pub struct FakeSmtp {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<SmtpState>>,
}

impl FakeSmtp {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, SmtpState> {
        self.state.lock().unwrap()
    }
}

pub fn serve(state: SmtpState) -> FakeSmtp {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(Mutex::new(state));
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let state = shared.clone();
            std::thread::spawn(move || {
                let _ = session(state, stream);
            });
        }
    });
    FakeSmtp { addr, state }
}

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s.trim()).unwrap_or_default()
}

fn session(state: Arc<Mutex<SmtpState>>, stream: TcpStream) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let say = |out: &mut TcpStream, l: &str| -> std::io::Result<()> { out.write_all(format!("{l}\r\n").as_bytes()) };
    say(&mut out, "220 fake.example.com ESMTP ready")?;
    let mut authed = false;
    let mut from = String::new();
    let mut to: Vec<String> = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let text = line.trim_end().to_owned();
        let upper = text.to_ascii_uppercase();
        state.lock().unwrap().log.push(if upper.starts_with("AUTH") { "AUTH ****".into() } else { text.clone() });
        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            let mechs = state.lock().unwrap().mechanisms.clone();
            say(&mut out, "250-fake.example.com")?;
            say(&mut out, &format!("250-AUTH {mechs}"))?;
            say(&mut out, "250 8BITMIME")?;
        } else if upper.starts_with("AUTH PLAIN") {
            let payload = b64(text.get(10..).unwrap_or(""));
            let s = state.lock().unwrap();
            let expected = format!("\0{}\0{}", s.user, s.password);
            drop(s);
            if payload == expected.as_bytes() {
                authed = true;
                say(&mut out, "235 2.7.0 Authentication successful")?;
            } else {
                say(&mut out, "535 5.7.8 Authentication credentials invalid")?;
            }
        } else if upper.starts_with("AUTH LOGIN") {
            say(&mut out, "334 VXNlcm5hbWU6")?;
            let mut u = String::new();
            reader.read_line(&mut u)?;
            say(&mut out, "334 UGFzc3dvcmQ6")?;
            let mut p = String::new();
            reader.read_line(&mut p)?;
            let s = state.lock().unwrap();
            let ok = b64(&u) == s.user.as_bytes() && b64(&p) == s.password.as_bytes();
            drop(s);
            if ok {
                authed = true;
                say(&mut out, "235 2.7.0 Authentication successful")?;
            } else {
                say(&mut out, "535 5.7.8 Authentication credentials invalid")?;
            }
        } else if upper.starts_with("AUTH XOAUTH2") {
            let payload = b64(text.get(12..).unwrap_or(""));
            let s = state.lock().unwrap();
            let expected = format!("user={}\x01auth=Bearer {}\x01\x01", s.user, s.token);
            drop(s);
            if payload == expected.as_bytes() {
                authed = true;
                say(&mut out, "235 2.7.0 Accepted")?;
            } else {
                say(&mut out, "535 5.7.8 Username and Password not accepted")?;
            }
        } else if upper.starts_with("MAIL FROM:") {
            if !authed {
                say(&mut out, "530 5.7.0 Authentication required")?;
                continue;
            }
            from = text[10..].trim().trim_start_matches('<').split('>').next().unwrap_or("").to_owned();
            to.clear();
            say(&mut out, "250 2.1.0 OK")?;
        } else if upper.starts_with("RCPT TO:") {
            let fail = state.lock().unwrap().fail_rcpt.take();
            if let Some(reply) = fail {
                say(&mut out, &reply)?;
                continue;
            }
            to.push(text[8..].trim().trim_start_matches('<').split('>').next().unwrap_or("").to_owned());
            say(&mut out, "250 2.1.5 OK")?;
        } else if upper == "DATA" {
            say(&mut out, "354 Go ahead")?;
            let mut data = Vec::new();
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l)? == 0 {
                    return Ok(());
                }
                if l == ".\r\n" || l == ".\n" {
                    break;
                }
                let l = l.strip_prefix('.').filter(|_| l.starts_with("..")).unwrap_or(&l);
                data.extend_from_slice(l.as_bytes());
            }
            let mut s = state.lock().unwrap();
            if let Some((imap, folder)) = &s.file_into {
                let mut i = imap.lock().unwrap();
                let mut m = super::fake_imap::FakeMessage::new(&["\\Seen"], "10-Nov-2026 10:00:00 +0000", data.clone());
                m.flags = vec!["\\Seen".into()];
                i.folder_mut(folder).add(m);
            }
            s.delivered.push(Delivered { from: from.clone(), to: to.clone(), data });
            if s.drop_after_data {
                s.drop_after_data = false;
                drop(s);
                let _ = out.shutdown(std::net::Shutdown::Both);
                return Ok(());
            }
            drop(s);
            say(&mut out, "250 2.0.0 OK queued")?;
        } else if upper == "RSET" || upper == "NOOP" {
            say(&mut out, "250 2.0.0 OK")?;
        } else if upper == "QUIT" {
            say(&mut out, "221 2.0.0 Bye")?;
            return Ok(());
        } else {
            say(&mut out, "502 5.5.2 Command not recognised")?;
        }
    }
}
