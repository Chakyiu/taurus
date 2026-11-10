use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, ClickEvent, Context, Div, Focusable as _,
    HighlightStyle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, ParentElement, SharedString, StatefulInteractiveElement,
    StrikethroughStyle, Styled, StyledImage as _, StyledText, Window, canvas, div, img, px,
    relative, rgb, rgba,
};
use gpui_component::input::Input;

use super::screen;
use crate::app::*;
use crate::data::*;
use crate::icons::{Icon, icon};
use crate::shell::{Bp, ring_shadow};
use crate::theme::{self as t, weight};
use crate::ui::*;

const TIME_W: f32 = 64.;
/// The speaker colour beside every line, so a turn reads as one block.
const RAIL_W: f32 = 3.;

/// A control drawn on the picture. `on` is the mode it toggles.
fn video_mode_button(id: &'static str, ic: Icon, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(32.))
        .rounded(t::R)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(if on {
            rgba(0xffffff38)
        } else {
            rgba(0x05080acc)
        })
        .border_1()
        .border_color(if on {
            rgba(0xffffff73)
        } else {
            rgba(0xffffff24)
        })
        .hover(|s| s.bg(rgba(0xffffff4d)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(icon(ic, px(15.), rgb(0xffffff).into()))
}

impl Workbench {
    /// The one workspace a project has: its lines, whatever is still filling
    /// them (a capture or an extraction), and a way to fix any of them.
    pub(crate) fn render_editor(
        &mut self,
        bp: Bp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(p) = self.current() else {
            return screen()
                .child(empty_state(
                    "No project open",
                    "Pick one from the list, or start something new.",
                    vec![
                        button(
                            "ed-go-projects",
                            Btn::Primary,
                            None,
                            Some("Projects"),
                            false,
                        )
                        .on_click(
                            cx.listener(|this, _, w, cx| this.show_view(View::Projects, w, cx)),
                        )
                        .into_any_element(),
                    ],
                ))
                .into_any_element();
        };
        let pid = p.id.clone();
        let capturing = self.live.project_id.as_deref() == Some(pid.as_str())
            && self.live.status != LiveStatus::Idle;
        let extracting = self.extracting(&pid);
        let fixes = p.segs.iter().filter(|s| s.fix.is_some()).count();

        let strip = if capturing {
            Some(self.live_strip(bp, cx))
        } else if extracting {
            Some(self.extract_strip(bp, cx))
        } else if self.reprocess.as_ref().is_some_and(|j| j.pid == pid) {
            Some(self.reprocess_strip(bp, cx))
        } else if self.speaking.as_ref().is_some_and(|j| j.pid == pid)
            || self.speaker_pending.as_deref() == Some(pid.as_str())
        {
            Some(self.speaker_strip(bp, cx))
        } else if self.correcting.as_ref().is_some_and(|j| j.pid == pid) {
            Some(self.correct_strip(bp, cx))
        } else if fixes > 0 {
            Some(self.fixes_strip(fixes, bp, cx))
        } else if p.status == Status::Failed {
            Some(self.failed_strip(p, bp, cx))
        } else {
            None
        };

        let show_summary = self.summary_job.as_ref().is_some_and(|j| j.pid == pid)
            || self.current().is_some_and(|p| !p.summary.trim().is_empty());
        let show_video = self.video_file(p).is_some();
        let fill = show_video && self.video_app;
        let fullscreen = window.is_fullscreen();
        // The lines are hidden while the picture fills the window, so skip laying them out.
        let body = if fill {
            None
        } else if p.segs.is_empty() {
            let text = if capturing {
                self.live.engine_note.clone().unwrap_or_else(|| {
                    "Listening. Lines appear here as soon as someone speaks.".into()
                })
            } else if extracting {
                "Lines appear here as soon as the track is read.".into()
            } else {
                "This project has no lines.".into()
            };
            let partial = capturing.then(|| self.heard_line(bp)).flatten();
            Some(
                col()
                    .flex_1()
                    .px(bp.pad_x())
                    .children(partial)
                    .when(!capturing || self.live.partial.is_none(), |d| {
                        d.child(hint(text).py(px(24.)).text_center())
                    })
                    .into_any_element(),
            )
        } else {
            Some(self.line_list(bp, capturing, window, cx))
        };
        screen()
            .when(
                self.video_export.as_ref().is_some_and(|j| j.pid == pid),
                |d| d.child(self.video_export_strip(bp, cx)),
            )
            .children(strip)
            .when(!fill && !p.segs.is_empty() && !capturing, |d| {
                d.child(self.toolbar(bp, cx))
            })
            .when(!fill && show_summary && !capturing, |d| {
                d.child(self.summary_panel(bp, cx))
            })
            .when(show_video, |d| {
                d.child(self.video_stage(bp, fill, fullscreen, cx))
                    .children(self.audio_source(p).map(|_| self.transport(bp, cx)))
            })
            .children(body)
            .when(!show_video, |d| {
                d.children(self.audio_source(p).map(|_| self.transport(bp, cx)))
            })
            .into_any_element()
    }

    /// Fills the window under the top bar, or returns to the strip above the lines.
    fn toggle_video_app(&mut self, cx: &mut Context<Self>) {
        self.video_app = !self.video_app;
        cx.notify();
    }

    /// Sends the picture to the screen. The first press also fills the window;
    /// the next leaves that space and restores the strip.
    fn toggle_video_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.is_fullscreen() && self.video_screen {
            window.toggle_fullscreen();
            self.video_screen = false;
            self.video_app = false;
        } else if window.is_fullscreen() {
            self.video_app = true;
            self.video_screen = true;
        } else {
            self.video_app = true;
            self.video_screen = true;
            window.toggle_fullscreen();
        }
        cx.notify();
    }

    /// Escape leaves the enlarged picture, and the fullscreen space when this
    /// control opened it.
    pub(crate) fn leave_video_fit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.video_screen && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        self.video_app = false;
        self.video_screen = false;
        cx.notify();
    }

    /// The picture for this project, with the cue at the playhead drawn on it.
    fn video_stage(
        &self,
        bp: Bp,
        fill: bool,
        fullscreen: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let frame = self.video_frame.clone();
        let error = self.video.as_ref().and_then(|v| v.error());
        let (ms, playing) = self.playback_clock();
        let caption = self.current().and_then(|p| {
            let seg = p.segs.iter().find(|s| s.in_ms <= ms && ms < s.out_ms)?;
            crate::video::overlay_line(p.translate, &seg.orig, &seg.trans)
        });
        let can_play = self
            .current()
            .is_some_and(|p| self.audio_source(p).is_some());
        let h = if bp.mobile() { 188. } else { 300. };
        let screen_on = fullscreen && self.video_screen;
        div()
            .id("video-stage")
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x05080A))
            .overflow_hidden()
            .when(!fill, |d| {
                d.flex_none()
                    .mx(bp.pad_x())
                    .mt(px(8.))
                    .h(px(h))
                    .rounded(t::R_LG)
            })
            .when(fill, |d| d.flex_1().min_h_0())
            .when(can_play, |d| d.cursor_pointer())
            .on_click(cx.listener(|this, ev: &ClickEvent, window, cx| {
                if ev.click_count() >= 2 {
                    // The first click of the pair already toggled playback.
                    if this.player.is_some() {
                        this.play(None, cx);
                    }
                    this.toggle_video_screen(window, cx);
                    return;
                }
                if this
                    .current()
                    .is_some_and(|p| this.audio_source(p).is_some())
                {
                    this.play(None, cx);
                }
            }))
            .when_some(frame, |d, frame| {
                d.child(img(frame).flex_1().w_full().object_fit(ObjectFit::Contain))
            })
            .when(self.video_frame.is_none(), |d| {
                d.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .px(px(24.))
                        .text_size(t::T_SMALL)
                        .text_color(t::text_2())
                        .text_center()
                        .child(error.unwrap_or_else(|| "Loading the video…".into())),
                )
            })
            .when(self.video_frame.is_some() && can_play && !playing, |d| {
                d.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .size(px(52.))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(rgba(0x05080AB8))
                                .child(icon(Icon::Play, px(22.), rgb(0xffffff).into())),
                        ),
                )
            })
            .when_some(caption, |d, line| {
                d.child(
                    div()
                        .absolute()
                        .bottom(px(12.))
                        .left(px(16.))
                        .right(px(16.))
                        .child(
                            div()
                                .w_full()
                                .px(px(14.))
                                .py(px(7.))
                                .rounded(t::R)
                                .bg(rgba(0x05080AD9))
                                .text_center()
                                .when_some(line.secondary, |d, text| {
                                    d.child(
                                        div()
                                            .text_size(px(13.5))
                                            .line_height(relative(1.35))
                                            .text_color(rgb(0xC3D0DA))
                                            .child(text),
                                    )
                                })
                                .child(
                                    div()
                                        .text_size(px(18.))
                                        .font_weight(weight(550.))
                                        .line_height(relative(1.35))
                                        .text_color(rgb(0xffffff))
                                        .child(line.primary),
                                ),
                        ),
                )
            })
            .child(
                row()
                    .absolute()
                    .top(px(10.))
                    .right(px(10.))
                    .gap(px(6.))
                    .child(
                        video_mode_button("video-app", Icon::Frame, fill).on_click(cx.listener(
                            |this, ev: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                if ev.click_count() > 1 {
                                    return;
                                }
                                this.toggle_video_app(cx);
                            },
                        )),
                    )
                    .child(
                        video_mode_button("video-screen", Icon::Expand, screen_on).on_click(
                            cx.listener(|this, ev: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                if ev.click_count() > 1 {
                                    return;
                                }
                                this.toggle_video_screen(window, cx);
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn summary_panel(&self, bp: Bp, cx: &mut Context<Self>) -> Div {
        let Some(p) = self.current() else {
            return div();
        };
        let writing = self
            .summary_job
            .as_ref()
            .filter(|j| j.pid == p.id)
            .map(|j| {
                let step = match (j.done, j.parts) {
                    (_, 0 | 1) => String::new(),
                    (done, parts) if done < parts => format!(", part {} of {parts}", done + 1),
                    (_, parts) => format!(", joining the {parts} parts"),
                };
                format!(
                    "Writing a summary in {} with {}{step}…",
                    lang_name(&j.lang),
                    j.model
                )
            });
        let text = p.summary.trim().to_string();
        let meta = summary_meta(p);
        col()
            .flex_none()
            .mx(bp.pad_x())
            .mt(px(4.))
            .mb(px(4.))
            .px(px(14.))
            .py(px(12.))
            .rounded(t::R)
            .bg(t::surface())
            .border_1()
            .border_color(t::line())
            .gap(px(6.))
            .child(
                row()
                    .gap(px(14.))
                    .child(label("Summary"))
                    .child(fill())
                    .when(writing.is_none(), |d| {
                        d.child(
                            link_button("sum-again", "Summarize again")
                                .on_click(cx.listener(|this, _, w, cx| this.open_summarize(w, cx))),
                        )
                    })
                    .when(writing.is_some(), |d| {
                        d.child(
                            link_button("sum-stop", "Stop")
                                .on_click(cx.listener(|this, _, _, cx| this.stop_summary(cx))),
                        )
                    })
                    .when(!text.is_empty(), |d| {
                        d.child(
                            link_button("sum-copy", "Copy")
                                .on_click(cx.listener(|this, _, _, cx| this.copy_summary(cx))),
                        )
                    }),
            )
            .when_some(meta, |d, m| {
                d.child(div().text_size(t::T_SMALL).text_color(t::text_3()).child(m))
            })
            .child(with_scroll_bar(
                "summary-frame",
                div()
                    .id("summary-body")
                    .w_full()
                    .max_h(px(168.))
                    .pr(px(16.))
                    .overflow_y_scroll()
                    .track_scroll(&self.summary_scroll)
                    .text_size(px(15.))
                    .line_height(relative(1.55))
                    .text_color(if text.is_empty() {
                        t::text_2()
                    } else {
                        t::text()
                    })
                    .child(if text.is_empty() {
                        writing
                            .clone()
                            .unwrap_or_else(|| "Writing a summary of these lines…".into())
                    } else {
                        text
                    }),
                &self.summary_scroll,
            ))
            .when(writing.is_some() && !p.summary.trim().is_empty(), |d| {
                d.child(hint(writing.unwrap_or_default()))
            })
    }

    fn toolbar(&self, bp: Bp, cx: &mut Context<Self>) -> Div {
        let Some(p) = self.current() else {
            return div();
        };
        let stats = project_stats(&self.glossary, p);
        let only = self.editor.review_only;
        let count = match stats.count {
            1 => "1 line".to_string(),
            n => format!("{n} lines"),
        };
        let all: Vec<usize> = (0..p.segs.len()).collect();
        let can_identify = self.audio_source(p).is_some()
            && self.speaking.is_none()
            && self.speaker_pending.is_none()
            && p.status != Status::Working;
        let reprocessing = self.reprocess.as_ref().is_some_and(|j| j.pid == p.id);
        let can_reprocess = self.audio_source(p).is_some()
            && p.status != Status::Working
            && p.status != Status::Live;
        row()
            .flex_none()
            .gap(px(14.))
            .px(bp.pad_x())
            .py(px(8.))
            .text_size(t::T_SMALL)
            .text_color(t::text_2())
            .child(count)
            .when(stats.flagged > 0 || only, |d| {
                d.child(
                    switch(
                        "ed-only",
                        only,
                        &format!("Only lines to check ({})", stats.flagged),
                    )
                    .on_click(cx.listener(|this, _, w, cx| this.toggle_review_only(w, cx))),
                )
            })
            .child(fill())
            .when(can_reprocess || reprocessing, |d| {
                d.child(
                    button(
                        "ed-reprocess",
                        Btn::Quiet,
                        Some(Icon::Captions),
                        Some(if reprocessing {
                            "Reprocessing…"
                        } else {
                            "Reprocess subtitle"
                        }),
                        reprocessing,
                    )
                    .when(!reprocessing, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| this.ask_reprocess(cx)))
                    }),
                )
            })
            .map(|d| {
                let busy = self.correcting.as_ref().is_some_and(|j| j.pid == p.id);
                d.child(
                    button(
                        "ed-correct",
                        Btn::Quiet,
                        Some(Icon::Wand),
                        Some(if busy { "Correcting…" } else { "Correct" }),
                        busy,
                    )
                    .when(!busy, |d| {
                        d.on_click(cx.listener(|this, _, w, cx| this.open_correct(w, cx)))
                    }),
                )
            })
            .when(
                p.summary.trim().is_empty()
                    && self.summary_job.as_ref().is_none_or(|j| j.pid != p.id),
                |d| {
                    d.child(
                        button(
                            "ed-summarize",
                            Btn::Quiet,
                            Some(Icon::Summary),
                            Some("Summarize"),
                            false,
                        )
                        .on_click(cx.listener(|this, _, w, cx| this.open_summarize(w, cx))),
                    )
                },
            )
            .when(
                self.summary_job.as_ref().is_some_and(|j| j.pid == p.id),
                |d| {
                    d.child(button(
                        "ed-summarize",
                        Btn::Quiet,
                        Some(Icon::Summary),
                        Some("Summarizing…"),
                        true,
                    ))
                },
            )
            .when(can_identify, |d| {
                d.child(
                    button(
                        "ed-speakers",
                        Btn::Quiet,
                        Some(Icon::Users),
                        Some(if p.has_speakers() {
                            "Identify again"
                        } else {
                            "Identify speakers"
                        }),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.identify_speakers(cx))),
                )
            })
            .when(p.video_path.is_some() && p.status != Status::Live, |d| {
                d.child(
                    button(
                        "ed-video",
                        Btn::Quiet,
                        Some(Icon::Film),
                        Some("Open video"),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.open_video(cx))),
                )
            })
            .when(p.kind == Kind::Audio && p.source_path.is_some(), |d| {
                d.child(
                    button(
                        "ed-audio",
                        Btn::Quiet,
                        Some(Icon::Waveform),
                        Some("Show audio"),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.reveal_recording(cx))),
                )
            })
            .when(!p.translate && p.status != Status::Working, |d| {
                d.child(
                    button(
                        "ed-translate",
                        Btn::Quiet,
                        Some(Icon::Translate),
                        Some("Translate"),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.open_translate(cx))),
                )
            })
            .when(p.translate && p.status != Status::Working, |d| {
                d.child(
                    button(
                        "ed-retranslate-all",
                        Btn::Quiet,
                        Some(Icon::Refresh),
                        Some("Retranslate all"),
                        false,
                    )
                    .on_click(
                        cx.listener(move |this, _, w, cx| this.retranslate(all.clone(), w, cx)),
                    ),
                )
            })
    }

    fn line_list(
        &self,
        bp: Bp,
        capturing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(p) = self.current() else {
            return div().into_any_element();
        };
        let rows = self.visible_rows();
        let none_left = rows.is_empty();
        let stacked = bp.w <= 760.;
        let focused = self.seg_input.read(cx).focus_handle(cx).is_focused(window);
        let translating = p.status == Status::Working || capturing;
        let playing = self.playing_line();
        let list = div()
            .id("lines")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.lines_scroll)
            .pb(px(32.))
            .children(rows.into_iter().map(|(i, flags)| {
                let here = playing == Some(i);
                self.line_row(
                    bp,
                    stacked,
                    i,
                    &p.segs[i],
                    &flags,
                    translating,
                    focused,
                    here,
                    cx,
                )
            }))
            .children(capturing.then(|| self.heard_line(bp)).flatten())
            .when(none_left, |d| {
                d.child(
                    hint("Every line reads well now.")
                        .px(bp.pad_x())
                        .py(px(20.)),
                )
            });
        with_scroll_bar("lines-frame", list, &self.lines_scroll)
            .flex_1()
            .min_h_0()
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn line_row(
        &self,
        bp: Bp,
        stacked: bool,
        i: usize,
        s: &Segment,
        flags: &[Flag],
        translating: bool,
        focused: bool,
        playing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.editor.active == Some(i);
        let Some(p) = self.current() else {
            return div().into_any_element();
        };
        let can_play = self.audio_source(p).is_some();
        let translated = p.translate;
        let speaker = s
            .speaker
            .filter(|n| i == 0 || p.segs[i - 1].speaker != Some(*n))
            .map(|n| {
                row()
                    .id(("spk", i))
                    .gap(px(6.))
                    .pl(px(TIME_W + RAIL_W + 24. + if translated { 0. } else { 9. }))
                    .pb(px(2.))
                    .text_size(px(12.5))
                    .font_weight(weight(560.))
                    .text_color(t::text_2())
                    .cursor_pointer()
                    .hover(|s| s.text_color(t::text()))
                    .child(div().size(px(7.)).rounded_full().bg(speaker_color(n)))
                    .child(p.speaker_name(n))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        cx.stop_propagation();
                        this.open_speaker_rename(n, w, cx)
                    }))
            });
        let at = s.in_ms;
        let issues: Vec<&Flag> = flags
            .iter()
            .filter(|f| {
                !matches!(
                    f.kind,
                    FlagKind::Ok | FlagKind::Term | FlagKind::Empty | FlagKind::Fix
                )
            })
            .collect();
        let fix = s.fix.as_deref().filter(|_| self.editor.editing != Some(i));
        let cjk = crate::subs::is_cjk(&p.from);

        let time = mono(div())
            .id(("time", i))
            .w(px(TIME_W))
            .flex_none()
            .pt(px(2.))
            .text_size(px(12.5))
            .text_color(if playing {
                t::accent()
            } else if issues.is_empty() {
                t::text_3()
            } else {
                t::warn()
            })
            .child(mmss(s.in_ms))
            .when(can_play, |d| {
                d.cursor_pointer()
                    .hover(|s| s.text_color(t::accent()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.play(Some(at), cx)
                    }))
            });
        let heard = |this: &Self, cx: &mut Context<Self>| match fix {
            Some(f) => this.fix_cell(i, &s.orig, f, cjk, false, cx),
            None => div()
                .text_size(px(15.))
                .line_height(relative(1.55))
                .text_color(t::text())
                .child(s.orig.clone()),
        };
        let texts = if !translated {
            div().flex_1().min_w_0().child(match fix {
                Some(f) => self
                    .fix_cell(i, &s.orig, f, cjk, true, cx)
                    .into_any_element(),
                None => self.text_cell(i, &s.orig, false, translating, focused, cx),
            })
        } else if stacked {
            let orig = heard(self, cx);
            let trans = self.text_cell(i, &s.trans, true, translating, focused, cx);
            col()
                .flex_1()
                .min_w_0()
                .gap(px(4.))
                .child(orig)
                .child(trans)
        } else {
            let orig = heard(self, cx);
            let trans = self.text_cell(i, &s.trans, true, translating, focused, cx);
            row()
                .flex_1()
                .min_w_0()
                .items_start()
                .gap(px(20.))
                .child(div().flex_1().min_w_0().pt(px(6.)).child(orig))
                .child(div().flex_1().min_w_0().child(trans))
        };

        let note = if active {
            Some(self.line_checks(i, cx))
        } else if !issues.is_empty() {
            Some(
                div()
                    .text_size(t::T_SMALL)
                    .text_color(t::warn_text())
                    .child(
                        issues
                            .iter()
                            .map(|f| f.label.as_str())
                            .collect::<Vec<_>>()
                            .join(" · "),
                    ),
            )
        } else {
            None
        };

        div()
            .id(("seg", i))
            .relative()
            .when(playing, |d| {
                d.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.))
                        .bg(t::accent()),
                )
            })
            .px(bp.pad_x())
            .py(px(8.))
            .border_b_1()
            .border_color(t::line_soft())
            .map(|d| {
                if active {
                    d.bg(t::surface())
                } else {
                    d.hover(|s| s.bg(t::surface()))
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| this.activate_row(i, cx)))
            .children(speaker)
            .child(
                row().items_start().gap(px(12.)).child(time).child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .when(p.has_speakers(), |d| {
                            d.pl(px(RAIL_W + 12.)).child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top(px(4.))
                                    .bottom(px(4.))
                                    .w(px(RAIL_W))
                                    .rounded_full()
                                    .when_some(s.speaker, |d, n| d.bg(speaker_color(n))),
                            )
                        })
                        .child(texts),
                ),
            )
            .when_some(note, |d, n| {
                let rail = if p.has_speakers() { RAIL_W + 12. } else { 0. };
                let cell = if translated { 0. } else { 9. };
                d.child(div().pl(px(TIME_W + 12. + rail + cell)).pt(px(4.)).child(n))
            })
            .into_any_element()
    }

    /// The selected line's numbers, in one sentence, plus the one action
    /// that applies to a single line.
    fn line_checks(&self, i: usize, cx: &mut Context<Self>) -> Div {
        let Some(p) = self.current() else {
            return div();
        };
        let s = &p.segs[i];
        let text = if p.translate {
            s.trans.trim()
        } else {
            s.orig.trim()
        };
        let dur = ((s.out_ms - s.in_ms) as f64 / 1000.0).max(0.1);
        let cps = if text.is_empty() {
            0.0
        } else {
            char_len(text) as f64 / dur
        };
        let longest = longest_line(text);
        let gap = p.segs.get(i + 1).map(|n| n.in_ms - s.out_ms);
        let terms = if p.translate {
            terms_in(&self.glossary, &s.orig, Some(&p.id))
        } else {
            Vec::new()
        };
        let translated = p.translate;

        let part = |text: String, bad: bool| {
            div()
                .text_color(if bad { t::warn_text() } else { t::text_2() })
                .child(text)
        };
        let mut parts = vec![
            part(
                format!("{} → {}", srt_time(s.in_ms), srt_time(s.out_ms)),
                false,
            ),
            part(format!("{dur:.1} s"), false),
        ];
        if !text.is_empty() {
            parts.push(part(format!("{cps:.0} chars/s"), cps > MAX_CPS));
            parts.push(part(format!("longest line {longest}"), longest > MAX_LINE));
        }
        if let Some(g) = gap {
            parts.push(part(format!("{g} ms to next"), g < MIN_GAP_MS));
        }
        if !terms.is_empty() {
            parts.push(part(format!("holds {}", terms.join(", ")), false));
        }
        row()
            .flex_wrap()
            .gap(px(12.))
            .text_size(t::T_SMALL)
            .children(parts)
            .when(translated, |d| {
                d.child(
                    link_button(("retranslate", i), "Retranslate").on_click(cx.listener(
                        move |this, _, w, cx| {
                            cx.stop_propagation();
                            this.retranslate(vec![i], w, cx)
                        },
                    )),
                )
            })
    }

    /// The editable text of a line (its translation, or the words themselves
    /// when nothing is translated) reads as plain text and turns into an
    /// editor in place, so the list stays a reading surface until touched.
    fn text_cell(
        &self,
        i: usize,
        text: &str,
        translated: bool,
        translating: bool,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.editor.loading.contains(&i) {
            return div()
                .h(px(32.))
                .w_full()
                .rounded(t::R_SM)
                .bg(t::surface_2())
                .with_animation(
                    ("shimmer", i),
                    Animation::new(Duration::from_millis(1100))
                        .repeat()
                        .with_easing(gpui::bounce(gpui::ease_in_out)),
                    |d, delta| {
                        d.bg(t::surface_2().blend(gpui::Hsla {
                            a: delta * 0.9,
                            ..t::surface_3()
                        }))
                    },
                )
                .into_any_element();
        }
        if self.editor.editing == Some(i) {
            return div()
                .w_full()
                .rounded(t::R_SM)
                .border_1()
                .border_color(if focused { t::accent() } else { t::line() })
                .bg(t::bg())
                .when(focused, |d| d.shadow(vec![ring_shadow()]))
                .child(
                    Input::new(&self.seg_input)
                        .appearance(false)
                        .px(px(9.))
                        .py(px(5.))
                        .text_size(px(15.))
                        .line_height(relative(1.55)),
                )
                .into_any_element();
        }
        let empty = text.trim().is_empty();
        div()
            .id(("tr", i))
            .w_full()
            .min_h(px(32.))
            .px(px(9.))
            .py(px(5.))
            .rounded(t::R_SM)
            .border_1()
            .border_color(gpui::transparent_black())
            .cursor_text()
            .hover(|st| st.border_color(t::line()))
            .text_size(px(15.))
            .line_height(relative(1.55))
            .map(|d| {
                if empty {
                    d.text_color(t::text_3())
                        .child(match (translated, translating) {
                            (true, true) => "Translating…",
                            (true, false) => "Not translated",
                            (false, _) => "Empty line",
                        })
                } else {
                    d.text_color(if translated {
                        t::accent_text()
                    } else {
                        t::text()
                    })
                    .child(text.to_string())
                }
            })
            .on_click(cx.listener(move |this, _, w, cx| {
                cx.stop_propagation();
                this.activate_row(i, cx);
                this.start_editing(i, w, cx)
            }))
            .into_any_element()
    }

    /// Play and pause, where the playhead is, and a bar to move it.
    fn transport(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let pid = self.project_id.clone().unwrap_or_default();
        let pl = self
            .player
            .as_ref()
            .filter(|(id, _)| *id == pid)
            .map(|(_, pl)| pl);
        let loading = self
            .player_loading
            .as_ref()
            .is_some_and(|(id, _)| *id == pid);
        let playing = pl.is_some_and(|p| p.is_playing());
        let (pos, dur) = match pl {
            Some(p) => (p.position_ms(), p.duration_ms()),
            None => (
                0,
                self.current()
                    .and_then(|p| p.segs.last())
                    .map(|s| s.out_ms)
                    .unwrap_or(0),
            ),
        };
        let ratio = if dur > 0 {
            (pos as f32 / dur as f32).clamp(0., 1.)
        } else {
            0.
        };
        let bounds = self.scrub_bounds.clone();
        let entity = cx.entity().downgrade();
        let scrubbing = self.scrubbing;
        let bar = div()
            .id("scrub")
            .relative()
            .flex_1()
            .h(px(16.))
            .flex()
            .items_center()
            .when(pl.is_some(), |d| d.cursor_pointer())
            .child(
                canvas(
                    move |b, _, _| bounds.set(b),
                    move |_, _, window, _| {
                        if !scrubbing {
                            return;
                        }
                        let e = entity.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, _, _, cx| {
                            e.update(cx, |this, cx| this.seek_ratio(ev.position.x, cx))
                                .ok();
                        });
                        let e = entity.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                            e.update(cx, |this, cx| {
                                this.scrubbing = false;
                                cx.notify();
                            })
                            .ok();
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .w_full()
                    .h(px(3.))
                    .rounded_full()
                    .bg(t::surface_3())
                    .child(
                        div()
                            .h_full()
                            .w(relative(ratio))
                            .rounded_full()
                            .bg(t::accent()),
                    ),
            )
            .when(pl.is_some(), |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(2.))
                        .left(relative(ratio))
                        .ml(px(-6.))
                        .size(px(12.))
                        .rounded_full()
                        .bg(t::text()),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        this.scrubbing = true;
                        this.seek_ratio(ev.position.x, cx);
                    }),
                )
            });
        row()
            .flex_none()
            .gap(px(12.))
            .px(bp.pad_x())
            .py(px(8.))
            .bg(t::surface())
            .border_t_1()
            .border_color(t::line_soft())
            .child(
                button(
                    "play",
                    Btn::Icon,
                    Some(if playing { Icon::Pause } else { Icon::Play }),
                    None,
                    loading,
                )
                .when(!loading, |d| {
                    d.on_click(cx.listener(|this, _, _, cx| this.play(None, cx)))
                }),
            )
            .child(
                mono(div())
                    .flex_none()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(if loading {
                        "Loading audio…".to_string()
                    } else {
                        format!("{} / {}", mmss(pos), mmss(dur))
                    }),
            )
            .child(bar)
            .into_any_element()
    }

    fn reprocess_strip(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let note = self
            .reprocess
            .as_ref()
            .map(|j| j.note.clone())
            .unwrap_or_default();
        self.strip(bp)
            .child(Self::strip_title("Reprocessing subtitles"))
            .child(
                mono(div())
                    .flex_none()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(note),
            )
            .child(div().flex_1().min_w(px(60.)).child(running_bar("re-run")))
            .child(
                button("re-stop", Btn::Quiet, None, Some("Stop"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.stop_reprocess(cx))),
            )
            .into_any_element()
    }

    fn speaker_strip(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let (title, note) = match &self.speaking {
            Some(j) => (
                "Identifying speakers".to_string(),
                format!("{} of {} lines", j.done, j.total),
            ),
            None => {
                let pct = self
                    .downloads
                    .get(crate::engine::speaker::SPEAKER_DOWNLOAD_ID)
                    .map_or(0., |d| d.pct);
                (
                    "Fetching the speaker model".to_string(),
                    format!(
                        "{:.0}% of {}, once only",
                        pct * 100.,
                        crate::engine::speaker::SPEAKER_SIZE
                    ),
                )
            }
        };
        self.strip(bp)
            .child(Self::strip_title(title))
            .child(
                mono(div())
                    .flex_none()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(note),
            )
            .child(div().flex_1().min_w(px(60.)).child(running_bar("spk-run")))
            .child(
                button("spk-cancel", Btn::Quiet, None, Some("Cancel"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_speakers(cx))),
            )
            .into_any_element()
    }

    fn correct_strip(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let note = self
            .correcting
            .as_ref()
            .map(|j| match j.span {
                Some((from, to)) if j.done < j.total => {
                    format!("{} of {} lines, reading {from}–{to}", j.done, j.total)
                }
                _ => format!("{} of {} lines", j.done, j.total),
            })
            .unwrap_or_default();
        self.strip(bp)
            .child(Self::strip_title("Correcting the lines"))
            .child(
                mono(div())
                    .flex_none()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(note),
            )
            .child(div().flex_1().min_w(px(60.)).child(running_bar("fix-run")))
            .child(
                button("fix-stop", Btn::Quiet, None, Some("Stop"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.stop_correcting(cx))),
            )
            .into_any_element()
    }

    fn video_export_strip(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let job = self.video_export.as_ref();
        let stopping = job.is_some_and(|j| j.stopping);
        let pct = job.map(|j| j.pct).unwrap_or(0.0);
        let note = if stopping {
            "Stopping…".to_string()
        } else if pct > 0.0 {
            format!("{}%", (pct * 100.0).round() as u32)
        } else {
            "Writing…".to_string()
        };
        self.strip(bp)
            .items_center()
            .child(Self::strip_title("Writing the video"))
            .child(
                mono(div())
                    .flex_none()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(note),
            )
            .child(div().flex_1().min_w(px(60.)).flex().items_center().child(
                if pct > 0.0 && !stopping {
                    div()
                        .w_full()
                        .h(px(3.))
                        .rounded_full()
                        .bg(t::surface_3())
                        .child(
                            div()
                                .h_full()
                                .w(relative(pct.clamp(0., 1.)))
                                .rounded_full()
                                .bg(t::accent()),
                        )
                        .into_any_element()
                } else {
                    running_bar("vid-run").into_any_element()
                },
            ))
            .when(!stopping, |d| {
                d.child(
                    button("vid-stop", Btn::Quiet, None, Some("Stop"), false)
                        .on_click(cx.listener(|this, _, _, cx| this.stop_video_export(cx))),
                )
            })
            .into_any_element()
    }

    fn fixes_strip(&self, n: usize, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        self.strip(bp)
            .child(Self::strip_title("Suggested fixes"))
            .child(
                mono(div())
                    .flex_none()
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(if n == 1 {
                        "1 line".to_string()
                    } else {
                        format!("{n} lines")
                    }),
            )
            .child(fill())
            .child(
                button("fix-none", Btn::Quiet, None, Some("Dismiss all"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss_fixes(None, cx))),
            )
            .child(
                button(
                    "fix-all",
                    Btn::Ghost,
                    Some(Icon::Check),
                    Some("Accept all"),
                    false,
                )
                .on_click(cx.listener(|this, _, w, cx| this.accept_fixes(None, w, cx))),
            )
            .into_any_element()
    }

    /// A suggested correction marked against what was heard, with the choice
    /// to take it or keep the line. `inset` lines it up with an editable cell.
    fn fix_cell(
        &self,
        i: usize,
        heard: &str,
        fix: &str,
        cjk: bool,
        inset: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let gone = HighlightStyle {
            color: Some(t::danger_text()),
            strikethrough: Some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(t::danger_text()),
            }),
            ..Default::default()
        };
        let added = HighlightStyle {
            color: Some(t::ok()),
            background_color: Some(gpui::Hsla { a: 0.14, ..t::ok() }),
            ..Default::default()
        };
        let runs = word_diff(heard, fix);
        let mut text = String::new();
        let mut marks = Vec::new();
        for (k, (change, run)) in runs.iter().enumerate() {
            let start = text.len();
            text.push_str(run);
            match change {
                Change::Same => {}
                Change::Gone => marks.push((start..text.len(), gone)),
                Change::Added => marks.push((start..text.len(), added)),
            }
            // Struck-out words and their replacement read apart, except in
            // scripts written without spaces.
            if !cjk
                && *change == Change::Gone
                && runs.get(k + 1).is_some_and(|(c, _)| *c == Change::Added)
            {
                text.push(' ');
            }
        }
        col()
            .gap(px(2.))
            .when(inset, |d| d.px(px(9.)).py(px(5.)))
            .child(
                div()
                    .text_size(px(15.))
                    .line_height(relative(1.55))
                    .text_color(t::text())
                    .child(StyledText::new(text).with_highlights(marks)),
            )
            .child(
                row()
                    .gap(px(14.))
                    .text_size(t::T_SMALL)
                    .child(link_button(("fix-take", i), "Accept").on_click(cx.listener(
                        move |this, _, w, cx| {
                            cx.stop_propagation();
                            this.accept_fixes(Some(i), w, cx)
                        },
                    )))
                    .child(
                        link_button(("fix-drop", i), "Dismiss").on_click(cx.listener(
                            move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.dismiss_fixes(Some(i), cx)
                            },
                        )),
                    ),
            )
    }

    fn failed_strip(&self, p: &Project, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let id = p.id.clone();
        let can_reprocess = self.audio_source(p).is_some();
        row()
            .flex_none()
            .gap(px(12.))
            .px(bp.pad_x())
            .py(px(10.))
            .bg(t::danger_wash())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(t::T_SMALL)
                    .text_color(t::danger_text())
                    .child(format!(
                        "{}.",
                        p.error
                            .clone()
                            .unwrap_or_else(|| "This project failed".into())
                            .trim_end_matches('.')
                    )),
            )
            .when(can_reprocess, |d| {
                d.child(
                    button(
                        "ed-failed-reprocess",
                        Btn::Quiet,
                        Some(Icon::Captions),
                        Some("Reprocess subtitle"),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.ask_reprocess(cx))),
                )
            })
            .child(
                button(
                    "ed-failed-del",
                    Btn::DangerGhost,
                    Some(Icon::Trash),
                    Some("Delete"),
                    false,
                )
                .on_click(cx.listener(move |this, _, w, cx| this.delete_project(&id, w, cx))),
            )
            .into_any_element()
    }

    pub(crate) fn strip(&self, bp: Bp) -> Div {
        row()
            .flex_none()
            .gap(px(12.))
            .px(bp.pad_x())
            .py(px(8.))
            .min_h(px(52.))
            .bg(t::surface())
            .border_b_1()
            .border_color(t::line_soft())
    }

    pub(crate) fn strip_title(text: impl Into<SharedString>) -> Div {
        div()
            .flex_none()
            .text_size(t::T_CONTROL)
            .font_weight(weight(560.))
            .whitespace_nowrap()
            .child(text.into())
    }
}

/// Language, model and length of the summary already written.
fn summary_meta(p: &Project) -> Option<String> {
    if p.summary.trim().is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !p.summary_lang.is_empty() {
        parts.push(lang_name(&p.summary_lang));
    }
    if !p.summary_model.is_empty() {
        parts.push(p.summary_model.clone());
    }
    if p.summary_long {
        parts.push("a fuller account".into());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Each voice keeps one colour wherever it speaks.
fn speaker_color(n: u8) -> gpui::Hsla {
    const HUES: [f32; 8] = [0.58, 0.07, 0.38, 0.82, 0.13, 0.95, 0.48, 0.70];
    gpui::hsla(HUES[n as usize % HUES.len()], 0.55, 0.55, 1.)
}
