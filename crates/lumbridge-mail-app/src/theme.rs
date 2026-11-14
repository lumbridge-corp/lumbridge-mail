//! The look, from `~/src/applied-research-1` (themes/applied-research-1.json, mods/.../kit.js): dark chrome
//! with mail read on a light "paper" card, and the same paper in the light appearance. Status colors
//! (red, amber, green) are kept for state; categories use their own validated order.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{px, rgb, rgba, App, Hsla};
use lumbridge_mail_core::Category;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub canvas: Hsla,
    pub chrome: Hsla,
    pub surface: Hsla,
    pub raised: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub line: Hsla,
    pub strong: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub dim: Hsla,
    pub accent: Hsla,
    pub accent_fg: Hsla,
    pub working: Hsla,
    pub blocked: Hsla,
    pub done: Hsla,
    pub anon: Hsla,
    pub backdrop: Hsla,
    pub paper: Paper,
}

/// The reader card: light in both appearances, so mail reads like print.
#[derive(Debug, Clone, Copy)]
pub struct Paper {
    pub card: Hsla,
    pub edge: Hsla,
    pub well: Hsla,
    pub grid: Hsla,
    pub hover: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub dim: Hsla,
    pub link: Hsla,
    pub quote: Hsla,
    pub warn_bg: Hsla,
    pub warn: Hsla,
    /// Deceptive links, forged senders.
    pub bad: Hsla,
    pub bad_bg: Hsla,
    /// A verified sender.
    pub good: Hsla,
    /// Code blocks and inline code.
    pub code_bg: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn ca(hex_rgba: u32) -> Hsla {
    rgba(hex_rgba).into()
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            dark: true,
            canvas: c(0x08090c),
            chrome: c(0x12161c),
            surface: c(0x161b22),
            raised: c(0x1f2630),
            hover: c(0x242c38),
            selected: c(0x22304a),
            line: c(0x262e3a),
            strong: c(0x3a4554),
            text: c(0xe3e8ef),
            muted: c(0xa3adba),
            dim: c(0x6f7a89),
            accent: c(0x6ea0ff),
            accent_fg: c(0x0b1220),
            working: c(0xf0b04a),
            blocked: c(0xff6b6b),
            done: c(0x4cc38a),
            anon: c(0xb89cff),
            backdrop: ca(0x0000_00a0),
            paper: Paper {
                card: c(0xfbfbf8),
                edge: c(0xffffff),
                well: c(0xeeede7),
                grid: c(0xe1e0d9),
                hover: c(0xf1f0eb),
                text: c(0x0b0b0b),
                muted: c(0x52514e),
                dim: c(0x86847d),
                link: c(0x1f63b5),
                quote: c(0xb5b3ab),
                warn_bg: c(0xfdf3df),
                warn: c(0x9a6400),
                bad: c(0xb3261e),
                bad_bg: c(0xfbeae8),
                good: c(0x1e7a46),
                code_bg: c(0xf1f0ea),
            },
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            canvas: c(0xe6e5df),
            chrome: c(0xf1f0eb),
            surface: c(0xf8f8f5),
            raised: c(0xffffff),
            hover: c(0xe8e7e1),
            selected: c(0xdce6f8),
            line: c(0xdcdad3),
            strong: c(0xc4c1b8),
            text: c(0x17191c),
            muted: c(0x52514e),
            dim: c(0x86847d),
            accent: c(0x2a78d6),
            accent_fg: c(0xffffff),
            working: c(0xd98e00),
            blocked: c(0xd03b3b),
            done: c(0x0ca30c),
            anon: c(0x7c5cd6),
            backdrop: ca(0x1018_2860),
            paper: Paper {
                card: c(0xffffff),
                edge: c(0xdcdad3),
                well: c(0xf3f2ee),
                grid: c(0xe6e4dd),
                hover: c(0xf5f4f0),
                text: c(0x0b0b0b),
                muted: c(0x52514e),
                dim: c(0x86847d),
                link: c(0x1f63b5),
                quote: c(0xc4c1b8),
                warn_bg: c(0xfdf3df),
                warn: c(0x9a6400),
                bad: c(0xb3261e),
                bad_bg: c(0xfbeae8),
                good: c(0x1e7a46),
                code_bg: c(0xf1f0ea),
            },
        }
    }

    pub fn of(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// An account's colour (`#rrggbb` from the account), or the accent.
    pub fn account(&self, color: Option<&str>) -> Hsla {
        color
            .and_then(|s| u32::from_str_radix(s.trim_start_matches('#'), 16).ok())
            .map(c)
            .unwrap_or(self.accent)
    }

    /// Chip colours for a category: (foreground, background).
    pub fn category(&self, cat: Category) -> (Hsla, Hsla) {
        let base = match cat {
            Category::Important => if self.dark { c(0xff8a5c) } else { c(0xc4501f) },
            Category::Personal => if self.dark { c(0x7fb0ff) } else { c(0x1f63b5) },
            Category::Transactional => if self.dark { c(0x4fd1a5) } else { c(0x0e8a5f) },
            Category::Newsletter => if self.dark { c(0xc6adff) } else { c(0x6a48c4) },
            Category::Notification => if self.dark { c(0x6fd3e0) } else { c(0x137e8c) },
            Category::Promotion | Category::Other => self.muted,
            Category::Sensitive => self.working,
            Category::Spam => self.blocked,
        };
        (base, base.opacity(if self.dark { 0.16 } else { 0.12 }))
    }
}

pub fn category_label(cat: Category) -> &'static str {
    match cat {
        Category::Important => "Important",
        Category::Personal => "People",
        Category::Transactional => "Receipts",
        Category::Newsletter => "Newsletter",
        Category::Promotion => "Promotion",
        Category::Notification => "Notification",
        Category::Sensitive => "Sensitive",
        Category::Spam => "Spam",
        Category::Other => "Other",
    }
}

/// The UI font: Inter where it is installed, else whatever GPUI resolves the system font to.
pub const UI_FONT: &str = "Inter";

/// Sans fonts to fall back to on Linux when Inter cannot show weights.
const LINUX_SANS: &[&str] = &["Noto Sans", "Ubuntu", "Cantarell", "Liberation Sans", "DejaVu Sans"];

/// The UI font to use, if not the platform default. GPUI's Linux text system draws a variable font
/// (the usual way Inter is installed now) at one weight only, which would lose bold text and the
/// unread rows; there we use Inter only when it has a separate bold face, else a sans that does.
pub fn pick_ui_font(cx: &App) -> Option<&'static str> {
    let ts = cx.text_system();
    let installed = ts.all_font_names();
    let has = |f: &str| installed.iter().any(|n| n == f);
    if cfg!(target_os = "macos") {
        return has(UI_FONT).then_some(UI_FONT);
    }
    let weights = |f: &str| {
        let regular = gpui_kit::font(f);
        let bold = gpui_kit::Font { weight: gpui_kit::FontWeight::BOLD, ..regular.clone() };
        ts.resolve_font(&regular) != ts.resolve_font(&bold)
    };
    std::iter::once(UI_FONT).chain(LINUX_SANS.iter().copied()).find(|f| has(f) && weights(f))
}

/// Make gpui-component's controls (inputs, palette, buttons) match the palette.
pub fn apply(dark: bool, cx: &mut App) {
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    native_appearance(dark);
    let p = Palette::of(dark);
    let ui_font = pick_ui_font(cx);
    Theme::update(cx, |t| {
        if let Some(f) = ui_font {
            t.font_family = f.into();
        }
        t.font_size = px(14.);
        t.radius = px(6.);
        t.radius_lg = px(10.);
        let k = &mut t.colors;
        k.background = p.surface;
        k.foreground = p.text;
        k.muted = p.raised;
        k.muted_foreground = p.dim;
        k.border = p.line;
        k.input = p.strong;
        k.ring = p.accent;
        k.caret = p.accent;
        k.selection = p.accent.opacity(0.3);
        k.popover = if dark { c(0x1a2029) } else { c(0xffffff) };
        k.popover_foreground = p.text;
        k.primary = p.accent;
        k.primary_hover = p.accent.opacity(0.9);
        k.primary_active = p.accent.opacity(0.8);
        k.primary_foreground = p.accent_fg;
        k.button_primary = p.accent;
        k.button_primary_hover = p.accent.opacity(0.9);
        k.button_primary_active = p.accent.opacity(0.8);
        k.button_primary_foreground = p.accent_fg;
        k.secondary = p.raised;
        k.secondary_hover = p.hover;
        k.secondary_active = p.selected;
        k.secondary_foreground = p.text;
        k.accent = p.hover;
        k.accent_foreground = p.text;
        k.list = p.surface;
        k.list_hover = p.hover;
        k.list_active = p.selected;
        k.list_active_border = p.accent;
        k.skeleton = p.raised;
        k.overlay = p.backdrop;
        k.title_bar = p.chrome;
        k.status_bar = p.chrome;
    });
}

/// The window frame AppKit draws (the native titlebar, traffic lights, menus, sheets) follows the app's
/// appearance, not just the system's: without this a dark app under a light system has a light titlebar.
/// Only on the main thread (GPUI's app thread); elsewhere it does nothing.
#[cfg(target_os = "macos")]
fn native_appearance(dark: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication};
    let Some(mtm) = MainThreadMarker::new() else { return };
    // SAFETY: AppKit's constant appearance names, valid for the life of the process.
    let name = unsafe { if dark { NSAppearanceNameDarkAqua } else { NSAppearanceNameAqua } };
    let appearance = NSAppearance::appearanceNamed(name);
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
}

#[cfg(not(target_os = "macos"))]
fn native_appearance(_dark: bool) {}

