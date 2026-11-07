use std::ops::Range;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, Div, HighlightStyle, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, StyledText, UnderlineStyle, div, px, relative,
};

use crate::app::*;
use crate::data::*;
use crate::icons::Icon;
use crate::shell::Bp;
use crate::theme as t;
use crate::ui::*;

const VU_BARS: usize = 8;

impl Workbench {
    fn marked_text(&self, (text, marks): (String, Vec<(Range<usize>, Mark)>)) -> StyledText {
        let term_line = UnderlineStyle {
            thickness: px(1.5),
            color: Some(t::accent_dim()),
            wavy: false,
        };
        let prov_line = UnderlineStyle {
            thickness: px(1.),
            color: Some(t::surface_3()),
            wavy: false,
        };
        let hl: Vec<(Range<usize>, HighlightStyle)> = marks
            .into_iter()
            .map(|(r, m)| {
                let style = match m {
                    Mark::Term => HighlightStyle {
                        underline: Some(term_line),
                        ..Default::default()
                    },
                    Mark::Provisional | Mark::ProvisionalTerm => HighlightStyle {
                        color: Some(t::text_3()),
                        underline: Some(prov_line),
                        ..Default::default()
                    },
                };
                (r, style)
            })
            .collect();
        StyledText::new(text).with_highlights(hl)
    }

    /// The words still being heard, under the settled lines. The recogniser
    /// may still change its last word, so that one reads as provisional.
    pub(crate) fn heard_line(&self, bp: Bp) -> Option<AnyElement> {
        let line = self.live.partial.as_ref()?;
        let n = line.orig.len().saturating_sub(1);
        let text = join_tokens(&line.orig, n, line.orig_cjk, true);
        Some(
            row()
                .items_start()
                .gap(px(12.))
                .px(bp.pad_x())
                .py(px(12.))
                .child(
                    mono(div())
                        .w(px(64.))
                        .flex_none()
                        .pt(px(2.))
                        .text_size(px(12.5))
                        .text_color(t::accent())
                        .child(mmss(self.live.live_ms())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(15.))
                        .line_height(relative(1.55))
                        .text_color(t::text_2())
                        .child(self.marked_text(text)),
                )
                .into_any_element(),
        )
    }

    /// Shown above a project while it records.
    pub(crate) fn live_strip(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let running = self.live.status == LiveStatus::Running;
        let level = self.live.level.sqrt().min(1.0);
        let vu = row()
            .flex_none()
            .items_end()
            .gap(px(2.))
            .w(px(40.))
            .h(px(16.))
            .children((0..VU_BARS).map(|i| {
                let lit = level * VU_BARS as f32;
                let bar = div().flex_1().rounded(px(1.));
                if running && (i as f32) < lit.ceil() {
                    let fill = (lit - i as f32).clamp(0.0, 1.0);
                    bar.h(relative(0.25 + 0.75 * fill)).bg(t::accent_dim())
                } else {
                    bar.h(relative(0.15)).bg(t::surface_3())
                }
            }));
        let note = self
            .live
            .engine_note
            .clone()
            .unwrap_or_else(|| self.source_label());

        self.strip(bp)
            .child(if running {
                pulse_dot("rec-dot", t::accent(), 8.)
            } else {
                div().size(px(8.)).rounded_full().bg(t::warn()).into_any_element()
            })
            .child(Self::strip_title(if running { "Recording" } else { "Paused" }))
            .child(
                mono(div())
                    .flex_none()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(mmss(self.live.live_ms())),
            )
            .child(vu)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_3())
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .when(!bp.mobile(), |d| d.child(note)),
            )
            .child(if running {
                button("live-pause", Btn::Quiet, Some(Icon::Pause), Some("Pause"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.pause_live(cx)))
            } else {
                button("live-resume", Btn::Quiet, Some(Icon::Play), Some("Resume"), false)
                    .on_click(cx.listener(|this, _, w, cx| this.start_live(w, cx)))
            })
            .child(
                button("live-stop", Btn::Ghost, Some(Icon::Stop), Some("Stop"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.stop_live(cx))),
            )
            .into_any_element()
    }

    /// "Spoken in", then an opt-in translation and its language.
    pub(crate) fn lang_pair(&self, from: Sel, to: Sel, disabled: bool, cx: &mut Context<Self>) -> Div {
        let on = self.settings.translate;
        let from_sel = div().flex_1().min_w_0().child(field("Spoken in", self.render_select(from, px(0.), disabled, cx)));
        col()
            .gap(px(12.))
            .child(
                row()
                    .items_end()
                    .gap(px(8.))
                    .child(from_sel)
                    .when(on && from == Sel::LiveFrom, |d| {
                        d.child(
                            button("live-swap", Btn::Icon, Some(Icon::Swap), None, disabled).when(!disabled, |d| {
                                d.on_click(cx.listener(|this, _, _, cx| this.swap_langs(cx)))
                            }),
                        )
                    })
                    .when(on, |d| {
                        d.child(div().flex_1().min_w_0().child(field(
                            "Translate to",
                            self.render_select(to, px(0.), disabled, cx),
                        )))
                    }),
            )
            .child(
                switch(SharedString::from(format!("translate-{from:?}")), on, "Translate the lines")
                    .when(!disabled, |d| d.on_click(cx.listener(|this, _, _, cx| this.toggle_translate_default(cx)))),
            )
    }

    pub(crate) fn capture_body(&self, cx: &mut Context<Self>) -> Div {
        let idle = self.live.status == LiveStatus::Idle;
        let sources = segmented().children([Source::System, Source::Mic].map(|s| {
            let ic = if s == Source::Mic { Icon::Mic } else { Icon::Monitor };
            seg_button(
                SharedString::from(format!("src-{s:?}")),
                Some(ic),
                s.label(),
                self.live.source == s,
                !idle,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.set_source(s, cx)))
        }));
        let device = if self.live.source == Source::Mic { "Microphone" } else { "Sound from" };
        col()
            .gap(px(16.))
            .child(field("Listen to", sources))
            .child(field(device, self.render_select(Sel::SourceDetail, px(200.), !idle, cx)))
            .child(self.lang_pair(Sel::LiveFrom, Sel::LiveTo, !idle, cx))
            .when(!self.settings.asr_ready(), |d| {
                d.child(
                    row()
                        .flex_wrap()
                        .text_size(t::T_SMALL)
                        .text_color(t::warn_text())
                        .child("Speech recognition is not set up yet.\u{00A0}")
                        .child(
                            link_button("cap-engines", "Set up engines")
                                .on_click(cx.listener(|this, _, w, cx| this.open_engines(w, cx))),
                        ),
                )
            })
    }
}
