//! Lumbridge Mail's desktop app (`lumbridge-mail`). See README.md for how it is put together.
//!
//! - [`backend`]: what the window talks to: the [`backend::Backend`] trait, made-up mail
//!   ([`backend::demo::Demo`]) and the daemon client ([`backend::rpc::Rpc`]).
//! - [`app`]: the window's state and behaviour; [`ui`]: drawing; [`keymap`]: every key in one table.
//! - [`reading`]: bodies through `lumbridge-mail-render` (the block tree the reader paints);
//!   [`remote`]: the images it may load once asked.
//! - Desktop: [`settings`], [`notify`], [`files`] (attachments: copy in, open, save), [`mailto`], [`instance`] (one window), [`paths`], [`cli`].

// GPUI's derive macros name `gpui::`; through the Kit facade that is this alias.
extern crate gpui_kit as gpui;

pub mod app;
pub mod backend;
pub mod cli;
pub mod contacts;
pub mod files;
pub mod instance;
pub mod keymap;
pub mod mailto;
pub mod notify;
pub mod paths;
pub mod reading;
pub mod remote;
pub mod settings;
pub mod theme;
pub mod time;
pub mod ui;

/// The app's id: window class / Wayland app id, macOS bundle id, `.desktop` file name.
pub const APP_ID: &str = "com.lumbridgecorp.mail";

pub use app::{MailApp, Options, View};

/// Initialise gpui-kit and the app's key bindings. Call once, before opening a window.
pub fn init(cx: &mut gpui::App) {
    gpui_kit::init(cx);
    keymap::bind(cx);
}

/// The asset source the app draws with: gpui-kit's full Lucide catalog (the default bundle only carries
/// the icons gpui-kit's own components use).
pub fn assets() -> gpui_kit::assets::AllAssets {
    gpui_kit::assets::AllAssets
}

/// The main window's options.
pub fn window_options(cx: &gpui::App) -> gpui::WindowOptions {
    use gpui::*;
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1440.), px(900.)), cx))),
        titlebar: Some(TitlebarOptions { title: Some("Lumbridge Mail".into()), ..Default::default() }),
        app_id: Some(APP_ID.into()),
        window_min_size: Some(size(px(960.), px(600.))),
        ..Default::default()
    }
}
