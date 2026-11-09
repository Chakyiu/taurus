//! Controls from the design's component sheet: buttons, chips, pills, fields,
//! segmented controls, switches, checkboxes, radio rows and tiles, cards.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, CursorStyle, Div, ElementId, Hsla,
    InteractiveElement, IntoElement, ParentElement, ScrollHandle, SharedString, Stateful, Styled,
    div, px, relative,
};
use gpui_component::scroll::ScrollableElement as _;

use crate::icons::{Icon, icon};
use crate::theme::{self as t, weight};

pub fn row() -> Div {
    div().flex().flex_row().items_center()
}

pub fn col() -> Div {
    div().flex().flex_col()
}

pub fn fill() -> Div {
    div().flex_1().min_w_0()
}

pub fn mono(d: Div) -> Div {
    d.font_family(t::MONO)
}

pub fn label(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(t::T_SMALL)
        .font_weight(weight(520.))
        .text_color(t::text_2())
        .child(text.into())
}

pub fn field(label_text: impl Into<SharedString>, control: impl IntoElement) -> Div {
    col()
        .gap(px(6.))
        .min_w_0()
        .child(label(label_text))
        .child(control)
}

pub fn hint(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(t::T_SMALL)
        .text_color(t::text_3())
        .child(text.into())
}

pub fn err_text(text: impl Into<SharedString>) -> Div {
    row()
        .items_start()
        .gap(px(5.))
        .text_size(t::T_SMALL)
        .text_color(t::danger_text())
        .child(div().text_size(px(9.)).pt(px(3.)).child("▲"))
        .child(div().child(text.into()))
}

// ─── buttons ───────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// Text only, no frame until hovered.
    Quiet,
    Ghost,
    Primary,
    Danger,
    DangerGhost,
    Icon,
    IconDanger,
}

pub fn button(
    id: impl Into<ElementId>,
    kind: Btn,
    ic: Option<Icon>,
    text: Option<&str>,
    disabled: bool,
) -> Stateful<Div> {
    let icon_only = matches!(kind, Btn::Icon | Btn::IconDanger);
    let (bg, border, fg, hover_bg, hover_border, hover_fg) = match kind {
        Btn::Quiet => (
            gpui::transparent_black(),
            gpui::transparent_black(),
            t::text_2(),
            t::surface_2(),
            t::surface_2(),
            t::text(),
        ),
        Btn::Ghost | Btn::Icon => (
            gpui::transparent_black(),
            t::line(),
            t::text_2(),
            t::surface_2(),
            t::surface_3(),
            t::text(),
        ),
        Btn::Primary => (
            t::accent(),
            t::accent(),
            t::accent_ink(),
            t::accent_hover(),
            t::accent_hover(),
            t::accent_ink(),
        ),
        Btn::Danger => (
            t::danger(),
            t::danger(),
            t::danger_ink(),
            t::danger_hover(),
            t::danger_hover(),
            t::danger_ink(),
        ),
        Btn::DangerGhost | Btn::IconDanger => (
            gpui::transparent_black(),
            t::line(),
            t::danger(),
            t::danger_wash(),
            t::danger_wash(),
            t::danger_text(),
        ),
    };
    let fw = match kind {
        Btn::Primary | Btn::Danger => 600.,
        _ => 540.,
    };
    let (bg, border, fg) = if disabled {
        (t::surface_2(), gpui::transparent_black(), t::text_3())
    } else {
        (bg, border, fg)
    };
    let group: SharedString = "btn".into();
    let icon_size = px(16.);

    let mut b = div()
        .id(id)
        .group(group.clone())
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(7.))
        .h(px(36.))
        .rounded(t::R)
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(t::T_CONTROL)
        .font_weight(weight(fw))
        .whitespace_nowrap()
        .when(icon_only, |d| d.w(px(36.)))
        .when(!icon_only, |d| d.px(px(if kind == Btn::Quiet { 10. } else { 14. })));

    if disabled {
        b = b.opacity(0.65).cursor(CursorStyle::OperationNotAllowed);
    } else {
        b = b.cursor_pointer().hover(move |s| {
            s.bg(hover_bg)
                .border_color(hover_border)
                .text_color(hover_fg)
        });
    }

    if let Some(i) = ic {
        let mut svg = icon(i, icon_size, fg);
        if !disabled {
            svg = svg.group_hover(group, move |s| s.text_color(hover_fg));
        }
        b = b.child(svg);
    }
    if let Some(text) = text {
        b = b.child(text.to_string());
    }
    b
}

pub fn link_button(id: impl Into<ElementId>, text: &str) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .text_color(t::accent())
        .underline()
        .hover(|s| s.text_color(t::link_hover()))
        .child(text.to_string())
}

// ─── chips, pills, flags ───────────────────────────────────

pub fn pulse_dot(id: impl Into<ElementId>, color: Hsla, size: f32) -> AnyElement {
    div()
        .size(px(size))
        .rounded_full()
        .bg(color)
        .flex_none()
        .with_animation(
            id,
            Animation::new(Duration::from_millis(1300))
                .repeat()
                .with_easing(gpui::bounce(gpui::ease_out_quint())),
            |d, delta| d.opacity(1.0 - 0.7 * delta),
        )
        .into_any_element()
}

// ─── progress ──────────────────────────────────────────────

/// A track still being captured has no completion to report yet: the bar
/// says "running", not a percentage it would have to invent.
pub fn running_bar(id: impl Into<ElementId>) -> Div {
    div()
        .relative()
        .w_full()
        .h(px(3.))
        .rounded_full()
        .bg(t::surface_3())
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .top_0()
                .h_full()
                .w(relative(0.38))
                .rounded_full()
                .bg(t::accent())
                .with_animation(
                    id,
                    Animation::new(Duration::from_millis(1500))
                        .repeat()
                        .with_easing(gpui::ease_out_quint()),
                    |d, delta| d.left(relative(-0.418 + delta * 1.52)),
                ),
        )
}

// ─── segmented, switch, checkbox, radios ───────────────────

pub fn segmented() -> Div {
    row()
        .flex_none()
        .p(px(2.))
        .gap(px(2.))
        .border_1()
        .border_color(t::line())
        .rounded(t::R)
        .bg(t::surface())
}

pub fn seg_button(
    id: impl Into<ElementId>,
    ic: Option<Icon>,
    text: &str,
    pressed: bool,
    disabled: bool,
) -> Stateful<Div> {
    let fg = if disabled && !pressed {
        t::text_3()
    } else if pressed {
        t::text()
    } else {
        t::text_2()
    };
    row()
        .id(id)
        .gap(px(6.))
        .h(px(30.))
        .px(px(12.))
        .rounded(t::R_SM)
        .text_size(px(14.))
        .text_color(fg)
        .whitespace_nowrap()
        .when(pressed, |d| d.bg(t::surface_3()).font_weight(weight(550.)))
        .when(!pressed && !disabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.bg(t::surface_2()).text_color(t::text()))
        })
        .when(disabled, |d| d.cursor(CursorStyle::OperationNotAllowed))
        .when_some(ic, |d, ic| d.child(icon(ic, px(15.), if pressed { t::accent() } else { fg })))
        .child(text.to_string())
}

pub fn switch(id: impl Into<ElementId>, on: bool, text: &str) -> Stateful<Div> {
    row()
        .id(id)
        .flex_none()
        .gap(px(7.))
        .cursor_pointer()
        .child(
            div()
                .relative()
                .w(px(30.))
                .h(px(17.))
                .rounded_full()
                .bg(if on { t::accent_dim() } else { t::surface_3() })
                .child(
                    div()
                        .absolute()
                        .top(px(2.5))
                        .left(px(if on { 15.5 } else { 2.5 }))
                        .size(px(12.))
                        .rounded_full()
                        .bg(if on { t::accent() } else { t::text_2() }),
                ),
        )
        .child(
            div()
                .text_size(t::T_SMALL)
                .text_color(if on { t::text() } else { t::text_2() })
                .whitespace_nowrap()
                .child(text.to_string()),
        )
}

pub fn check_box(on: bool) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .size(px(17.))
        .rounded(px(4.))
        .border_1()
        .map(|d| {
            if on {
                d.bg(t::accent()).border_color(t::accent())
            } else {
                d.bg(t::surface()).border_color(t::line())
            }
        })
        .when(on, |d| d.child(icon(Icon::Check, px(12.), t::accent_ink())))
}

pub fn checkbox_row(id: impl Into<ElementId>, on: bool, text: &str) -> Stateful<Div> {
    row()
        .id(id)
        .gap(px(9.))
        .cursor_pointer()
        .text_size(t::T_CONTROL)
        .text_color(if on { t::text() } else { t::text_2() })
        .child(check_box(on))
        .child(text.to_string())
}

fn radio_mark(on: bool) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .mt(px(3.))
        .size(px(15.))
        .rounded_full()
        .border_1()
        .border_color(if on { t::accent() } else { t::line() })
        .bg(t::surface())
        .when(on, |d| {
            d.child(div().size(px(8.)).rounded_full().bg(t::accent()))
        })
}

pub fn radio_row(
    id: impl Into<ElementId>,
    on: bool,
    title: &str,
    sub: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_start()
        .gap(px(10.))
        .px(px(11.))
        .py(px(9.))
        .rounded(t::R)
        .border_1()
        .cursor_pointer()
        .map(|d| {
            if on {
                d.border_color(t::accent_dim()).bg(t::accent_wash())
            } else {
                d.border_color(t::line())
                    .bg(t::surface())
                    .hover(|s| s.border_color(t::surface_3()).bg(t::surface_2()))
            }
        })
        .child(radio_mark(on))
        .child(
            col()
                .flex_1()
                .gap(px(2.))
                .min_w_0()
                .child(
                    div()
                        .text_size(t::T_CONTROL)
                        .font_weight(weight(550.))
                        .text_color(t::text())
                        .child(title.to_string()),
                )
                .child(sub),
        )
}

pub fn radio_sub(text: &str) -> Div {
    div()
        .text_size(t::T_SMALL)
        .text_color(t::text_2())
        .line_height(relative(1.5))
        .child(text.to_string())
}

// ─── scrolling ─────────────────────────────────────────────

/// Lays the scroll bar over the right edge of `area`, which scrolls and
/// tracks `handle`; the bar shows only when there is more to see. The column
/// returned stands in for `area` in the layout, so size it as `area` was.
pub fn with_scroll_bar(id: impl Into<ElementId>, area: impl IntoElement, handle: &ScrollHandle) -> Stateful<Div> {
    col().id(id).relative().child(area).vertical_scrollbar(handle)
}

// ─── text blocks ───────────────────────────────────────────

pub fn empty_state(title: &str, body: impl IntoElement, actions: Vec<AnyElement>) -> Div {
    col().w_full().items_center().py(px(54.)).child(
        col()
            .max_w(px(440.))
            .items_center()
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(25.))
                    .font_weight(weight(620.))
                    .line_height(relative(1.2))
                    .text_center()
                    .child(title.to_string()),
            )
            .child(
                div()
                    .text_size(px(15.))
                    .text_color(t::text_2())
                    .line_height(relative(1.65))
                    .text_center()
                    .max_w(px(400.))
                    .child(body),
            )
            .when(!actions.is_empty(), |d| {
                d.child(
                    row()
                        .flex_wrap()
                        .justify_center()
                        .gap(px(8.))
                        .mt(px(8.))
                        .children(actions),
                )
            }),
    )
}

