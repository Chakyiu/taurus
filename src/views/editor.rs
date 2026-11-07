use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, Div, Focusable as _, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, canvas, div, px, relative,
};
use gpui_component::input::Input;

use super::screen;
use crate::app::*;
use crate::data::*;
use crate::icons::Icon;
use crate::shell::{Bp, ring_shadow};
use crate::theme::{self as t, weight};
use crate::ui::*;

const TIME_W: f32 = 64.;
/// The speaker colour beside every line, so a turn reads as one block.
const RAIL_W: f32 = 3.;

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
                    vec![button("ed-go-projects", Btn::Primary, None, Some("Projects"), false)
                        .on_click(cx.listener(|this, _, w, cx| this.show_view(View::Projects, w, cx)))
                        .into_any_element()],
                ))
                .into_any_element();
        };
        let pid = p.id.clone();
        let capturing = self.live.project_id.as_deref() == Some(pid.as_str())
            && self.live.status != LiveStatus::Idle;
        let extracting = self.extracting(&pid);

        let strip = if capturing {
            Some(self.live_strip(bp, cx))
        } else if extracting {
            Some(self.extract_strip(bp, cx))
        } else if self.speaking.as_ref().is_some_and(|j| j.pid == pid)
            || self.speaker_pending.as_deref() == Some(pid.as_str())
        {
            Some(self.speaker_strip(bp, cx))
        } else if p.status == Status::Failed {
            Some(self.failed_strip(p, bp, cx))
        } else {
            None
        };

        let body: AnyElement = if p.segs.is_empty() {
            let text = if capturing {
                self.live
                    .engine_note
                    .clone()
                    .unwrap_or_else(|| "Listening. Lines appear here as soon as someone speaks.".into())
            } else if extracting {
                "Lines appear here as soon as the track is read.".into()
            } else {
                "This project has no lines.".into()
            };
            let partial = capturing.then(|| self.heard_line(bp)).flatten();
            col()
                .flex_1()
                .px(bp.pad_x())
                .children(partial)
                .when(!capturing || self.live.partial.is_none(), |d| {
                    d.child(hint(text).py(px(24.)).text_center())
                })
                .into_any_element()
        } else {
            self.line_list(bp, capturing, window, cx)
        };

        let transport = self.audio_source(p).map(|_| self.transport(bp, cx));
        screen()
            .children(strip)
            .when(!p.segs.is_empty() && !capturing, |d| d.child(self.toolbar(bp, cx)))
            .child(body)
            .children(transport)
            .into_any_element()
    }

    fn toolbar(&self, bp: Bp, cx: &mut Context<Self>) -> Div {
        let Some(p) = self.current() else { return div() };
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
                    switch("ed-only", only, &format!("Only lines to check ({})", stats.flagged))
                        .on_click(cx.listener(|this, _, w, cx| this.toggle_review_only(w, cx))),
                )
            })
            .child(fill())
            .when(can_identify, |d| {
                d.child(
                    button(
                        "ed-speakers",
                        Btn::Quiet,
                        Some(Icon::Users),
                        Some(if p.has_speakers() { "Identify again" } else { "Identify speakers" }),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.identify_speakers(cx))),
                )
            })
            .when(p.kind == Kind::Audio && p.source_path.is_some(), |d| {
                d.child(
                    button("ed-audio", Btn::Quiet, Some(Icon::Waveform), Some("Show audio"), false)
                        .on_click(cx.listener(|this, _, _, cx| this.reveal_recording(cx))),
                )
            })
            .when(!p.translate && p.status != Status::Working, |d| {
                d.child(
                    button("ed-translate", Btn::Quiet, Some(Icon::Translate), Some("Translate"), false)
                        .on_click(cx.listener(|this, _, _, cx| this.open_translate(cx))),
                )
            })
            .when(p.translate && p.status != Status::Working, |d| {
                d.child(
                    button("ed-retranslate-all", Btn::Quiet, Some(Icon::Refresh), Some("Retranslate all"), false)
                        .on_click(cx.listener(move |this, _, w, cx| this.retranslate(all.clone(), w, cx))),
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
        let Some(p) = self.current() else { return div().into_any_element() };
        let rows = self.visible_rows();
        let none_left = rows.is_empty();
        let stacked = bp.w <= 760.;
        let focused = self.seg_input.read(cx).focus_handle(cx).is_focused(window);
        let translating = p.status == Status::Working || capturing;
        let playing = self.playing_line();
        div()
            .id("lines")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.lines_scroll)
            .pb(px(32.))
            .children(rows.into_iter().map(|(i, flags)| {
                let here = playing == Some(i);
                self.line_row(bp, stacked, i, &p.segs[i], &flags, translating, focused, here, cx)
            }))
            .children(capturing.then(|| self.heard_line(bp)).flatten())
            .when(none_left, |d| {
                d.child(hint("Every line reads well now.").px(bp.pad_x()).py(px(20.)))
            })
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
        let Some(p) = self.current() else { return div().into_any_element() };
        let can_play = self.audio_source(p).is_some();
        let translated = p.translate;
        let speaker = s.speaker.filter(|n| i == 0 || p.segs[i - 1].speaker != Some(*n)).map(|n| {
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
            .filter(|f| !matches!(f.kind, FlagKind::Ok | FlagKind::Term | FlagKind::Empty))
            .collect();

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
        let texts = if !translated {
            div().flex_1().min_w_0().child(self.text_cell(i, &s.orig, false, translating, focused, cx))
        } else if stacked {
            let orig = div().text_size(px(15.)).line_height(relative(1.55)).text_color(t::text()).child(s.orig.clone());
            let trans = self.text_cell(i, &s.trans, true, translating, focused, cx);
            col().flex_1().min_w_0().gap(px(4.)).child(orig).child(trans)
        } else {
            let orig = div().text_size(px(15.)).line_height(relative(1.55)).text_color(t::text()).child(s.orig.clone());
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
                    .child(issues.iter().map(|f| f.label.as_str()).collect::<Vec<_>>().join(" · ")),
            )
        } else {
            None
        };

        div()
            .id(("seg", i))
            .relative()
            .when(playing, |d| {
                d.child(div().absolute().left_0().top_0().bottom_0().w(px(2.)).bg(t::accent()))
            })
            .px(bp.pad_x())
            .py(px(8.))
            .border_b_1()
            .border_color(t::line_soft())
            .map(|d| if active { d.bg(t::surface()) } else { d.hover(|s| s.bg(t::surface())) })
            .on_click(cx.listener(move |this, _, _, cx| this.activate_row(i, cx)))
            .children(speaker)
            .child(
                row()
                    .items_start()
                    .gap(px(12.))
                    .child(time)
                    .child(
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
        let Some(p) = self.current() else { return div() };
        let s = &p.segs[i];
        let text = if p.translate { s.trans.trim() } else { s.orig.trim() };
        let dur = ((s.out_ms - s.in_ms) as f64 / 1000.0).max(0.1);
        let cps = if text.is_empty() { 0.0 } else { char_len(text) as f64 / dur };
        let longest = longest_line(text);
        let gap = p.segs.get(i + 1).map(|n| n.in_ms - s.out_ms);
        let terms = if p.translate { terms_in(&self.glossary, &s.orig, Some(&p.id)) } else { Vec::new() };
        let translated = p.translate;

        let part = |text: String, bad: bool| {
            div()
                .text_color(if bad { t::warn_text() } else { t::text_2() })
                .child(text)
        };
        let mut parts = vec![
            part(format!("{} → {}", srt_time(s.in_ms), srt_time(s.out_ms)), false),
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
                d.child(link_button(("retranslate", i), "Retranslate").on_click(cx.listener(
                    move |this, _, w, cx| {
                        cx.stop_propagation();
                        this.retranslate(vec![i], w, cx)
                    },
                )))
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
                    d.text_color(t::text_3()).child(match (translated, translating) {
                        (true, true) => "Translating…",
                        (true, false) => "Not translated",
                        (false, _) => "Empty line",
                    })
                } else {
                    d.text_color(if translated { t::accent_text() } else { t::text() }).child(text.to_string())
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
        let pl = self.player.as_ref().filter(|(id, _)| *id == pid).map(|(_, pl)| pl);
        let loading = self.player_loading.as_ref().is_some_and(|(id, _)| *id == pid);
        let playing = pl.is_some_and(|p| p.is_playing());
        let (pos, dur) = match pl {
            Some(p) => (p.position_ms(), p.duration_ms()),
            None => (0, self.current().and_then(|p| p.segs.last()).map(|s| s.out_ms).unwrap_or(0)),
        };
        let ratio = if dur > 0 { (pos as f32 / dur as f32).clamp(0., 1.) } else { 0. };
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
                            e.update(cx, |this, cx| this.seek_ratio(ev.position.x, cx)).ok();
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
                    .child(div().h_full().w(relative(ratio)).rounded_full().bg(t::accent())),
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
                .when(!loading, |d| d.on_click(cx.listener(|this, _, _, cx| this.play(None, cx)))),
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

    fn speaker_strip(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let (title, note) = match &self.speaking {
            Some(j) => ("Identifying speakers".to_string(), format!("{} of {} lines", j.done, j.total)),
            None => {
                let pct = self.downloads.get(crate::engine::speaker::SPEAKER_DOWNLOAD_ID).map_or(0., |d| d.pct);
                (
                    "Fetching the speaker model".to_string(),
                    format!("{:.0}% of {}, once only", pct * 100., crate::engine::speaker::SPEAKER_SIZE),
                )
            }
        };
        self.strip(bp)
            .child(Self::strip_title(title))
            .child(mono(div()).flex_none().text_size(t::T_SMALL).text_color(t::text_2()).child(note))
            .child(div().flex_1().min_w(px(60.)).child(running_bar("spk-run")))
            .child(
                button("spk-cancel", Btn::Quiet, None, Some("Cancel"), false)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_speakers(cx))),
            )
            .into_any_element()
    }

    fn failed_strip(&self, p: &Project, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let id = p.id.clone();
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
                        p.error.clone().unwrap_or_else(|| "This project failed".into()).trim_end_matches('.')
                    )),
            )
            .child(
                button("ed-failed-del", Btn::DangerGhost, Some(Icon::Trash), Some("Delete"), false)
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

/// Each voice keeps one colour wherever it speaks.
fn speaker_color(n: u8) -> gpui::Hsla {
    const HUES: [f32; 8] = [0.58, 0.07, 0.38, 0.82, 0.13, 0.95, 0.48, 0.70];
    gpui::hsla(HUES[n as usize % HUES.len()], 0.55, 0.55, 1.)
}
