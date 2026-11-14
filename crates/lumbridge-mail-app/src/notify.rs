//! Desktop notifications for new mail, behind a setting.
//!
//! The rules (CONTRIBUTING.md rules 3 and 4) are in [`new_mail_note`], which is pure and tested: sensitive mail shows
//! only its one fixed line ("A security code from Brightdesk"), anon accounts show nothing unless the user
//! turned that on. Delivery is the OS's own tool, so the app links nothing extra: `notify-send`
//! (libnotify) on Linux, `osascript` on macOS. Mail text is passed as arguments, never through a shell
//! or a script string, and escaped for the notification server's markup.

use crate::settings::Notifications;
use lumbridge_mail_core::safety;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub title: String,
    pub body: String,
}

pub trait Notifier: Send + Sync {
    fn notify(&self, note: &Note);
}

fn clip(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        flat.to_string()
    } else {
        format!("{}…", flat.chars().take(max - 1).collect::<String>())
    }
}

/// What to show for a new message, or None for nothing.
pub fn new_mail_note(from: &str, subject: &str, anon: bool, settings: &Notifications) -> Option<Note> {
    if !settings.enabled || (anon && !settings.anon) {
        return None;
    }
    let from = clip(if from.trim().is_empty() { "someone" } else { from }, 80);
    if let Some(kind) = safety::sensitive_kind(&from, subject, "") {
        return Some(Note { title: "Lumbridge Mail".into(), body: format!("{kind} from {from}") });
    }
    let subject = if subject.trim().is_empty() { "(no subject)".to_string() } else { clip(subject, 200) };
    Some(Note { title: from, body: subject })
}

/// The notification servers on Linux read a small markup language in the body.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The OS's notification tool. Failures are ignored (a desktop without a notification server just shows
/// nothing); the in-app toast is still there.
pub struct System;

impl System {
    fn command(note: &Note) -> Command {
        if cfg!(target_os = "macos") {
            let mut c = Command::new("osascript");
            c.args([
                "-e", "on run argv",
                "-e", "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e", "end run",
                &note.title, &note.body,
            ]);
            c
        } else {
            let mut c = Command::new("notify-send");
            c.args(["--app-name=Lumbridge Mail", "--icon=com.lumbridgecorp.mail", "--category=email.arrived", "--"])
                .arg(&note.title)
                .arg(escape_markup(&note.body));
            c
        }
    }
}

impl Notifier for System {
    fn notify(&self, note: &Note) {
        let mut cmd = Self::command(note);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            // Reap it off the UI thread.
            let _ = std::thread::Builder::new().name("notify".into()).spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Collects notes instead of showing them (tests).
#[derive(Default)]
pub struct Recorder(pub parking_lot::Mutex<Vec<Note>>);

impl Notifier for Recorder {
    fn notify(&self, note: &Note) {
        self.0.lock().push(note.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: Notifications = Notifications { enabled: true, anon: false };

    #[test]
    fn ordinary_mail_shows_sender_and_subject() {
        let n = new_mail_note("Maya Chen", "Running late tonight", false, &ON).unwrap();
        assert_eq!(n, Note { title: "Maya Chen".into(), body: "Running late tonight".into() });
        assert!(new_mail_note("Maya", "x", false, &Notifications { enabled: false, anon: true }).is_none());
    }

    #[test]
    fn sensitive_mail_shows_only_the_fixed_line() {
        let n = new_mail_note("Brightdesk", "Your sign-in code is 482913", false, &ON).unwrap();
        assert_eq!(n.body, "A security code from Brightdesk");
        assert!(!n.body.contains("482913") && !n.title.contains("482913"));
    }

    #[test]
    fn anon_is_silent_unless_enabled() {
        assert!(new_mail_note("kestrel_92", "Offer", true, &ON).is_none());
        let on = Notifications { enabled: true, anon: true };
        assert_eq!(new_mail_note("kestrel_92", "Offer", true, &on).unwrap().body, "Offer");
    }

    #[test]
    fn hostile_text_stays_text() {
        let n = new_mail_note("-rf <b>x</b>", "line\nbreak <i>&</i>", false, &ON).unwrap();
        assert_eq!(n.body, "line break <i>&</i>");
        assert_eq!(escape_markup(&n.body), "line break &lt;i&gt;&amp;&lt;/i&gt;");
        let args: Vec<String> = System::command(&n).get_args().map(|a| a.to_string_lossy().into()).collect();
        if !cfg!(target_os = "macos") {
            let dashdash = args.iter().position(|a| a == "--").unwrap();
            assert_eq!(args[dashdash + 1], "-rf <b>x</b>", "a title starting with - is not an option");
        }
        assert!(clip(&"x".repeat(500), 200).chars().count() == 200);
    }
}
