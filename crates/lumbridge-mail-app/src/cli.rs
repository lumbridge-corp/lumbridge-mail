//! Command-line arguments of `lumbridge-mail` (kept here, not in `main.rs`, so they are tested).

use std::path::PathBuf;

pub const USAGE: &str = "usage: lumbridge-mail [--demo [--threads N]] [--socket PATH | --remote HOST] [--no-start]
                     [--light | --dark] [--undo-send SECONDS] [--new-instance] [mailto:...]

  --demo            made-up mail, no daemon
  --socket PATH     the daemon's socket (default: $LUMBRIDGE_MAIL_HOME/maild.sock, else
                    $XDG_RUNTIME_DIR/lumbridge-mail/maild.sock); a socket given here is never auto-started
  --remote HOST     use the daemon on HOST over ssh (`ssh -T HOST lbmail pipe`; never started from here);
                    `local` for this machine's. Default: $LBMAIL_REMOTE, else `[daemon] remote` in config.toml
  --no-start        do not start lumbridge-maild when it is not running
  --light, --dark   the appearance for this run (otherwise the saved setting)
  --new-instance    do not hand over to a running window (tests, a second demo)
  mailto:...        open the composer with this address (what the desktop's mail handler passes)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub demo: bool,
    pub threads: usize,
    pub socket: Option<PathBuf>,
    /// `--remote HOST` (`local`: this machine's daemon).
    pub remote: Option<String>,
    pub no_start: bool,
    /// Some(true) = dark, Some(false) = light, None = the saved setting.
    pub dark: Option<bool>,
    pub undo_send: u64,
    pub new_instance: bool,
    pub mailto: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Args),
    Help,
}

pub fn parse(args: impl IntoIterator<Item = String>, default_threads: usize) -> Result<Parsed, String> {
    let mut a = Args { demo: false, threads: default_threads, socket: None, remote: None, no_start: false, dark: None, undo_send: 5,
                       new_instance: false, mailto: None };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => a.demo = true,
            "--light" => a.dark = Some(false),
            "--dark" => a.dark = Some(true),
            "--no-start" => a.no_start = true,
            "--new-instance" => a.new_instance = true,
            "--threads" => a.threads = it.next().and_then(|n| n.parse().ok()).ok_or("--threads needs a number")?,
            "--socket" => a.socket = Some(it.next().ok_or("--socket needs a path")?.into()),
            "--remote" => a.remote = Some(it.next().ok_or("--remote needs an ssh host (or local)")?),
            "--undo-send" => a.undo_send = it.next().and_then(|n| n.parse().ok()).ok_or("--undo-send needs seconds")?,
            "-h" | "--help" => return Ok(Parsed::Help),
            url if url.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:")) => a.mailto = Some(url.to_string()),
            // Desktop launchers pass `%u` even when there is no URL.
            "" => {}
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok(Parsed::Run(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Args {
        match parse(args.iter().map(|s| s.to_string()), 320).unwrap() {
            Parsed::Run(a) => a,
            Parsed::Help => panic!("help"),
        }
    }

    #[test]
    fn flags_and_a_mailto() {
        let a = run(&[]);
        assert!(!a.demo && a.dark.is_none() && a.mailto.is_none() && a.threads == 320);
        let a = run(&["--demo", "--threads", "50000", "--light", "mailto:maya.chen@postbox.example?subject=Hi"]);
        assert!(a.demo);
        assert_eq!(a.threads, 50_000);
        assert_eq!(a.dark, Some(false));
        assert_eq!(a.mailto.as_deref(), Some("mailto:maya.chen@postbox.example?subject=Hi"));
        let a = run(&["--socket", "/tmp/x.sock", "--no-start", "MAILTO:a@b.example", ""]);
        assert_eq!(a.socket, Some(PathBuf::from("/tmp/x.sock")));
        assert!(a.no_start);
        assert!(a.mailto.is_some());
        assert_eq!(parse(["--help".to_string()], 1).unwrap(), Parsed::Help);
        assert!(parse(["--threads".to_string()], 1).is_err());
        assert!(parse(["https://example.com".to_string()], 1).is_err(), "only mailto URLs");
        assert_eq!(run(&["--remote", "homeserver"]).remote.as_deref(), Some("homeserver"));
        assert!(parse(["--remote".to_string()], 1).is_err());
    }
}
