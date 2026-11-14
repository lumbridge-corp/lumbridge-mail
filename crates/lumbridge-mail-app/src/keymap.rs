//! Every action and its keys, in one table. The bindings, the command palette and the shortcuts sheet
//! all read `COMMANDS`, so they cannot drift apart (and the README's table is checked against it in a test).

use gpui_kit::{Action, App, KeyBinding, KeyBindingContextPredicate};
use std::rc::Rc;

gpui_kit::actions!(
    mail,
    [
        Next,
        Prev,
        Open,
        Back,
        Archive,
        Trash,
        ToggleStar,
        ToggleRead,
        Reply,
        ReplyAll,
        Forward,
        Compose,
        FocusSearch,
        TogglePalette,
        ToggleShortcuts,
        GoInbox,
        GoStarred,
        GoSent,
        GoDrafts,
        GoArchive,
        GoImportant,
        GoTrash,
        ToggleSelect,
        SelectAll,
        ClearSelection,
        ToggleAiPanel,
        ToggleTheme,
        Undo,
        SyncNow,
        LoadRemote,
        ExpandAll,
        ToggleOffline,
        Send,
        CloseComposer,
        AttachFile,
        ToggleCcBcc,
        ToggleDensity,
        ToggleNotifications,
        ToggleAnonNotifications,
        ForgetImageSenders,
    ]
);

/// Go to a view by key: `u:inbox`, `c:Newsletter`, `m:<account>|<mailbox>` (the palette's "Go to" rows).
#[derive(Clone, PartialEq, Debug, Default, gpui::Action)]
#[action(namespace = mail, no_json)]
pub struct GoView {
    pub key: String,
}

/// Where a key works. `Nav`: the mail list and reader have focus (single letters are safe there).
/// `App`: anywhere in the window. `Composer`: inside the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Nav,
    App,
    Composer,
}

impl Scope {
    pub fn context(self) -> &'static str {
        match self {
            Scope::Nav => NAV_CONTEXT,
            Scope::App => APP_CONTEXT,
            Scope::Composer => COMPOSER_CONTEXT,
        }
    }
}

pub const NAV_CONTEXT: &str = "MailNav";
pub const APP_CONTEXT: &str = "MailApp";
pub const COMPOSER_CONTEXT: &str = "MailComposer";
pub const SEARCH_CONTEXT: &str = "MailSearch";

pub struct Command {
    /// Space-separated keystrokes per binding; several bindings separated by `|`. First one is shown.
    /// Empty: no key, the palette only (settings).
    pub keys: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub scope: Scope,
    pub action: fn() -> Box<dyn Action>,
    /// Shown in the command palette.
    pub in_palette: bool,
}

macro_rules! cmd {
    ($keys:expr, $label:expr, $group:expr, $scope:ident, $action:ident) => {
        Command { keys: $keys, label: $label, group: $group, scope: Scope::$scope,
                  action: || Box::new($action), in_palette: true }
    };
    ($keys:expr, $label:expr, $group:expr, $scope:ident, $action:ident, hidden) => {
        Command { keys: $keys, label: $label, group: $group, scope: Scope::$scope,
                  action: || Box::new($action), in_palette: false }
    };
}

pub const COMMANDS: &[Command] = &[
    cmd!("j|down", "Next conversation", "Move", Nav, Next),
    cmd!("k|up", "Previous conversation", "Move", Nav, Prev),
    cmd!("enter|o", "Open conversation", "Move", Nav, Open),
    cmd!("escape", "Back / close", "Move", App, Back, hidden),
    cmd!("x", "Select conversation", "Move", Nav, ToggleSelect),
    cmd!("* a|shift-8 a", "Select all loaded", "Move", Nav, SelectAll),
    cmd!("* n|shift-8 n", "Clear selection", "Move", Nav, ClearSelection),
    cmd!("g i", "Go to Unified Inbox", "Go to", Nav, GoInbox),
    cmd!("g !|g shift-1", "Go to Important", "Go to", Nav, GoImportant),
    cmd!("g s", "Go to Starred", "Go to", Nav, GoStarred),
    cmd!("g t", "Go to Sent", "Go to", Nav, GoSent),
    cmd!("g d", "Go to Drafts", "Go to", Nav, GoDrafts),
    cmd!("g a", "Go to Archive", "Go to", Nav, GoArchive),
    cmd!("g #|g shift-3", "Go to Trash", "Go to", Nav, GoTrash),
    cmd!("e", "Archive", "Act", Nav, Archive),
    cmd!("#|shift-3|delete", "Move to Trash", "Act", Nav, Trash),
    cmd!("s", "Star / unstar", "Act", Nav, ToggleStar),
    cmd!("u", "Mark read / unread", "Act", Nav, ToggleRead),
    cmd!("z", "Undo", "Act", Nav, Undo),
    cmd!("r", "Reply", "Write", Nav, Reply),
    cmd!("a", "Reply all", "Write", Nav, ReplyAll),
    cmd!("f", "Forward", "Write", Nav, Forward),
    cmd!("c", "Compose", "Write", Nav, Compose),
    cmd!("secondary-enter", "Send", "Write", Composer, Send, hidden),
    cmd!("secondary-shift-a", "Attach a file", "Write", Composer, AttachFile, hidden),
    cmd!("secondary-shift-c", "Show Cc and Bcc", "Write", Composer, ToggleCcBcc, hidden),
    cmd!("/", "Search", "Find", Nav, FocusSearch),
    cmd!("secondary-k|ctrl-k", "Command palette", "Find", App, TogglePalette, hidden),
    cmd!("?|shift-/", "Keyboard shortcuts", "Find", Nav, ToggleShortcuts),
    cmd!("i", "AI panel", "View", Nav, ToggleAiPanel),
    cmd!("m", "Load remote images", "View", Nav, LoadRemote),
    cmd!(";", "Expand all messages", "View", Nav, ExpandAll),
    cmd!("secondary-shift-l", "Light / dark", "View", App, ToggleTheme),
    cmd!("shift-r", "Sync now", "View", Nav, SyncNow),
    cmd!("secondary-shift-o", "Work offline (demo)", "View", App, ToggleOffline),
    cmd!("", "Compact / comfortable list", "Settings", App, ToggleDensity),
    cmd!("", "Desktop notifications on / off", "Settings", App, ToggleNotifications),
    cmd!("", "Notifications for anon accounts on / off", "Settings", App, ToggleAnonNotifications),
    cmd!("", "Forget senders allowed to show images", "Settings", App, ForgetImageSenders),
];

/// The groups, in the order the shortcuts sheet shows them.
pub const GROUPS: &[&str] = &["Write", "Act", "Go to", "Find", "View", "Move", "Settings"];

pub fn bind(cx: &mut App) {
    let mut bindings = Vec::new();
    for c in COMMANDS {
        let predicate = KeyBindingContextPredicate::parse(c.scope.context()).ok().map(Rc::new);
        for keys in c.keys.split('|').filter(|k| !k.is_empty()) {
            match KeyBinding::load(keys, (c.action)(), predicate.clone(), false, None, cx) {
                Ok(b) => bindings.push(b),
                Err(e) => eprintln!("lumbridge-mail: bad key binding {keys:?}: {e}"),
            }
        }
    }
    // The composer closes on escape; the palette and sheets do too (handled by `Back`).
    bindings.push(KeyBinding::new("escape", CloseComposer, Some(COMPOSER_CONTEXT)));
    // Text fields bind enter and escape themselves; inside the composer and the search field ours win
    // (loaded later, same depth).
    let composer_input = format!("{COMPOSER_CONTEXT} > Input");
    for (keys, action) in [("secondary-enter", Box::new(Send) as Box<dyn Action>), ("escape", Box::new(CloseComposer))] {
        let predicate = KeyBindingContextPredicate::parse(&composer_input).ok().map(Rc::new);
        if let Ok(b) = KeyBinding::load(keys, action, predicate, false, None, cx) {
            bindings.push(b);
        }
    }
    let search_input = format!("{SEARCH_CONTEXT} > Input");
    let predicate = KeyBindingContextPredicate::parse(&search_input).ok().map(Rc::new);
    if let Ok(b) = KeyBinding::load("escape", Box::new(Back), predicate, false, None, cx) {
        bindings.push(b);
    }
    cx.bind_keys(bindings);
}

gpui_kit::actions!(lumbridge_mail, [Quit]);

/// The process-wide keys `main` binds: quit on `secondary-q` / `secondary-w`. Through `KeyBinding::load`
/// like every other key (`KeyBinding::new` does not know `secondary` and panics on it).
pub fn bind_app_keys(cx: &mut App) {
    let mut bindings = Vec::new();
    for keys in ["secondary-q", "secondary-w"] {
        match KeyBinding::load(keys, Box::new(Quit), None, false, None, cx) {
            Ok(b) => bindings.push(b),
            Err(e) => eprintln!("lumbridge-mail: bad key binding {keys:?}: {e}"),
        }
    }
    cx.bind_keys(bindings);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

/// The keys of a command as shown to people: "g i", "⌘K" on macOS, "Ctrl K" elsewhere.
pub fn display_keys(keys: &str) -> Vec<String> {
    let first = keys.split('|').next().unwrap_or("");
    if first.is_empty() {
        return Vec::new();
    }
    first.split(' ').map(display_stroke).collect()
}

/// Every binding of `keys` as people see it, without repeats: on Linux and Windows `secondary-k|ctrl-k`
/// is "Ctrl K" once, not "Ctrl K or Ctrl K".
pub fn display_alternatives(keys: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for k in keys.split('|').filter(|k| !k.is_empty()) {
        let shown = display_keys(k);
        if !out.contains(&shown) {
            out.push(shown);
        }
    }
    out
}

fn display_stroke(stroke: &str) -> String {
    let mac = cfg!(target_os = "macos");
    let parts: Vec<&str> = if stroke == "-" { vec!["-"] } else { stroke.split('-').collect() };
    let (mods, key) = parts.split_at(parts.len() - 1);
    let key = key.first().copied().unwrap_or("");
    let mut out = String::new();
    for m in mods {
        out.push_str(match (*m, mac) {
            ("secondary", true) | ("cmd", _) => "⌘",
            ("secondary", false) | ("ctrl", _) => "Ctrl ",
            ("shift", true) => "⇧",
            ("shift", false) => "Shift ",
            ("alt", true) => "⌥",
            ("alt", false) => "Alt ",
            _ => "",
        });
    }
    let key = match key {
        "enter" => "Enter".to_string(),
        "escape" => "Esc".to_string(),
        "delete" => "Del".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        k if k.len() == 1 => k.to_uppercase(),
        k => k.to_string(),
    };
    out + &key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_display_for_people() {
        assert_eq!(display_keys("g i"), vec!["G", "I"]);
        assert_eq!(display_keys("j|down"), vec!["J"]);
        if !cfg!(target_os = "macos") {
            assert_eq!(display_keys("secondary-k|ctrl-k"), vec!["Ctrl K"]);
            assert_eq!(display_keys("secondary-shift-l"), vec!["Ctrl Shift L"]);
            assert_eq!(display_alternatives("secondary-k|ctrl-k"), vec![vec!["Ctrl K"]]);
        }
        assert_eq!(display_alternatives("j|down"), vec![vec!["J"], vec!["↓"]]);
    }

    #[test]
    fn every_group_is_known_and_labels_are_unique() {
        let mut labels = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(GROUPS.contains(&c.group), "{}", c.group);
            assert!(labels.insert(c.label), "duplicate {}", c.label);
        }
    }

    #[test]
    fn the_readme_lists_every_key() {
        let readme = include_str!("../README.md");
        for c in COMMANDS.iter().filter(|c| !c.keys.is_empty()) {
            let first = c.keys.split('|').next().unwrap();
            assert!(readme.contains(&format!("`{first}`")), "README is missing `{first}` ({})", c.label);
        }
    }
}
