//! Small drawing helpers shared by the panes.

use crate::theme::Palette;
use gpui::prelude::*;
use gpui::*;
use lumbridge_mail_core::{Account, Persona};
use std::rc::Rc;

pub fn h_row() -> Div {
    div().flex().flex_row().items_center()
}

pub fn v_col() -> Div {
    div().flex().flex_col()
}

/// A Lucide icon from gpui-kit's assets (`icons/<name>.svg`).
pub fn icon(name: &str, size: f32, color: Hsla) -> Svg {
    svg().path(SharedString::from(format!("icons/{name}.svg"))).size(px(size)).flex_none().text_color(color)
}

pub fn dot(color: Hsla, size: f32) -> Div {
    div().size(px(size)).flex_none().rounded_full().bg(color)
}

/// "Anon" or "Work" etc. next to an identity.
pub fn persona_tag(p: &Palette, a: &Account) -> Div {
    let (text, color) = match &a.persona {
        Persona::Personal => ("Personal".to_string(), p.dim),
        Persona::Work => ("Work".to_string(), p.dim),
        Persona::Anon => ("Anon".to_string(), p.anon),
        Persona::Other(s) => (s.clone(), p.dim),
    };
    div()
        .px_1p5()
        .rounded_sm()
        .border_1()
        .border_color(color.opacity(0.5))
        .text_size(px(10.5))
        .text_color(color)
        .child(text)
}

pub fn chip(text: impl Into<SharedString>, fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(7.))
        .h(px(18.))
        .flex()
        .items_center()
        .rounded(px(9.))
        .bg(bg)
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(text.into())
}

/// Keys as small boxes ("G" "I").
pub fn keys_hint(p: &Palette, keys: &[String]) -> Div {
    h_row().gap_0p5().children(keys.iter().map(|k| {
        div()
            .px_1()
            .min_w(px(18.))
            .h(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .border_1()
            .border_color(p.strong)
            .bg(p.raised)
            .text_size(px(10.5))
            .text_color(p.muted)
            .child(k.clone())
    }))
}

/// Initials in a coloured circle.
pub fn avatar(name: &str, color: Hsla, size: f32) -> Div {
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .collect::<String>()
        .to_uppercase();
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(color.opacity(0.22))
        .text_size(px(size * 0.4))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(if initials.is_empty() { "?".to_string() } else { initials })
}

/// A stable colour per sender, from the categorical order.
pub fn person_color(email: &str) -> Hsla {
    const ORDER: [u32; 6] = [0x2a78d6, 0xeb6834, 0x1baf7a, 0x8a5cd6, 0xc23f7a, 0x2f9bb0];
    let h = email.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
    rgb(ORDER[(h % ORDER.len() as u32) as usize]).into()
}

/// Two or three options side by side, one on.
pub fn segmented(
    p: &Palette,
    id: &'static str,
    options: &[&'static str],
    selected: usize,
    on_change: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let on_change = Rc::new(on_change);
    div()
        .id(id)
        .flex()
        .p(px(2.))
        .gap(px(2.))
        .rounded_md()
        .bg(p.raised)
        .border_1()
        .border_color(p.line)
        .children(options.iter().enumerate().map(|(i, label)| {
            let on_change = on_change.clone();
            div()
                .id(SharedString::from(format!("{id}-{i}")))
                .px_2()
                .h(px(22.))
                .flex()
                .items_center()
                .rounded_sm()
                .text_xs()
                .when(i == selected, |d| d.bg(p.selected).text_color(p.text))
                .when(i != selected, |d| d.text_color(p.dim).hover(|s| s.text_color(p.text)))
                .child(*label)
                .on_click(move |_, window, cx| on_change(&i, window, cx))
        }))
}

/// A little button for the paper card (gpui-kit's buttons follow the chrome theme, the card does not).
pub fn paper_button(p: &Palette, id: impl Into<ElementId>, glyph: &str, label: Option<&str>) -> Stateful<Div> {
    let pp = p.paper;
    div()
        .id(id.into())
        .flex()
        .items_center()
        .gap_1p5()
        .h(px(28.))
        .px(px(if label.is_some() { 10. } else { 7. }))
        .rounded_md()
        .border_1()
        .border_color(pp.grid)
        .bg(pp.card)
        .text_sm()
        .text_color(pp.muted)
        .hover(|s| s.bg(pp.hover).text_color(pp.text))
        .child(icon(glyph, 14., pp.muted))
        .when_some(label.map(str::to_owned), |d, l| d.child(l))
}
