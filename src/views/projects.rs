use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use super::screen;
use crate::app::*;
use crate::data::*;
use crate::icons::Icon;
use crate::shell::Bp;
use crate::theme::{self as t, weight};
use crate::ui::*;

/// Search only earns its place once the list no longer fits on a screen.
const SEARCH_FROM: usize = 8;

impl Workbench {
    pub(crate) fn render_projects(
        &mut self,
        bp: Bp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.projects.is_empty() {
            return screen()
                .child(empty_state(
                    "No projects yet",
                    "Caption something playing on this Mac, pull the subtitles out of a video, or import a subtitle file. You can also drop files on this window.",
                    vec![
                        button("pj-e-live", Btn::Primary, Some(Icon::Waveform), Some("Live caption"), false)
                            .on_click(cx.listener(|this, _, _, cx| this.open_capture(cx)))
                            .into_any_element(),
                        button("pj-e-video", Btn::Ghost, Some(Icon::Film), Some("From a video"), false)
                            .on_click(cx.listener(|this, _, _, cx| this.browse_video(cx)))
                            .into_any_element(),
                        button("pj-e-import", Btn::Ghost, Some(Icon::Upload), Some("Import subtitles"), false)
                            .on_click(cx.listener(|this, _, _, cx| this.browse_subtitles(cx)))
                            .into_any_element(),
                    ],
                ))
                .into_any_element();
        }

        let q = self.pj_search.read(cx).value().trim().to_lowercase();
        let rows: Vec<AnyElement> = self
            .projects
            .iter()
            .filter(|p| {
                q.is_empty()
                    || format!("{} {} {}", p.name, p.source, lang_name(p.out_lang()))
                        .to_lowercase()
                        .contains(&q)
            })
            .map(|p| self.project_row(p, bp, cx))
            .collect();
        let none_match = rows.is_empty();

        let search = (self.projects.len() >= SEARCH_FROM).then(|| {
            div()
                .w(px(260.))
                .mb(px(8.))
                .child(self.text_field(&self.pj_search, Some(Icon::Search), false, window, cx))
        });

        screen()
            .child(
                div()
                    .id("pj-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(bp.pad_x())
                    .pt(px(16.))
                    .pb(px(32.))
                    .child(
                        col()
                            .w_full()
                            .max_w(px(960.))
                            .mx_auto()
                            .children(search)
                            .children(rows)
                            .when(none_match, |d| {
                                d.child(hint("Nothing matches that search.").py(px(20.)))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn project_row(&self, p: &Project, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let pid = p.id.clone();
        let group = SharedString::from(format!("proj-{pid}"));
        let failed = p.status == Status::Failed;
        let meta = if failed {
            p.error.clone().unwrap_or_else(|| "Failed".into())
        } else {
            let mut parts = vec![
                p.kind.label().to_string(),
                if p.translate {
                    format!("{} → {}", lang_name(&p.from), lang_name(&p.to))
                } else {
                    lang_name(&p.from)
                },
            ];
            if !p.segs.is_empty() {
                parts.push(self.track_len(p));
            }
            parts.push(p.when());
            parts.join(" · ")
        };
        let (status, color) = self.status_text(p);
        let recording = p.status == Status::Live
            && !(self.live.project_id.as_deref() == Some(p.id.as_str())
                && self.live.status == LiveStatus::Paused);

        let (open_id, del_id) = (pid.clone(), pid.clone());
        let delete = button(
            SharedString::from(format!("del-{pid}")),
            Btn::IconDanger,
            Some(Icon::Trash),
            None,
            false,
        )
        .when(!bp.mobile(), |d| d.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
        .on_click(cx.listener(move |this, _, w, cx| {
            cx.stop_propagation();
            this.delete_project(&del_id, w, cx)
        }));

        row()
            .id(SharedString::from(format!("proj-{pid}")))
            .group(group)
            .gap(px(14.))
            .px(px(12.))
            .py(px(11.))
            .rounded(t::R)
            .cursor_pointer()
            .hover(|s| s.bg(t::surface()))
            .on_click(cx.listener(move |this, _, w, cx| this.open_project(&open_id, w, cx)))
            .child(
                col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(weight(550.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(p.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(t::T_SMALL)
                            .text_color(if failed { t::danger_text() } else { t::text_2() })
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(meta),
                    ),
            )
            .child(
                row()
                    .flex_none()
                    .gap(px(7.))
                    .text_size(t::T_SMALL)
                    .text_color(color)
                    .whitespace_nowrap()
                    .when(recording, |d| {
                        d.child(pulse_dot(SharedString::from(format!("rec-{pid}")), t::accent(), 7.))
                    })
                    .child(status),
            )
            .child(delete)
            .into_any_element()
    }
}
