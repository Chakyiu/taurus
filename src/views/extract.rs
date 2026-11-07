use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, Div, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, div, px,
};

use crate::app::*;
use crate::shell::Bp;
use crate::theme::{self as t, weight};
use crate::ui::*;

impl Workbench {
    /// Which track of the picked video to use, and in which languages.
    pub(crate) fn extract_body(&self, _bp: Bp, cx: &mut Context<Self>) -> Div {
        let ex = &self.extract;
        let name = ex.file.as_ref().map(|f| f.name.clone()).unwrap_or_default();
        let mut body = col().gap(px(16.)).child(
            col()
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(weight(550.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(name),
                )
                .child(
                    mono(div())
                        .text_size(t::T_SMALL)
                        .text_color(t::text_2())
                        .child(ex.file_meta()),
                ),
        );

        if crate::media::tool("ffmpeg").is_none() {
            return body.child(err_text(
                "ffmpeg is not installed, so videos cannot be read yet. Install it with `brew install ffmpeg`.",
            ));
        }
        if let Some(e) = ex.scan_error.clone() {
            return body.child(err_text(format!("Taurus could not read this file: {e}.")));
        }
        if !ex.scanned() {
            return body.child(
                col()
                    .gap(px(8.))
                    .child(running_bar("ex-scan"))
                    .child(hint("Reading the file…")),
            );
        }

        let tracks = col().gap(px(6.)).children(ex.tracks().into_iter().enumerate().map(|(ix, r)| {
            let row = radio_row(
                SharedString::from(format!("track-{ix}")),
                r.key.is_some() && ex.track == r.key,
                &r.title,
                mono(div()).text_size(t::T_SMALL).text_color(t::text_2()).child(r.sub),
            );
            match r.key {
                Some(key) => row.on_click(cx.listener(move |this, _, _, cx| {
                    this.select_track(key);
                    cx.notify();
                })),
                None => row.opacity(0.55).cursor_default(),
            }
        }));
        body = body.child(field("Use", tracks)).child(self.lang_pair(Sel::ExFrom, Sel::ExTo, false, cx));
        body
    }

    /// Shown above the new project while its video is read.
    pub(crate) fn extract_strip(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let ex = &self.extract;
        let stage = ex
            .stages
            .iter()
            .find(|s| s.state == StageState::Run)
            .or_else(|| ex.stages.iter().find(|s| s.state == StageState::Wait));
        let (title, note) = match stage {
            Some(s) => (s.label.clone(), s.note.clone()),
            None => ("Finishing".into(), String::new()),
        };
        let step = ex
            .stages
            .iter()
            .position(|s| s.state != StageState::Done)
            .unwrap_or(ex.stages.len());
        self.strip(bp)
            .child(Self::strip_title(title))
            .child(
                mono(div())
                    .flex_none()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(format!("step {} of {}", (step + 1).min(ex.stages.len()), ex.stages.len())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(60.))
                    .child(running_bar("ex-run"))
                    .when(!note.is_empty(), |d| {
                        d.child(
                            mono(div())
                                .pt(px(4.))
                                .text_size(px(12.))
                                .text_color(t::text_3())
                                .child(note),
                        )
                    }),
            )
            .child(
                button("ex-cancel", Btn::Quiet, None, Some("Cancel"), false)
                    .on_click(cx.listener(|this, _, w, cx| this.cancel_extract(w, cx))),
            )
            .into_any_element()
    }
}
