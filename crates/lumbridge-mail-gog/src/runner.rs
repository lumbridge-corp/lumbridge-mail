//! How a gog command line actually runs: a local binary, or `ssh <host>` to the machine that holds the
//! tokens. The provider builds the full argument list (guard flags included) and hands it to a `Runner`;
//! tests use [`crate::fake::FakeRunner`] instead.
//!
//! Some calls need files: an attachment to send (`--attach <path>`), an HTML body (`--body-html-file`),
//! or a download (`gmail attachment ... --out <path>`). Those are expressed as [`Arg::Input`] and
//! [`Arg::Out`], and the runner decides where the files live: a private temp directory locally, or a
//! `mktemp -d` on the remote host, filled from a tar stream on ssh's stdin and sent back as a tar stream
//! on its stdout. Either way the directory is removed when the call ends.

use lumbridge_mail_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// One argument of a gog command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// Passed as is (never interpreted by a shell: the ssh runner quotes it).
    Lit(String),
    /// The path of the staged input file with this name (see [`GogCall::inputs`]).
    Input(String),
    /// The path gog should write its output file to; the bytes come back in [`GogOutput::file`].
    Out,
}

impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Arg::Lit(s.to_owned())
    }
}
impl From<String> for Arg {
    fn from(s: String) -> Self {
        Arg::Lit(s)
    }
}

/// A complete gog invocation, without the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GogCall {
    pub args: Vec<Arg>,
    /// Bytes for gog's stdin (`--body-file -`, `--raw-file -`). None means /dev/null.
    pub stdin: Option<Vec<u8>>,
    /// Files to stage before the call, by name. Names are relative paths of `[A-Za-z0-9._ -]` segments.
    pub inputs: Vec<(String, Vec<u8>)>,
    pub timeout: Duration,
}

impl GogCall {
    pub fn new(args: Vec<Arg>, timeout: Duration) -> Self {
        Self { args, stdin: None, inputs: Vec::new(), timeout }
    }

    /// The dotted command this call is allowed to run (the value after `--enable-commands-exact`).
    pub fn command(&self) -> Option<&str> {
        let mut it = self.args.iter();
        while let Some(a) = it.next() {
            if a == &Arg::Lit("--enable-commands-exact".into())
                && let Some(Arg::Lit(c)) = it.next()
            {
                return Some(c);
            }
        }
        None
    }

    /// The literal arguments, for matching in tests and for logs. Inputs show as `<in:name>`.
    pub fn display_args(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| match a {
                Arg::Lit(s) => s.clone(),
                Arg::Input(n) => format!("<in:{n}>"),
                Arg::Out => "<out>".into(),
            })
            .collect()
    }

    /// True when the call needs a scratch directory (staged inputs or an output file).
    pub fn needs_files(&self) -> bool {
        !self.inputs.is_empty() || self.args.iter().any(|a| matches!(a, Arg::Out | Arg::Input(_)))
    }

    fn wants_out(&self) -> bool {
        self.args.iter().any(|a| matches!(a, Arg::Out))
    }
}

/// What came back. A non-zero `code` is not an `Err`: the provider maps it with [`classify`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GogOutput {
    /// Exit status; -1 when killed by a signal.
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
    /// The file written to [`Arg::Out`], if the call had one and gog wrote it.
    pub file: Option<Vec<u8>>,
}

impl GogOutput {
    pub fn ok_json(v: serde_json::Value) -> Self {
        Self { code: 0, stdout: serde_json::to_vec(&v).unwrap_or_default(), ..Default::default() }
    }
    pub fn fail(code: i32, stderr: &str) -> Self {
        Self { code, stderr: stderr.into(), ..Default::default() }
    }
}

#[async_trait::async_trait]
pub trait Runner: Send + Sync {
    /// Run one gog command. `Err` only for failures of the runner itself (spawn, timeout, transport);
    /// gog's own failures come back as an `Ok` with a non-zero code.
    async fn run(&self, call: GogCall) -> Result<GogOutput>;

    /// How busy this runner's backend is (the budget's numbers), when it keeps them.
    fn load(&self) -> Option<lumbridge_mail_core::ProviderLoad> {
        None
    }
}

/// How hard the processes we start may push their host: `nice` (and `ionice -c3`, idle I/O class,
/// where it exists) in front of gog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Niceness {
    /// `nice -n` value; None = don't renice.
    pub nice: Option<i32>,
    pub ionice_idle: bool,
}

impl Default for Niceness {
    fn default() -> Self {
        Self { nice: Some(15), ionice_idle: true }
    }
}

/// A path given for the remote host: `~/x` becomes `"$HOME"/'x'` (expanded there, not here); anything
/// else is quoted as is. `~user/...` is not supported (see [`check_remote_path`]).
pub fn remote_path(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
        None if p == "~" => "\"$HOME\"".into(),
        None => sh_quote(p),
    }
}

/// Paths for the remote host must be absolute, `~/...`, or a bare command name looked up on the remote
/// `PATH` (binaries only). `~user/...` and relative paths are refused with a clear message.
pub fn check_remote_path(what: &str, p: &str, allow_bare: bool) -> Result<()> {
    let p = p.trim();
    if p.is_empty() {
        return Err(Error::Invalid(format!("{what} is empty")));
    }
    if p.starts_with('~') && !(p == "~" || p.starts_with("~/")) {
        return Err(Error::Invalid(format!("{what} {p:?}: only ~/ (the ssh user's home) is expanded on the remote host, not ~user")));
    }
    if p.starts_with('/') || p.starts_with("~/") {
        return Ok(());
    }
    if allow_bare && !p.contains('/') {
        return Ok(());
    }
    Err(Error::Invalid(format!("{what} {p:?}: give an absolute path or one starting with ~/ (it is used on the remote host)")))
}

// gog's stable exit codes (internal/cmd/exit_codes.go, unchanged from v0.29.0 to 0.48).
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_EMPTY: i32 = 3;
pub const EXIT_AUTH: i32 = 4;
pub const EXIT_NOT_FOUND: i32 = 5;
pub const EXIT_PERMISSION: i32 = 6;
pub const EXIT_RATE_LIMITED: i32 = 7;
pub const EXIT_RETRYABLE: i32 = 8;
pub const EXIT_CONFIG: i32 = 10;

/// Map a failed gog call to the workspace error kinds. `what` names the command for the message.
/// Stderr is trimmed to its last line-ish 300 characters; it is gog's own text, never mail content
/// (gog prints API errors, not message data, on stderr).
pub fn classify(what: &str, out: &GogOutput) -> Error {
    let err = out.stderr.trim();
    let tail: String = {
        let n = err.chars().count();
        err.chars().skip(n.saturating_sub(300)).collect()
    };
    let msg = if tail.is_empty() { format!("{what}: exit {}", out.code) } else { format!("{what}: {tail}") };
    let low = err.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| low.contains(n));
    match out.code {
        EXIT_AUTH | EXIT_CONFIG => Error::Auth(msg),
        EXIT_NOT_FOUND => Error::NotFound(msg),
        EXIT_RATE_LIMITED | EXIT_RETRYABLE => Error::Offline(msg),
        EXIT_PERMISSION => Error::Other(msg),
        EXIT_USAGE if has(&["is not enabled", "blocked by", "no-send", "gmail sending is"]) => Error::Refused(msg),
        EXIT_USAGE => Error::Invalid(msg),
        _ if has(&["invalid_grant", "token has been expired or revoked", "unauthorized_client", "no refresh token",
                    "auth add", "oauth2: cannot fetch token", "keyring"]) => Error::Auth(msg),
        _ if has(&["dial tcp", "no such host", "connection refused", "network is unreachable", "i/o timeout",
                    "tls handshake timeout", "connection reset", "context deadline exceeded", "temporary failure in name resolution",
                    "server misbehaving", "unexpected eof"]) => Error::Offline(msg),
        _ if has(&["not found", "notfound", "404"]) => Error::NotFound(msg),
        _ => Error::Other(msg),
    }
}

/// A staged-file name is safe when every segment is plain (no `..`, no absolute paths, no shell metacharacters).
pub fn safe_input_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        })
}

fn check_inputs(call: &GogCall) -> Result<()> {
    for (name, _) in &call.inputs {
        if !safe_input_name(name) {
            return Err(Error::Invalid(format!("bad staged file name {name:?}")));
        }
    }
    for a in &call.args {
        if let Arg::Input(n) = a
            && !call.inputs.iter().any(|(m, _)| m == n)
        {
            return Err(Error::Invalid(format!("argument refers to a file that is not staged: {n}")));
        }
    }
    Ok(())
}

/// Spawn, feed stdin, collect output, all under one deadline.
async fn run_command(mut cmd: Command, stdin: Option<Vec<u8>>, timeout: Duration, what: &str) -> Result<std::process::Output> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| Error::Other(format!("{what}: cannot start: {e}")))?;
    let feeder = match (stdin, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => Some(tokio::spawn(async move {
            // A child that exits early closes the pipe; that error is reported by the exit status instead.
            let _ = pipe.write_all(&bytes).await;
            let _ = pipe.shutdown().await;
        })),
        _ => None,
    };
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(Error::Other(format!("{what}: {e}"))),
        Err(_) => return Err(Error::Offline(format!("{what}: timed out after {}s", timeout.as_secs()))),
    };
    if let Some(f) = feeder {
        let _ = f.await;
    }
    Ok(out)
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// Runs a gog binary on this machine.
#[derive(Debug, Clone)]
pub struct LocalRunner {
    pub binary: PathBuf,
    /// A shell env file to source first (`set -a; . file`), e.g. the one holding `GOG_KEYRING_PASSWORD`.
    pub env_file: Option<PathBuf>,
    /// Off by default here (tests run `/bin/sh` as "gog"); the daemon turns it on.
    pub niceness: Niceness,
}

fn find_tool(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.is_file())
}

impl LocalRunner {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self { binary: binary.into(), env_file: None, niceness: Niceness { nice: None, ionice_idle: false } }
    }
    pub fn with_env_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.env_file = Some(path.into());
        self
    }
    pub fn with_niceness(mut self, n: Niceness) -> Self {
        self.niceness = n;
        self
    }

    /// `[nice -n N] [ionice -c3]` in front of the binary, for the tools this machine has.
    fn prefix(&self) -> Vec<std::ffi::OsString> {
        let mut p = Vec::new();
        if let (Some(n), Some(nice)) = (self.niceness.nice, find_tool("nice")) {
            p.extend([nice.into_os_string(), "-n".into(), n.to_string().into()]);
        }
        if self.niceness.ionice_idle
            && let Some(io) = find_tool("ionice")
        {
            p.extend([io.into_os_string(), "-c3".into()]);
        }
        p
    }

    fn command(&self, args: &[String]) -> Command {
        let prefix = self.prefix();
        if !prefix.is_empty() {
            let mut c = Command::new("/bin/sh");
            // `sh -c 'set -a; . "$1"; set +a; shift; exec "$@"' sh <env> nice -n 15 gog args`, or without the env file.
            let script = if self.env_file.is_some() { r#"set -a; . "$1" || exit 96; set +a; shift; exec "$@""# } else { r#"exec "$@""# };
            c.arg("-c").arg(script).arg("sh");
            if let Some(env) = &self.env_file {
                c.arg(env);
            }
            c.args(&prefix).arg(&self.binary).args(args);
            return c;
        }
        match &self.env_file {
            // The env file and the arguments are positional parameters, never parsed as shell text.
            Some(env) => {
                let mut c = Command::new("/bin/sh");
                c.arg("-c")
                    .arg(r#"set -a; . "$1" || exit 96; set +a; shift; exec "$@""#)
                    .arg("sh")
                    .arg(env)
                    .arg(&self.binary)
                    .args(args);
                c
            }
            None => {
                let mut c = Command::new(&self.binary);
                c.args(args);
                c
            }
        }
    }
}

#[async_trait::async_trait]
impl Runner for LocalRunner {
    async fn run(&self, call: GogCall) -> Result<GogOutput> {
        check_inputs(&call)?;
        let what = call.command().unwrap_or("gog").to_owned();
        let dir = if call.needs_files() {
            Some(tempfile::Builder::new().prefix("lbmail-gog-").tempdir().map_err(|e| Error::Other(format!("{what}: temp dir: {e}")))?)
        } else {
            None
        };
        let base = dir.as_ref().map(|d| d.path().to_path_buf()).unwrap_or_default();
        for (name, bytes) in &call.inputs {
            let p = base.join("in").join(name);
            if let Some(parent) = p.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| Error::Other(format!("{what}: {e}")))?;
            }
            tokio::fs::write(&p, bytes).await.map_err(|e| Error::Other(format!("{what}: {e}")))?;
        }
        let out_path = base.join("out.bin");
        let args: Vec<String> = call
            .args
            .iter()
            .map(|a| match a {
                Arg::Lit(s) => s.clone(),
                Arg::Input(n) => base.join("in").join(n).to_string_lossy().into_owned(),
                Arg::Out => out_path.to_string_lossy().into_owned(),
            })
            .collect();
        let out = run_command(self.command(&args), call.stdin.clone(), call.timeout, &what).await?;
        let file = if call.wants_out() { tokio::fs::read(&out_path).await.ok() } else { None };
        Ok(GogOutput {
            code: exit_code(&out.status),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            file,
        })
    }
}

/// One shared ssh connection per host (OpenSSH `ControlMaster`), so a gog call is a new session on a
/// live connection instead of a new TCP + key exchange + login each time.
///
/// The master is started on its own (`ssh -f -N -o ControlMaster=yes -o ControlPersist=<s>`, all stdio on
/// /dev/null so it can never hold our pipes open) the first time a call needs it, and calls use it as
/// clients only (`ControlMaster=no`): when the socket is missing or dead ssh connects directly, which
/// is the clean fallback. A dead master's socket is removed so the next call starts a new one. If the
/// master cannot be started (no multiplexing on this ssh, a path too long for a socket), calls go
/// direct for `retry_after` and then try again.
///
/// Sessions per master: the host's budget (`HostLimits::max_in_flight`, 2 by default) is the most
/// sessions ever open at once; keep it below the server's `MaxSessions` (OpenSSH default 10). A
/// "session open refused" reply is treated like a dead master: that call is retried once directly.
#[derive(Debug)]
pub struct Mux {
    /// Private directory (0700) for the control socket.
    pub dir: PathBuf,
    pub persist: Duration,
    pub retry_after: Duration,
    state: tokio::sync::Mutex<MuxState>,
}

#[derive(Debug, Default)]
struct MuxState {
    off_until: Option<tokio::time::Instant>,
    warned: bool,
}

impl Mux {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), persist: Duration::from_secs(300), retry_after: Duration::from_secs(600), state: Default::default() }
    }

    /// The control socket for `host`: short (a socket path must fit in ~100 bytes), stable, private.
    pub fn socket_for(&self, host: &str) -> PathBuf {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in host.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.dir.join(format!("cm-{h:012x}"))
    }
}

/// Runs gog on another machine over ssh (BatchMode, so a missing key fails instead of prompting).
#[derive(Debug, Clone)]
pub struct SshRunner {
    /// An ssh destination, e.g. `gog-host` (anything `ssh` accepts, including config aliases).
    pub host: String,
    /// The gog binary on the remote host (`~/` is expanded there).
    pub binary: String,
    /// A remote env file to source first (`~/` is expanded there).
    pub env_file: Option<String>,
    /// Extra ssh options, e.g. `["-o", "ConnectTimeout=8"]`.
    pub ssh_args: Vec<String>,
    pub ssh: PathBuf,
    /// `nice` / `ionice` on the remote command.
    pub niceness: Niceness,
    /// Share one connection (None: every call connects).
    pub mux: Option<Arc<Mux>>,
}

fn mux_trouble(stderr: &str) -> bool {
    let low = stderr.to_ascii_lowercase();
    ["control socket", "controlsocket", "mux_client", "session open refused", "master is not running", "master refused"]
        .iter()
        .any(|n| low.contains(n))
}

impl SshRunner {
    pub fn new(host: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            binary: binary.into(),
            env_file: None,
            ssh_args: vec![
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=8".into(),
                "-o".into(),
                "ServerAliveInterval=15".into(),
                "-o".into(),
                "ServerAliveCountMax=3".into(),
            ],
            ssh: PathBuf::from("ssh"),
            niceness: Niceness::default(),
            mux: None,
        }
    }
    pub fn with_env_file(mut self, path: impl Into<String>) -> Self {
        self.env_file = Some(path.into());
        self
    }
    pub fn with_mux(mut self, mux: Arc<Mux>) -> Self {
        self.mux = Some(mux);
        self
    }
    pub fn with_niceness(mut self, n: Niceness) -> Self {
        self.niceness = n;
        self
    }

    /// The remote shell text for a call. Every literal is single-quoted; only `$d` (our temp dir),
    /// `$HOME` (for `~/` paths) and the renice prefix expand.
    pub fn remote_script(&self, call: &GogCall) -> String {
        let mut gog = String::new();
        if let Some(env) = &self.env_file {
            gog.push_str(&format!("set -a; . {} || exit 96; set +a; ", remote_path(env)));
        }
        // Our own variable names, set after the env file so it cannot change them.
        if let Some(n) = self.niceness.nice {
            gog.push_str(&format!("lbm_n=; command -v nice >/dev/null 2>&1 && lbm_n='nice -n {n}'; "));
        }
        if self.niceness.ionice_idle {
            gog.push_str("lbm_i=; command -v ionice >/dev/null 2>&1 && lbm_i='ionice -c3'; ");
        }
        gog.push_str("exec ");
        // Unquoted on purpose: each is empty or two words.
        if self.niceness.nice.is_some() {
            gog.push_str("$lbm_n ");
        }
        if self.niceness.ionice_idle {
            gog.push_str("$lbm_i ");
        }
        gog.push_str(&remote_path(&self.binary));
        for a in &call.args {
            gog.push(' ');
            match a {
                Arg::Lit(s) => gog.push_str(&sh_quote(s)),
                Arg::Input(n) => gog.push_str(&format!("\"$d\"/in/{}", sh_quote(n))),
                Arg::Out => gog.push_str("\"$d\"/out/file"),
            }
        }
        if !call.needs_files() {
            return gog;
        }
        let stdin = if call.stdin.is_some() { "\"$d\"/in/.stdin" } else { "/dev/null" };
        // Inputs arrive as a tar on stdin; stdout and the out file go back as a tar on stdout.
        format!(
            "d=$(mktemp -d) || exit 97; trap 'rm -rf \"$d\"' EXIT; mkdir \"$d\"/in \"$d\"/out || exit 97; \
             tar -x -f - -C \"$d\"/in || exit 98; \
             ( {gog} ) <{stdin} >\"$d\"/out/stdout; rc=$?; \
             tar -c -f - -C \"$d\"/out . || exit 99; exit $rc"
        )
    }

    fn bundle_stdin(call: &GogCall) -> Result<Vec<u8>> {
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |name: &str, bytes: &[u8]| -> std::io::Result<()> {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o600);
            h.set_cksum();
            b.append_data(&mut h, name, bytes)
        };
        for (name, bytes) in &call.inputs {
            add(name, bytes).map_err(|e| Error::Other(format!("tar: {e}")))?;
        }
        if let Some(s) = &call.stdin {
            add(".stdin", s).map_err(|e| Error::Other(format!("tar: {e}")))?;
        }
        b.into_inner().map_err(|e| Error::Other(format!("tar: {e}")))
    }

    /// The control socket to use for this call, starting the master if there is none. None = connect
    /// directly (multiplexing off, failed recently, or not possible here).
    async fn control_path(&self) -> Option<PathBuf> {
        let mux = self.mux.as_ref()?;
        let mut st = mux.state.lock().await;
        if st.off_until.is_some_and(|t| tokio::time::Instant::now() < t) {
            return None;
        }
        let path = mux.socket_for(&self.host);
        let off = |st: &mut MuxState, why: String| {
            if !st.warned {
                eprintln!("lumbridge-mail-gog: ssh {}: not sharing a connection ({why}); each call connects", self.host);
                st.warned = true;
            }
            st.off_until = Some(tokio::time::Instant::now() + mux.retry_after);
            None
        };
        if path.as_os_str().len() > 100 {
            return off(&mut st, format!("control path {} is too long for a socket", path.display()));
        }
        if let Err(e) = private_dir(&mux.dir) {
            return off(&mut st, e.to_string());
        }
        if path.exists() {
            return Some(path);
        }
        let mut cmd = Command::new(&self.ssh);
        cmd.args(&self.ssh_args)
            .args(["-o", "ControlMaster=yes", "-o"])
            .arg(format!("ControlPath={}", path.display()))
            .arg("-o")
            .arg(format!("ControlPersist={}", mux.persist.as_secs().max(1)))
            .args(["-f", "-N", "--"])
            .arg(&self.host)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let started = match cmd.spawn() {
            Ok(mut child) => matches!(tokio::time::timeout(Duration::from_secs(20), child.wait()).await, Ok(Ok(s)) if s.success()),
            Err(_) => false,
        };
        if started && path.exists() {
            st.warned = false;
            Some(path)
        } else {
            off(&mut st, "the shared connection did not start".into())
        }
    }

    async fn run_once(&self, call: &GogCall, control: Option<&Path>, what: &str) -> Result<std::process::Output> {
        let bundled = call.needs_files();
        let stdin = if bundled { Some(Self::bundle_stdin(call)?) } else { call.stdin.clone() };
        let mut cmd = Command::new(&self.ssh);
        cmd.args(&self.ssh_args);
        match control {
            Some(p) => {
                cmd.args(["-o", "ControlMaster=no", "-o"]).arg(format!("ControlPath={}", p.display()));
            }
            None if self.mux.is_some() => {
                cmd.args(["-o", "ControlMaster=no", "-o", "ControlPath=none"]);
            }
            None => {}
        }
        cmd.arg("--").arg(&self.host).arg(self.remote_script(call));
        run_command(cmd, stdin, call.timeout, what).await
    }

    /// Close the shared connection (`ssh -O exit`), if one is up. Best effort.
    pub async fn close(&self) {
        let Some(mux) = &self.mux else { return };
        let path = mux.socket_for(&self.host);
        if !path.exists() {
            return;
        }
        let mut cmd = Command::new(&self.ssh);
        cmd.args(["-o"]).arg(format!("ControlPath={}", path.display())).args(["-O", "exit", "--"]).arg(&self.host);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
        if let Ok(mut c) = cmd.spawn() {
            let _ = tokio::time::timeout(Duration::from_secs(5), c.wait()).await;
        }
    }
}

/// Make `dir` a private (0700) directory of ours.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Read the tar the remote script sends back: `./stdout` and maybe `./file`.
pub(crate) fn unbundle_stdout(bytes: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    use std::io::Read;
    let mut stdout = Vec::new();
    let mut file = None;
    let mut ar = tar::Archive::new(bytes);
    let entries = ar.entries().map_err(|e| Error::Other(format!("reply tar: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| Error::Other(format!("reply tar: {e}")))?;
        let path: PathBuf = entry.path().map_err(|e| Error::Other(format!("reply tar: {e}")))?.into_owned();
        let name = path.strip_prefix(".").unwrap_or(&path).to_path_buf();
        let mut buf = Vec::new();
        if name == Path::new("stdout") {
            entry.read_to_end(&mut buf).map_err(|e| Error::Other(format!("reply tar: {e}")))?;
            stdout = buf;
        } else if name == Path::new("file") {
            entry.read_to_end(&mut buf).map_err(|e| Error::Other(format!("reply tar: {e}")))?;
            file = Some(buf);
        }
    }
    Ok((stdout, file))
}

#[async_trait::async_trait]
impl Runner for SshRunner {
    async fn run(&self, call: GogCall) -> Result<GogOutput> {
        check_inputs(&call)?;
        let what = call.command().unwrap_or("gog").to_owned();
        let bundled = call.needs_files();
        let control = self.control_path().await;
        let mut out = self.run_once(&call, control.as_deref(), &what).await?;
        let mut code = exit_code(&out.status);
        let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if let Some(p) = &control
            && mux_trouble(&stderr)
        {
            // The master is gone or refused the session: forget it (the next call starts a new one).
            let _ = std::fs::remove_file(p);
            if code == 255 {
                out = self.run_once(&call, None, &what).await?;
                code = exit_code(&out.status);
                stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            }
        }
        if code == 255 {
            // ssh's own failure: the host is unreachable, or our key was refused.
            let low = stderr.to_ascii_lowercase();
            let msg = format!("{what}: ssh {}: {}", self.host, stderr.trim());
            return Err(if low.contains("permission denied") || low.contains("host key verification failed") {
                Error::Auth(msg)
            } else {
                Error::Offline(msg)
            });
        }
        if bundled {
            if matches!(code, 96..=99) && out.stdout.is_empty() {
                return Err(Error::Other(format!("{what}: remote staging failed (exit {code}): {}", stderr.trim())));
            }
            let (stdout, file) = unbundle_stdout(&out.stdout)?;
            return Ok(GogOutput { code, stdout, stderr, file });
        }
        Ok(GogOutput { code, stdout: out.stdout, stderr, file: None })
    }
}

/// POSIX single-quote a word.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ',' | ':' | '=' | '@')) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: &[&str]) -> GogCall {
        GogCall::new(args.iter().map(|s| Arg::from(*s)).collect(), Duration::from_secs(5))
    }

    #[test]
    fn quoting_survives_hostile_words() {
        assert_eq!(sh_quote("gmail"), "gmail");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's $(rm -rf ~)"), r"'it'\''s $(rm -rf ~)'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn exit_codes_map_to_error_kinds() {
        let k = |code, err: &str| classify("gmail.raw", &GogOutput::fail(code, err));
        assert!(matches!(k(4, "auth required"), Error::Auth(_)));
        assert!(matches!(k(10, "credentials missing"), Error::Auth(_)));
        assert!(matches!(k(5, "Requested entity was not found."), Error::NotFound(_)));
        assert!(matches!(k(7, "rate limit"), Error::Offline(_)));
        assert!(matches!(k(8, "backend error"), Error::Offline(_)));
        assert!(matches!(k(2, "command \"gmail send\" is not enabled"), Error::Refused(_)));
        assert!(matches!(k(2, "Gmail sending is blocked by --gmail-no-send"), Error::Refused(_)));
        assert!(matches!(k(2, "--max must be > 0"), Error::Invalid(_)));
        assert!(matches!(k(1, "Get \"https://gmail.googleapis.com/...\": dial tcp: lookup gmail.googleapis.com: no such host"), Error::Offline(_)));
        assert!(matches!(k(1, "oauth2: \"invalid_grant\" \"Token has been expired or revoked.\""), Error::Auth(_)));
        assert!(matches!(k(1, "something odd"), Error::Other(_)));
        assert!(k(8, "x").is_retryable());
        assert!(!k(4, "x").is_retryable());
    }

    #[test]
    fn plain_remote_script_is_one_exec() {
        let mut r = SshRunner::new("box", "~/.local/bin/gog").with_env_file("~/.config/gog.env");
        r.niceness = Niceness { nice: None, ionice_idle: false };
        let s = r.remote_script(&call(&["--json", "gmail", "search", "from:a b"]));
        // `~/` is expanded by the remote shell ($HOME there), never quoted into a literal "~" path.
        assert_eq!(s, "set -a; . \"$HOME\"/.config/gog.env || exit 96; set +a; exec \"$HOME\"/.local/bin/gog --json gmail search 'from:a b'");
        let r = SshRunner::new("box", "/opt/gog bin/gog");
        let s = r.remote_script(&call(&["--json"]));
        assert_eq!(
            s,
            "lbm_n=; command -v nice >/dev/null 2>&1 && lbm_n='nice -n 15'; lbm_i=; command -v ionice >/dev/null 2>&1 && lbm_i='ionice -c3'; exec $lbm_n $lbm_i '/opt/gog bin/gog' --json"
        );
    }

    #[test]
    fn remote_paths_expand_home_there_and_refuse_other_users() {
        assert_eq!(remote_path("~/x y/gog"), "\"$HOME\"/'x y/gog'");
        assert_eq!(remote_path("/usr/bin/gog"), "/usr/bin/gog");
        assert_eq!(remote_path("it's"), r"'it'\''s'");
        assert!(check_remote_path("binary", "gog", true).is_ok());
        assert!(check_remote_path("binary", "~/.local/bin/gog", true).is_ok());
        assert!(check_remote_path("env_file", "/etc/gog.env", false).is_ok());
        let e = check_remote_path("binary", "~sam/bin/gog", true).unwrap_err();
        assert!(e.to_string().contains("only ~/"), "{e}");
        assert!(check_remote_path("env_file", "gog.env", false).is_err());
        assert!(check_remote_path("binary", "bin/gog", true).is_err());
    }

    #[test]
    fn staged_remote_script_uses_its_temp_dir() {
        let r = SshRunner::new("box", "gog");
        let mut c = call(&["gmail", "send", "--attach"]);
        c.args.push(Arg::Input("a0/report.pdf".into()));
        c.inputs.push(("a0/report.pdf".into(), b"%PDF".to_vec()));
        c.stdin = Some(b"hello".to_vec());
        let s = r.remote_script(&c);
        assert!(s.contains("exec $lbm_n $lbm_i gog gmail send --attach \"$d\"/in/a0/report.pdf"), "{s}");
        assert!(s.contains("<\"$d\"/in/.stdin"), "{s}");
        assert!(s.contains("tar -x -f - -C \"$d\"/in"), "{s}");
        assert!(s.contains("trap 'rm -rf \"$d\"' EXIT"), "{s}");
    }

    #[test]
    fn staged_names_are_checked() {
        assert!(safe_input_name("a0/report final.pdf"));
        assert!(!safe_input_name("../etc/passwd"));
        assert!(!safe_input_name("/abs"));
        assert!(!safe_input_name("a/$(x)"));
        let mut c = call(&["x"]);
        c.args.push(Arg::Input("missing".into()));
        assert!(matches!(check_inputs(&c), Err(Error::Invalid(_))));
    }

    #[test]
    fn bundles_round_trip() {
        let mut c = call(&["x"]);
        c.inputs.push(("a0/f.txt".into(), b"one".to_vec()));
        c.stdin = Some(b"two".to_vec());
        let tar_bytes = SshRunner::bundle_stdin(&c).unwrap();
        let mut ar = tar::Archive::new(&tar_bytes[..]);
        let names: Vec<String> = ar.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["a0/f.txt", ".stdin"]);

        // The reply side: what `tar -c -C out .` produces.
        let mut b = tar::Builder::new(Vec::new());
        for (n, d) in [("./stdout", &b"{}"[..]), ("./file", &b"PDF"[..])] {
            let mut h = tar::Header::new_gnu();
            h.set_size(d.len() as u64);
            h.set_cksum();
            b.append_data(&mut h, n, d).unwrap();
        }
        let (stdout, file) = unbundle_stdout(&b.into_inner().unwrap()).unwrap();
        assert_eq!(stdout, b"{}");
        assert_eq!(file.as_deref(), Some(&b"PDF"[..]));
    }

    #[tokio::test]
    async fn local_runner_stages_files_feeds_stdin_and_reads_back_the_out_file() {
        // `/bin/sh -c '<script>' sh <args>` stands in for gog: it copies an input and stdin to the out file.
        let r = LocalRunner::new("/bin/sh");
        let mut c = GogCall::new(
            vec![Arg::from("-c"), Arg::from(r#"cat "$1" - > "$2"; echo '{"ok":true}'; echo warn >&2; exit 3"#), Arg::from("sh"),
                 Arg::Input("a0/in.txt".into()), Arg::Out],
            Duration::from_secs(10),
        );
        c.inputs.push(("a0/in.txt".into(), b"file-".to_vec()));
        c.stdin = Some(b"stdin".to_vec());
        let out = r.run(c).await.unwrap();
        assert_eq!(out.code, 3);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), r#"{"ok":true}"#);
        assert_eq!(out.stderr.trim(), "warn");
        assert_eq!(out.file.as_deref(), Some(&b"file-stdin"[..]));
    }

    #[tokio::test]
    async fn local_runner_sources_the_env_file_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join("gog.env");
        std::fs::write(&env, "GOG_TEST_VALUE='from env'\n").unwrap();
        let r = LocalRunner::new("/bin/sh").with_env_file(&env);
        let c = GogCall::new(vec![Arg::from("-c"), Arg::from("printf %s \"$GOG_TEST_VALUE\"")], Duration::from_secs(10));
        let out = r.run(c).await.unwrap();
        assert_eq!(out.stdout, b"from env");

        let slow = GogCall::new(vec![Arg::from("-c"), Arg::from("sleep 5")], Duration::from_millis(200));
        assert!(matches!(LocalRunner::new("/bin/sh").run(slow).await, Err(Error::Offline(_))));
        let missing = GogCall::new(vec![], Duration::from_secs(1));
        assert!(matches!(LocalRunner::new("/nonexistent/gog").run(missing).await, Err(Error::Other(_))));
    }

    #[tokio::test]
    async fn ssh_runner_script_runs_under_a_local_shell() {
        // Run the exact remote script with /bin/sh in place of ssh: proves the staging protocol end to end.
        let dir = tempfile::tempdir().unwrap();
        let fake_ssh = dir.path().join("ssh");
        // Our fake ssh ignores options and the host, then runs the script it was given (the last argument).
        std::fs::write(&fake_ssh, "#!/bin/sh\nfor a; do s=$a; done\nexec /bin/sh -c \"$s\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let fake_gog = dir.path().join("gog");
        std::fs::write(&fake_gog, "#!/bin/sh\n# args: <in> <out>\ncat \"$1\" - > \"$2\"\necho '{\"saved\":1}'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_gog, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut r = SshRunner::new("nowhere", fake_gog.to_string_lossy().into_owned());
        r.ssh = fake_ssh;
        let mut c = GogCall::new(vec![Arg::Input("x/in.bin".into()), Arg::Out], Duration::from_secs(10));
        c.inputs.push(("x/in.bin".into(), b"abc".to_vec()));
        c.stdin = Some(b"def".to_vec());
        // A script we just wrote can be "text file busy" for a moment while another test thread forks
        // (the child briefly holds our write handle). Retry instead of failing on that race.
        let mut tries = 0;
        let out = loop {
            match r.run(c.clone()).await {
                Ok(o) if o.code == 126 && tries < 50 => {}
                Err(Error::Other(m)) if m.contains("Text file busy") && tries < 50 => {}
                other => break other.unwrap(),
            }
            tries += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), r#"{"saved":1}"#);
        assert_eq!(out.file.as_deref(), Some(&b"abcdef"[..]));

        // Plain calls skip the tar dance entirely.
        let plain = GogCall::new(vec![Arg::from("/dev/null"), Arg::from("/dev/stdout")], Duration::from_secs(10));
        let out = r.run(plain).await.unwrap();
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), r#"{"saved":1}"#);
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A script just written can be "text file busy" for a moment while another test thread forks;
        // wait until it can be executed so the runner's own spawns never hit that race.
        for _ in 0..100 {
            match std::process::Command::new(&p).arg("ControlPath=/nonexistent/x").stdout(Stdio::null()).stderr(Stdio::null()).status() {
                Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(20)),
                _ => break,
            }
        }
        p
    }

    /// Runs `r.run(c)`, retrying the "text file busy" race of freshly written scripts.
    async fn run_retrying(r: &SshRunner, c: GogCall) -> GogOutput {
        let mut tries = 0;
        loop {
            match r.run(c.clone()).await {
                Ok(o) if o.code == 126 && tries < 50 => {}
                Err(Error::Other(m)) if m.contains("Text file busy") && tries < 50 => {}
                other => return other.unwrap(),
            }
            tries += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn calls_share_one_master_connection_and_recover_from_a_dead_one() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        // A stand-in for ssh: logs its arguments; "ControlMaster=yes" makes the socket file and exits
        // like `ssh -f`; with a "dead" flag a client complains about the socket, then runs anyway.
        let fake_ssh = script(dir.path(), "ssh", &format!(
            "#!/bin/sh\necho \"$*\" >> {d}/log\ncp=''; master=''\nfor a; do case \"$a\" in ControlPath=*) cp=${{a#ControlPath=}};; ControlMaster=yes) master=1;; esac; s=$a; done\n\
             if [ -n \"$master\" ]; then : > \"$cp\"; exit 0; fi\n\
             if [ -f {d}/dead ] && [ -n \"$cp\" ] && [ \"$cp\" != none ]; then echo \"Control socket connect($cp): Connection refused\" >&2; fi\n\
             exec /bin/sh -c \"$s\"\n"));
        let fake_gog = script(dir.path(), "gog", "#!/bin/sh\necho '{\"ok\":1}'\n");
        let mux = Arc::new(Mux::new(dir.path().join("mux")));
        let mut r = SshRunner::new("box", fake_gog.to_string_lossy().into_owned()).with_mux(mux.clone());
        r.ssh = fake_ssh;
        r.niceness = Niceness { nice: None, ionice_idle: false };
        let c = || GogCall::new(vec![Arg::from("--json")], Duration::from_secs(10));
        for _ in 0..3 {
            assert_eq!(run_retrying(&r, c()).await.code, 0);
        }
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        let sock = mux.socket_for("box").display().to_string();
        assert_eq!(lines.iter().filter(|l| l.contains("ControlMaster=yes")).count(), 1, "one master: {log}");
        let master = lines.iter().find(|l| l.contains("ControlMaster=yes")).unwrap();
        assert!(master.contains("-f -N") && master.contains("ControlPersist=300"), "{log}");
        assert_eq!(lines.iter().filter(|l| l.contains("ControlMaster=no") && l.contains(&format!("ControlPath={sock}"))).count(), 3, "{log}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.path().join("mux")).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // The master died: the call still works (ssh went direct), the stale socket is dropped, and
        // the next call starts a new master.
        std::fs::write(dir.path().join("dead"), "").unwrap();
        assert_eq!(run_retrying(&r, c()).await.code, 0);
        assert!(!mux.socket_for("box").exists());
        std::fs::remove_file(dir.path().join("dead")).unwrap();
        assert_eq!(run_retrying(&r, c()).await.code, 0);
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert_eq!(log.lines().filter(|l| l.contains("ControlMaster=yes")).count(), 2, "{log}");
    }

    #[tokio::test]
    async fn without_a_master_calls_go_direct() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        // This ssh cannot multiplex: the master never appears.
        let fake_ssh = script(dir.path(), "ssh", &format!(
            "#!/bin/sh\necho \"$*\" >> {d}/log\nfor a; do case \"$a\" in ControlMaster=yes) exit 255;; esac; s=$a; done\nexec /bin/sh -c \"$s\"\n"));
        let fake_gog = script(dir.path(), "gog", "#!/bin/sh\necho '{}'\n");
        let mut r = SshRunner::new("box", fake_gog.to_string_lossy().into_owned()).with_mux(Arc::new(Mux::new(dir.path().join("mux"))));
        r.ssh = fake_ssh;
        let c = || GogCall::new(vec![Arg::from("--json")], Duration::from_secs(10));
        assert_eq!(run_retrying(&r, c()).await.code, 0);
        assert_eq!(run_retrying(&r, c()).await.code, 0);
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert_eq!(log.lines().filter(|l| l.contains("ControlMaster=yes")).count(), 1, "not retried at every call: {log}");
        assert_eq!(log.lines().filter(|l| l.contains("ControlPath=none")).count(), 2, "{log}");
    }
}
