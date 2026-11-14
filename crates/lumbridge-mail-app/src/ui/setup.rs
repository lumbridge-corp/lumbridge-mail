//! The first-run screen: the daemon is there but has no accounts. Two choices: "Try with demo mail"
//! (three made-up accounts, no network) or "Add an account": Gmail through gog (the gog binary here or
//! on an ssh host, and the address; read-only unless unticked) or IMAP (the address, servers guessed by
//! the daemon's `accounts.discover`, a password that goes to the OS keyring) or Resend (a from address on
//! a domain verified at Resend, and an API key that goes to the OS keyring). All write the daemon's
//! `config.toml` through `accounts.add`, which also reloads it; then the window loads again.

use super::widgets::*;
use crate::app::MailApp;
use gpui::prelude::*;
use gpui::*;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Sizable;
use lumbridge_mail_core::rpc::{Discovered, NewAccount, NewProvider};
use lumbridge_mail_core::Persona;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Choose,
    Gog,
    Imap,
    Resend,
}

pub const PERSONAS: [(&str, Persona); 3] = [("Personal", Persona::Personal), ("Work", Persona::Work), ("Anon", Persona::Anon)];

pub struct Setup {
    pub step: Step,
    pub email: Entity<InputState>,
    pub name: Entity<InputState>,
    /// gog: a path to the binary here, or `ssh:<host>`.
    pub gog_where: Entity<InputState>,
    pub imap_host: Entity<InputState>,
    pub smtp_host: Entity<InputState>,
    pub password: Entity<InputState>,
    /// Resend: the API key (masked; to the keyring through `accounts.add`).
    pub api_key: Entity<InputState>,
    /// Resend: the domain receives mail through Resend (an Inbox).
    pub receiving: bool,
    /// Index into [`PERSONAS`].
    pub persona: usize,
    pub readonly: bool,
    pub discovered: Option<Discovered>,
    pub busy: bool,
    pub error: Option<String>,
}

fn field(window: &mut Window, cx: &mut App, placeholder: &'static str) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

impl Setup {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let password = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("Password or app password");
            s.set_masked(true, window, cx);
            s
        });
        let api_key = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("re_… (a full-access key lists mail; a sending key only sends)");
            s.set_masked(true, window, cx);
            s
        });
        Self {
            step: Step::Choose,
            api_key,
            receiving: true,
            email: field(window, cx, "you@example.com"),
            name: field(window, cx, "Your name, as mail is sent"),
            gog_where: field(window, cx, "gog, /path/to/gog, or ssh:host"),
            imap_host: field(window, cx, "imap.example.com"),
            smtp_host: field(window, cx, "smtp.example.com (empty: no sending)"),
            password,
            persona: 0,
            readonly: true,
            discovered: None,
            busy: false,
            error: None,
        }
    }

    /// The account the form describes (None with the reason when something is missing).
    pub fn new_account(&self, cx: &App) -> Result<(NewAccount, Option<String>), String> {
        let v = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let email = v(&self.email);
        if !email.contains('@') {
            return Err("Enter the account's email address.".into());
        }
        let persona = PERSONAS[self.persona.min(PERSONAS.len() - 1)].1.clone();
        let (provider, password) = match self.step {
            Step::Gog => {
                let w = v(&self.gog_where);
                let (binary, ssh_host) = match w.strip_prefix("ssh:") {
                    Some(h) if !h.trim().is_empty() => (None, Some(h.trim().to_string())),
                    Some(_) => return Err("Name the ssh host after ssh: (for example ssh:mail-box).".into()),
                    None => ((!w.is_empty()).then_some(w), None),
                };
                (NewProvider::Gog { binary, ssh_host }, None)
            }
            Step::Imap => {
                let imap_host = v(&self.imap_host);
                if imap_host.is_empty() {
                    return Err("Enter the IMAP server (Find servers fills it in).".into());
                }
                let smtp_host = v(&self.smtp_host);
                let d = self.discovered.as_ref();
                let same_imap = d.is_some_and(|d| d.imap_host == imap_host);
                let same_smtp = d.is_some_and(|d| d.smtp_host == smtp_host);
                let password = self.password.read(cx).value().to_string();
                if password.is_empty() {
                    return Err("Enter the password; it is kept in your system keyring, not in a file.".into());
                }
                (
                    NewProvider::Imap {
                        imap_port: d.filter(|_| same_imap).map(|d| d.imap_port),
                        imap_security: d.filter(|_| same_imap).map(|d| d.imap_security.clone()),
                        smtp_port: d.filter(|_| same_smtp).map(|d| d.smtp_port),
                        smtp_security: d.filter(|_| same_smtp).map(|d| d.smtp_security.clone()),
                        imap_host,
                        smtp_host: (!smtp_host.is_empty()).then_some(smtp_host),
                        username: None,
                    },
                    Some(password),
                )
            }
            Step::Resend => {
                let key = self.api_key.read(cx).value().trim().to_string();
                if key.is_empty() {
                    return Err("Enter the Resend API key; it is kept in your system keyring, not in a file.".into());
                }
                (NewProvider::Resend { addresses: Vec::new(), receiving: Some(self.receiving) }, Some(key))
            }
            Step::Choose => return Err("Choose how to add mail.".into()),
        };
        Ok((
            NewAccount { id: None, persona, label: String::new(), email, name: v(&self.name), readonly: self.readonly, provider, password: None },
            password,
        ))
    }
}

/// The made-up accounts "Try with demo mail" adds (the daemon's demo provider; nothing leaves the machine).
pub fn demo_accounts() -> Vec<NewAccount> {
    let one = |id: &str, persona: Persona, label: &str, email: &str, name: &str, set: &str| NewAccount {
        id: Some(id.into()),
        persona,
        label: label.into(),
        email: email.into(),
        name: name.into(),
        readonly: false,
        provider: NewProvider::Demo { set: Some(set.into()) },
        password: None,
    };
    vec![
        one("demo-personal", Persona::Personal, "Personal (demo)", "sam@example.com", "Sam Example", "personal"),
        one("demo-work", Persona::Work, "Work (demo)", "sam@work.example", "Sam Example", "work"),
        one("demo-anon", Persona::Anon, "Forum (demo)", "otter@anon.example", "Otter", "anon"),
    ]
}

impl MailApp {
    /// Connected to a daemon that has no accounts: show the first-run screen instead of an empty inbox.
    pub fn needs_setup(&self) -> bool {
        self.accounts_loaded && self.accounts.is_empty() && !self.is_demo()
    }

    pub fn setup_step(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.setup.step = step;
        self.setup.error = None;
        // gog is for a first live Gmail account: read-only unless the person unticks it.
        self.setup.readonly = step == Step::Gog;
        self.setup.email.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub fn setup_demo(&mut self, cx: &mut Context<Self>) {
        self.setup.busy = true;
        self.setup.error = None;
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let mut failure = None;
            for a in demo_accounts() {
                if let Err(e) = b.add_account(a).await {
                    failure = Some(e);
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.setup.busy = false;
                match failure {
                    Some(e) => this.setup.error = Some(crate::app::plain_error(&e)),
                    None => {
                        this.show_toast("Added three demo accounts (made-up mail)", false, cx);
                        this.retry(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// "Find servers": the daemon's guess for the address fills the IMAP and SMTP fields.
    pub fn setup_discover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let email = self.setup.email.read(cx).value().trim().to_string();
        if !email.contains('@') {
            self.setup.error = Some("Enter the email address first.".into());
            cx.notify();
            return;
        }
        let b = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let found = b.discover(email).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match found {
                    Ok(d) => {
                        let (imap, smtp) = (d.imap_host.clone(), d.smtp_host.clone());
                        this.setup.imap_host.update(cx, |s, cx| s.set_value(imap, window, cx));
                        this.setup.smtp_host.update(cx, |s, cx| s.set_value(smtp, window, cx));
                        this.setup.discovered = Some(d);
                        this.setup.error = None;
                    }
                    Err(e) => this.setup.error = Some(crate::app::plain_error(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn setup_submit(&mut self, cx: &mut Context<Self>) {
        let (mut account, password) = match self.setup.new_account(cx) {
            Ok(x) => x,
            Err(why) => {
                self.setup.error = Some(why);
                cx.notify();
                return;
            }
        };
        account.password = password;
        self.setup.busy = true;
        self.setup.error = None;
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let added = b.add_account(account).await;
            let _ = this.update(cx, |this, cx| {
                this.setup.busy = false;
                match added {
                    Ok(a) => {
                        let ro = if a.readonly { " (read-only)" } else { "" };
                        this.show_toast(format!("Added {}{ro}", a.email), false, cx);
                        this.retry(cx);
                    }
                    Err(e) => this.setup.error = Some(crate::app::plain_error(&e)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn setup_choice(&self, id: &'static str, glyph: &'static str, title: &'static str, hint: &'static str) -> Stateful<Div> {
        let p = self.pal;
        h_row()
            .id(id)
            .gap_3()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(p.line)
            .bg(p.raised)
            .hover(|s| s.border_color(p.accent))
            .child(div().size(px(36.)).rounded_md().bg(p.accent.opacity(0.14)).flex().items_center().justify_center()
                .child(icon(glyph, 16., p.accent)))
            .child(v_col().flex_1().gap_0p5()
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title))
                .child(div().text_xs().text_color(p.dim).child(hint)))
            .child(icon("chevron-right", 14., p.dim))
    }

    fn labelled(&self, label: &'static str, input: &Entity<InputState>) -> Div {
        let p = self.pal;
        v_col().gap_1()
            .child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(p.muted).child(label))
            .child(Input::new(input).small())
    }

    pub fn render_setup(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let s = &self.setup;
        let mut card = v_col()
            .id("setup")
            .w(px(520.))
            .gap_4()
            .p_6()
            .rounded_xl()
            .border_1()
            .border_color(p.line)
            .bg(p.surface)
            .child(v_col().gap_1()
                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child("Welcome to Lumbridge Mail"))
                .child(div().text_sm().text_color(p.muted).child(
                    "No accounts yet. lumbridge-maild keeps your mail on this machine and in sync; add one to start.")));
        match s.step {
            Step::Choose => {
                card = card
                    .child(self.setup_choice("setup-demo", "sparkles", "Try with demo mail",
                                             "Three made-up accounts (personal, work, anon). No network, nothing real.")
                        .on_click(cx.listener(|this, _, _, cx| this.setup_demo(cx))))
                    .child(self.setup_choice("setup-gog", "mail", "Add Gmail through gog",
                                             "Uses gog's existing sign-in. Starts read-only.")
                        .on_click(cx.listener(|this, _, w, cx| this.setup_step(Step::Gog, w, cx))))
                    .child(self.setup_choice("setup-imap", "server", "Add another account (IMAP)",
                                             "Fastmail, iCloud, Proton Bridge, your own server. Password to the keyring.")
                        .on_click(cx.listener(|this, _, w, cx| this.setup_step(Step::Imap, w, cx))))
                    .child(self.setup_choice("setup-resend", "send", "Add Resend (send and receive for a domain)",
                                             "A domain verified at Resend. Sent mail with delivery status, received mail. API key to the keyring.")
                        .on_click(cx.listener(|this, _, w, cx| this.setup_step(Step::Resend, w, cx))));
            }
            step => {
                let title = match step {
                    Step::Gog => "Gmail through gog",
                    Step::Resend => "Resend for a domain",
                    _ => "An IMAP account",
                };
                card = card
                    .child(h_row().gap_2()
                        .child(div().id("setup-back").text_xs().text_color(p.accent).hover(|s| s.underline()).child("‹ Back")
                            .on_click(cx.listener(|this, _, w, cx| this.setup_step(Step::Choose, w, cx))))
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title)))
                    .child(self.labelled("Email address", &s.email.clone()))
                    .child(self.labelled("Your name", &s.name.clone()))
                    .child(v_col().gap_1()
                        .child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(p.muted).child("Persona"))
                        .child(segmented(&p, "setup-persona", &["Personal", "Work", "Anon"], s.persona,
                                         cx.listener(|this, ix: &usize, _, cx| {
                                             this.setup.persona = *ix;
                                             cx.notify();
                                         }))));
                if step == Step::Gog {
                    card = card
                        .child(self.labelled("Where gog runs", &s.gog_where.clone()))
                        .child(div().text_xs().text_color(p.dim).child(
                            "gog must already be signed in to this Gmail account. ssh:host runs it on the machine that holds the sign-in."));
                } else if step == Step::Resend {
                    card = card
                        .child(self.labelled("Resend API key", &s.api_key.clone()))
                        .child(Checkbox::new("setup-receiving").checked(s.receiving)
                            .label("This domain receives mail through Resend (show an Inbox)")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.setup.receiving = *checked;
                                cx.notify();
                            })))
                        .child(div().text_xs().text_color(p.dim).child(
                            "The address is the From of mail you send; its domain must be verified at Resend. \
                             Read state, stars and drafts stay on this device: Resend does not keep them."));
                } else {
                    card = card
                        .child(h_row().gap_2().items_end()
                            .child(div().flex_1().child(self.labelled("IMAP server", &s.imap_host.clone())))
                            .child(Button::new("setup-find").small().outline().label("Find servers")
                                .on_click(cx.listener(|this, _, w, cx| this.setup_discover(w, cx)))))
                        .child(self.labelled("SMTP server", &s.smtp_host.clone()))
                        .when_some(s.discovered.as_ref().and_then(|d| d.note.clone()), |d, note| {
                            d.child(div().text_xs().text_color(p.dim).child(note))
                        })
                        .child(self.labelled("Password", &s.password.clone()));
                }
                card = card
                    .child(Checkbox::new("setup-readonly").checked(s.readonly)
                        .label("Read-only: never change anything on the server")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.setup.readonly = *checked;
                            cx.notify();
                        })))
                    .child(h_row().gap_2().justify_end()
                        .child(Button::new("setup-add").small().primary().label("Add account").loading(s.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.setup_submit(cx)))));
            }
        }
        if let Some(e) = &s.error {
            card = card.child(h_row().gap_2().p_3().rounded_md().bg(p.blocked.opacity(0.1))
                .child(icon("circle-alert", 13., p.blocked))
                .child(div().flex_1().text_sm().text_color(p.text).child(e.clone())));
        }
        if s.busy && s.step == Step::Choose {
            card = card.child(div().text_xs().text_color(p.dim).child("Adding…"));
        }
        div().flex().flex_1().h_full().items_center().justify_center().bg(p.canvas).child(card).into_any_element()
    }
}
