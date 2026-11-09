//! The window shell: the top bar and its New menu, dropdowns, dialogs,
//! toasts and the text field frame.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, Corner, Div, Entity, ExternalPaths,
    Focusable, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Pixels,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, anchored, deferred, div, img,
    px, relative,
};
use gpui_component::input::{Input, InputState};

use crate::app::*;
use crate::data::*;
use crate::engine::AsrChoice;
use crate::icons::{APP_ICON, Icon, icon};
use crate::theme::{self as t, weight};
use crate::ui::*;
use crate::update;

/// Layout thresholds from the design's media queries, read off the window.
#[derive(Clone, Copy)]
pub struct Bp {
    pub w: f32,
}

impl Bp {
    pub fn of(window: &Window) -> Self {
        Self {
            w: f32::from(window.viewport_size().width),
        }
    }
    /// Navigation moves to a top strip and rows reflow onto two lines.
    pub fn mobile(self) -> bool {
        self.w <= 720.
    }
    pub fn tiny(self) -> bool {
        self.w <= 480.
    }
    pub fn pad_x(self) -> Pixels {
        if self.mobile() { px(16.) } else { px(24.) }
    }
}

impl Workbench {
    // ─── top bar ───────────────────────────────────────────

    /// One slim bar: where you are on the left, what you can do on the right.
    /// The empty stretches drag the window.
    fn render_topbar(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let update = self.update_button(bp, cx);
        let drag = || div().window_control_area(gpui::WindowControlArea::Drag);
        let back = || {
            button(
                "top-back",
                Btn::Quiet,
                Some(Icon::Back),
                Some("Projects"),
                false,
            )
            .on_click(cx.listener(|this, _, w, cx| this.show_view(View::Projects, w, cx)))
        };
        let mut left = row().gap(px(6.)).min_w_0();
        let mut right = row().flex_none().gap(px(6.)).children(update);
        match self.view {
            View::Projects => {
                left = left.child(
                    div()
                        .pl(px(4.))
                        .text_size(px(15.))
                        .font_weight(weight(620.))
                        .child("Taurus"),
                );
                right = right
                    .child(self.engine_button(bp, cx))
                    .child(
                        button(
                            "top-glossary",
                            Btn::Quiet,
                            Some(Icon::Tag),
                            (!bp.mobile()).then_some("Glossary"),
                            false,
                        )
                        .on_click(
                            cx.listener(|this, _, w, cx| this.show_view(View::Glossary, w, cx)),
                        ),
                    )
                    .child(
                        button(
                            "top-about",
                            Btn::Quiet,
                            Some(Icon::Info),
                            (!bp.mobile()).then_some("About"),
                            false,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.open_about(cx))),
                    )
                    .child(self.new_button(cx));
            }
            View::Glossary => {
                left = left.child(back()).child(self.crumb("Glossary"));
                right = right.child(
                    button(
                        "gl-add",
                        Btn::Primary,
                        Some(Icon::Plus),
                        Some("Add term"),
                        false,
                    )
                    .on_click(cx.listener(|this, _, w, cx| this.open_term_dialog(None, w, cx))),
                );
            }
            View::Editor => {
                left = left.child(back());
                if let Some(p) = self.current() {
                    let status = self.status_text(p);
                    left = left
                        .child(
                            div()
                                .id("top-name")
                                .min_w_0()
                                .px(px(8.))
                                .py(px(4.))
                                .rounded(t::R_SM)
                                .cursor_pointer()
                                .hover(|s| s.bg(t::surface_2()))
                                .text_size(px(15.))
                                .font_weight(weight(580.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(p.name.clone())
                                .on_click(cx.listener(|this, _, w, cx| {
                                    let id = this.project_id.clone();
                                    this.open_rename_dialog(id, w, cx)
                                })),
                        )
                        .when(!bp.tiny(), |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_size(t::T_SMALL)
                                    .text_color(status.1)
                                    .whitespace_nowrap()
                                    .child(status.0),
                            )
                        });
                    let can = !p.segs.is_empty();
                    right = right.child(
                        button(
                            "top-export",
                            Btn::Ghost,
                            Some(Icon::Download),
                            Some("Export"),
                            !can,
                        )
                        .when(can, |d| {
                            d.on_click(cx.listener(|this, _, _, cx| this.open_export(cx)))
                        }),
                    );
                }
            }
        }
        row()
            .flex_none()
            .h(px(48.))
            .pr(px(10.))
            .border_b_1()
            .border_color(t::line_soft())
            .child(drag().w(px(78.)).h_full().flex_none())
            .child(left)
            .child(drag().flex_1().h_full().min_w(px(12.)))
            .child(right)
            .into_any_element()
    }

    fn crumb(&self, text: &str) -> Div {
        div()
            .px(px(8.))
            .text_size(px(15.))
            .font_weight(weight(580.))
            .child(text.to_string())
    }

    /// What a project is doing, in a word or two, and the colour to say it in.
    pub fn status_text(&self, p: &Project) -> (String, gpui::Hsla) {
        let paused = self.live.project_id.as_deref() == Some(p.id.as_str())
            && self.live.status == LiveStatus::Paused;
        match p.status {
            Status::Live if paused => ("Paused".into(), t::warn_text()),
            Status::Live => ("Recording".into(), t::accent()),
            Status::Working if p.segs.is_empty() => ("Extracting".into(), t::accent()),
            Status::Working => (
                format!("Translating · {}%", p.translated_pct()),
                t::accent(),
            ),
            Status::Failed => ("Failed".into(), t::danger_text()),
            Status::Review => {
                let open = project_stats(&self.glossary, p).flagged;
                (
                    match open {
                        1 => "1 line to check".into(),
                        n => format!("{n} lines to check"),
                    },
                    t::warn_text(),
                )
            }
            Status::Done => ("Done".into(), t::text_3()),
        }
    }

    fn engine_button(&self, bp: Bp, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let (asr_ok, mt_ok) = (self.settings.asr_ready(), self.settings.mt_ready());
        let (dot, text) = match self.downloads.iter().next() {
            Some((_, d)) => (t::warn(), format!("Downloading {:.0}%", d.pct * 100.0)),
            None => match (asr_ok, mt_ok || !self.settings.translate) {
                (true, true) => (t::ok(), "Engines".to_string()),
                (true, false) => (t::warn(), "No translation".into()),
                (false, _) => (t::warn(), "Set up engines".into()),
            },
        };
        status_button("top-engines", dot, text, bp)
            .on_click(cx.listener(|this, _, w, cx| this.open_engines(w, cx)))
    }

    /// Shown once a newer Taurus is installed, or found but not installed.
    fn update_button(&self, bp: Bp, cx: &mut Context<Self>) -> Option<gpui::Stateful<Div>> {
        let (text, restart) = match &self.update {
            UpdateState::Ready(_) => ("Restart to update", true),
            UpdateState::Available { .. } => ("Update available", false),
            _ => return None,
        };
        Some(
            status_button("top-update", t::accent(), text.into(), bp).on_click(cx.listener(
                move |this, _, _, cx| {
                    if restart {
                        this.restart_to_update(cx)
                    } else {
                        this.open_about(cx)
                    }
                },
            )),
        )
    }

    fn new_button(&self, cx: &mut Context<Self>) -> Div {
        let trigger = button(
            "top-new",
            Btn::Primary,
            Some(Icon::Plus),
            Some("New"),
            false,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_new_menu(cx)));
        let mut wrap = div().flex_none().child(trigger);
        if !self.new_menu {
            return wrap;
        }
        let item = |id: &'static str, ic: Icon, title: &'static str, sub: &'static str| {
            row()
                .id(id)
                .items_start()
                .gap(px(10.))
                .px(px(10.))
                .py(px(8.))
                .rounded(t::R_SM)
                .cursor_pointer()
                .hover(|s| s.bg(t::surface_3()))
                .child(div().pt(px(2.)).child(icon(ic, px(16.), t::text_2())))
                .child(
                    col()
                        .min_w_0()
                        .child(div().text_size(t::T_CONTROL).child(title))
                        .child(
                            div()
                                .text_size(t::T_SMALL)
                                .text_color(t::text_2())
                                .child(sub),
                        ),
                )
        };
        let menu = col()
            .id("new-menu")
            .occlude()
            .mt(px(6.))
            .w(px(272.))
            .p(px(4.))
            .gap(px(1.))
            .rounded(t::R)
            .bg(t::surface_2())
            .border_1()
            .border_color(t::line())
            .shadow(vec![gpui::BoxShadow {
                color: t::toast_shadow(),
                offset: gpui::point(px(0.), px(8.)),
                blur_radius: px(24.),
                spread_radius: px(0.),
            }])
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.new_menu = false;
                cx.notify();
            }))
            .child(
                item(
                    "new-live",
                    Icon::Waveform,
                    "Live caption",
                    "Listen to this Mac or a microphone",
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_capture(cx))),
            )
            .child(
                item(
                    "new-video",
                    Icon::Film,
                    "From a video",
                    "Use its subtitle track or its audio",
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.new_menu = false;
                    this.browse_video(cx)
                })),
            )
            .child(
                item(
                    "new-import",
                    Icon::Upload,
                    "Import subtitles",
                    "SRT, VTT or ASS",
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.new_menu = false;
                    this.browse_subtitles(cx)
                })),
            );
        wrap = wrap.child(
            deferred(
                anchored()
                    .anchor(Corner::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu),
            )
            .with_priority(2),
        );
        wrap
    }

    // ─── text fields ───────────────────────────────────────

    /// The `.input` frame around a gpui-component editor: 36px, 1px line,
    /// accent ring while focused, danger border when invalid.
    pub fn text_field(
        &self,
        state: &Entity<InputState>,
        prefix: Option<Icon>,
        bad: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let focused = state.read(cx).focus_handle(cx).is_focused(window);
        let border = if bad {
            t::danger()
        } else if focused {
            t::accent()
        } else {
            t::line()
        };
        row()
            .h(px(36.))
            .w_full()
            .rounded(t::R)
            .border_1()
            .border_color(border)
            .bg(t::surface())
            .when(focused, |d| d.shadow(vec![ring_shadow()]))
            .when(!focused && !bad, |d| {
                d.hover(|s| s.border_color(t::surface_3()))
            })
            .when_some(prefix, |d, ic| {
                d.pl(px(10.)).child(icon(ic, px(15.), t::text_3()))
            })
            .child(
                Input::new(state)
                    .appearance(false)
                    .h(px(34.))
                    .px(px(if prefix.is_some() { 8. } else { 11. }))
                    .text_size(t::T_CONTROL),
            )
    }

    // ─── selects ───────────────────────────────────────────

    pub fn render_select(
        &self,
        sel: Sel,
        min_w: Pixels,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let options = self.select_options(sel);
        let value = self.select_value(sel);
        let label = options
            .iter()
            .find(|(k, _)| *k == value)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| value.clone());
        let open = self.open_select == Some(sel) && !disabled;
        let id = SharedString::from(format!("sel-{sel:?}"));

        let trigger = row()
            .id(id.clone())
            .h(px(36.))
            .min_w(min_w)
            .w_full()
            .pl(px(11.))
            .pr(px(10.))
            .gap(px(8.))
            .rounded(t::R)
            .border_1()
            .text_size(t::T_CONTROL)
            .map(|d| {
                if disabled {
                    d.bg(t::surface_2())
                        .border_color(t::line())
                        .text_color(t::text_3())
                        .cursor(gpui::CursorStyle::OperationNotAllowed)
                } else {
                    d.bg(t::surface())
                        .border_color(if open { t::accent() } else { t::line() })
                        .text_color(t::text())
                        .cursor_pointer()
                        .when(open, |d| d.shadow(vec![ring_shadow()]))
                        .hover(|s| s.border_color(if open { t::accent() } else { t::surface_3() }))
                }
            })
            .child(
                fill()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label),
            )
            .child(icon(Icon::Chevron, px(14.), t::text_3()))
            .when(!disabled, |d| {
                d.on_click(cx.listener(move |this, _, _, cx| this.toggle_select(sel, cx)))
            });

        let mut wrap = col().min_w_0().child(trigger);
        if open {
            let items = options.into_iter().enumerate().map(|(ix, (key, text))| {
                let selected = key == value;
                row()
                    .id(SharedString::from(format!("{id}-opt-{ix}")))
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(t::R_SM)
                    .cursor_pointer()
                    .text_size(t::T_CONTROL)
                    .text_color(if selected {
                        t::accent_text()
                    } else {
                        t::text()
                    })
                    .hover(|s| s.bg(t::surface_3()))
                    .child(
                        fill()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(text),
                    )
                    .when(selected, |d| {
                        d.child(icon(Icon::Check, px(14.), t::accent()))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick(sel, key.clone(), window, cx)
                    }))
            });
            wrap = wrap.child(
                deferred(
                    anchored()
                        .anchor(Corner::TopLeft)
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            col()
                                .id(SharedString::from(format!("{id}-menu")))
                                .occlude()
                                .mt(px(4.))
                                .min_w(min_w.max(px(160.)))
                                .max_w(px(360.))
                                .max_h(px(320.))
                                .overflow_y_scroll()
                                .p(px(4.))
                                .gap(px(1.))
                                .rounded(t::R)
                                .bg(t::surface_2())
                                .border_1()
                                .border_color(t::line())
                                .shadow(vec![gpui::BoxShadow {
                                    color: t::toast_shadow(),
                                    offset: gpui::point(px(0.), px(8.)),
                                    blur_radius: px(24.),
                                    spread_radius: px(0.),
                                }])
                                .on_mouse_down_out(
                                    cx.listener(|this, _, _, cx| this.dismiss_select(cx)),
                                )
                                .children(items),
                        ),
                )
                .with_priority(2),
            );
        }
        wrap
    }

    // ─── dialogs ───────────────────────────────────────────

    fn render_dialog(
        &self,
        bp: Bp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let width = match dialog {
            Dialog::Engines { .. } => 580.,
            Dialog::Term { .. } => 520.,
            Dialog::Capture | Dialog::Extract | Dialog::Export => 480.,
            Dialog::Confirm { .. } | Dialog::Rename { .. } | Dialog::Translate => 400.,
            Dialog::Summarize { .. } | Dialog::About | Dialog::Correct => 440.,
        };
        let max_w = px(f32::min(width, bp.w - 32.));
        let mut extra: Option<AnyElement> = None;

        let close_btn = |cx: &mut Context<Self>| {
            button("dlg-x", Btn::Icon, Some(Icon::X), None, false)
                .on_click(cx.listener(|this, _, window, cx| this.close_dialog(window, cx)))
        };
        let cancel_label = if matches!(dialog, Dialog::About) {
            "Close"
        } else {
            "Cancel"
        };
        let cancel = |cx: &mut Context<Self>| {
            button("dlg-cancel", Btn::Ghost, None, Some(cancel_label), false)
                .on_click(cx.listener(|this, _, window, cx| this.close_dialog(window, cx)))
        };
        let title_el = |text: String| {
            fill()
                .text_size(t::T_HEAD)
                .font_weight(weight(600.))
                .line_height(relative(1.3))
                .child(text)
        };
        let form_field = |label_el: Div, control: AnyElement, extra: Vec<AnyElement>| {
            col()
                .gap(px(6.))
                .mb(px(18.))
                .child(label_el)
                .child(control)
                .children(extra)
        };
        let req_label = |text: &str| label(text.to_string());

        let (head, body, ok): (Div, Div, gpui::Stateful<Div>) = match dialog {
            Dialog::Confirm {
                title,
                text,
                ok_label,
                ..
            } => (
                row().gap(px(10.)).child(title_el(title.clone())),
                div().child(
                    div()
                        .text_size(px(15.))
                        .text_color(t::text_2())
                        .line_height(relative(1.6))
                        .child(text.clone()),
                ),
                button("dlg-ok", Btn::Danger, None, Some(ok_label), false)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_dialog(window, cx))),
            ),
            Dialog::Rename {
                speaker, note, err, ..
            } => (
                row()
                    .gap(px(10.))
                    .child(title_el(match speaker {
                        Some(n) => format!("Name speaker {}", n + 1),
                        None => "Rename this project".into(),
                    }))
                    .child(close_btn(cx)),
                div()
                    .child(form_field(
                        req_label(if speaker.is_some() {
                            "Name"
                        } else {
                            "Project name"
                        }),
                        self.text_field(&self.rename_input, None, err.is_some(), window, cx)
                            .into_any_element(),
                        err.iter()
                            .map(|e| err_text(e.clone()).into_any_element())
                            .collect(),
                    ))
                    .child(
                        div()
                            .text_size(px(15.))
                            .text_color(t::text_2())
                            .line_height(relative(1.6))
                            .child(note.clone()),
                    ),
                button("dlg-ok", Btn::Primary, None, Some("Save name"), false)
                    .on_click(cx.listener(|this, _, window, cx| this.rename_project(window, cx))),
            ),
            Dialog::Translate => {
                let from = self
                    .current()
                    .map(|p| lang_name(&p.from))
                    .unwrap_or_default();
                (
                    row()
                        .gap(px(10.))
                        .child(title_el("Add a translation".into()))
                        .child(close_btn(cx)),
                    col()
                        .gap(px(16.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .text_color(t::text_2())
                                .line_height(relative(1.6))
                                .child(format!("Every line is spoken in {from}. Each one gets a translation next to it, using the glossary.")),
                        )
                        .child(field("Translate to", self.render_select(Sel::TrTo, px(200.), false, cx)))
                        .when(!self.settings.mt_ready(), |d| {
                            d.child(
                                row()
                                    .flex_wrap()
                                    .text_size(t::T_SMALL)
                                    .text_color(t::warn_text())
                                    .child("Translation is not set up yet.\u{00A0}")
                                    .child(
                                        link_button("tr-engines", "Set up engines")
                                            .on_click(cx.listener(|this, _, w, cx| this.open_engines(w, cx))),
                                    ),
                            )
                        }),
                    button("dlg-ok", Btn::Primary, Some(Icon::Translate), Some("Translate"), false)
                        .on_click(cx.listener(|this, _, w, cx| this.translate_now(w, cx))),
                )
            }
            Dialog::Summarize {
                long,
                custom,
                looking,
                lookup_error,
                ..
            } => {
                let endpoint = self
                    .settings
                    .mt_url
                    .trim()
                    .trim_end_matches('/')
                    .to_string();
                let ready = self.settings.mt_ready();
                let named = !self.sum_model.read(cx).value().trim().is_empty();
                let blocked = !ready || (*custom && !named);
                let long = *long;
                (
                    row()
                        .gap(px(10.))
                        .child(title_el("Summarize".into()))
                        .child(close_btn(cx)),
                    col()
                        .gap(px(16.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .text_color(t::text_2())
                                .line_height(relative(1.6))
                                .child("An account of these lines. Choose the language, the model and how long it should be."),
                        )
                        .child(field("Language", self.render_select(Sel::SumLang, px(220.), false, cx)))
                        .child(field("Model", self.render_select(Sel::SumModel, px(220.), false, cx)))
                        .when(*custom, |d| {
                            d.child(field(
                                "Model name",
                                self.text_field(&self.sum_model, None, false, window, cx),
                            ))
                        })
                        .when(*looking, |d| {
                            d.child(hint("Looking up models on this endpoint…"))
                        })
                        .when_some(lookup_error.clone(), |d, e| {
                            d.child(hint(format!("Could not list models ({e}). You can still use the model above, or type another.")))
                        })
                        .child(field(
                            "Length",
                            segmented()
                                .child(
                                    seg_button("sum-short", None, "A few sentences", !long, false)
                                        .on_click(cx.listener(|this, _, _, cx| this.set_summary_length(false, cx))),
                                )
                                .child(
                                    seg_button("sum-long", None, "A fuller account", long, false).on_click(
                                        cx.listener(|this, _, _, cx| this.set_summary_length(true, cx)),
                                    ),
                                ),
                        ))
                        .child(hint(format!("Sent to {endpoint}. The endpoint and its key are set in Engines.")))
                        .when(!ready, |d| {
                            d.child(
                                row()
                                    .flex_wrap()
                                    .text_size(t::T_SMALL)
                                    .text_color(t::warn_text())
                                    .child("The endpoint is not set up yet.\u{00A0}")
                                    .child(
                                        link_button("sum-engines", "Set up engines")
                                            .on_click(cx.listener(|this, _, w, cx| this.open_engines(w, cx))),
                                    ),
                            )
                        }),
                    button("dlg-ok", Btn::Primary, Some(Icon::Summary), Some("Summarize"), blocked)
                        .when(!blocked, |d| {
                            d.on_click(cx.listener(|this, _, w, cx| this.summarize_now(w, cx)))
                        }),
                )
            }
            Dialog::Capture => (
                row()
                    .gap(px(10.))
                    .child(title_el("Live caption".into()))
                    .child(close_btn(cx)),
                self.capture_body(window, cx),
                {
                    let busy = self.live.starting;
                    button(
                        "dlg-ok",
                        Btn::Primary,
                        Some(Icon::Play),
                        Some(if busy { "Starting…" } else { "Start" }),
                        busy,
                    )
                    .when(!busy, |d| {
                        d.on_click(cx.listener(|this, _, w, cx| this.start_live(w, cx)))
                    })
                },
            ),
            Dialog::Extract => (
                row()
                    .gap(px(10.))
                    .child(title_el("From a video".into()))
                    .child(close_btn(cx)),
                self.extract_body(bp, cx),
                {
                    let can = self.extract.track.is_some();
                    button("dlg-ok", Btn::Primary, None, Some("Start"), !can).when(can, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| this.extract_run(cx)))
                    })
                },
            ),
            Dialog::Export => {
                let built = self.build_export();
                let can = built.as_ref().is_some_and(|b| b.count > 0);
                extra = Some(
                    button("exp-copy", Btn::Ghost, Some(Icon::Copy), Some("Copy"), !can)
                        .when(can, |d| {
                            d.on_click(cx.listener(|this, _, _, cx| this.copy_export(cx)))
                        })
                        .into_any_element(),
                );
                (
                    row()
                        .gap(px(10.))
                        .child(title_el("Export".into()))
                        .child(close_btn(cx)),
                    self.export_body(built.as_ref(), cx),
                    button(
                        "dlg-ok",
                        Btn::Primary,
                        Some(Icon::Download),
                        Some("Save file…"),
                        !can,
                    )
                    .when(can, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| this.save_export(cx)))
                    }),
                )
            }
            Dialog::Engines { .. } => (
                row()
                    .gap(px(10.))
                    .child(title_el("Engines".into()))
                    .child(close_btn(cx)),
                self.engines_body(window, cx),
                button("dlg-ok", Btn::Primary, None, Some("Save"), false)
                    .on_click(cx.listener(|this, _, window, cx| this.save_engines(window, cx))),
            ),
            Dialog::About => (
                row()
                    .gap(px(10.))
                    .child(title_el("About Taurus".into()))
                    .child(close_btn(cx)),
                self.about_body(cx),
                self.update_action(cx),
            ),
            Dialog::Correct => {
                let ready = self.settings.mt_ready();
                let (lang, translated) = self
                    .current()
                    .map(|p| (lang_name(&p.from), p.translate))
                    .unwrap_or_default();
                let endpoint = self
                    .settings
                    .mt_url
                    .trim()
                    .trim_end_matches('/')
                    .to_string();
                let model = self.settings.mt_model.trim().to_string();
                (
                    row()
                        .gap(px(10.))
                        .child(title_el("Correct the lines".into()))
                        .child(close_btn(cx)),
                    col()
                        .gap(px(16.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .text_color(t::text_2())
                                .line_height(relative(1.6))
                                .child(format!(
                                    "Reads the {lang} lines in order and suggests fixes for words the speech engine misheard. Nothing changes until you accept a fix{}.",
                                    if translated { ", and the lines you accept are translated again" } else { "" }
                                )),
                        )
                        .child(field(
                            "What is it about? (optional)",
                            self.text_field(&self.correct_about, None, false, window, cx),
                        ))
                        .child(hint(
                            "The topic and the names that come up help the most. Glossary terms count as correct spellings.",
                        ))
                        .child(hint(format!("Sent to {endpoint} with {model}. Both are set in Engines.")))
                        .when(!ready, |d| {
                            d.child(
                                row()
                                    .flex_wrap()
                                    .text_size(t::T_SMALL)
                                    .text_color(t::warn_text())
                                    .child("The endpoint is not set up yet.\u{00A0}")
                                    .child(
                                        link_button("fix-engines", "Set up engines")
                                            .on_click(cx.listener(|this, _, w, cx| this.open_engines(w, cx))),
                                    ),
                            )
                        }),
                    button("dlg-ok", Btn::Primary, Some(Icon::Wand), Some("Correct"), !ready)
                        .when(ready, |d| d.on_click(cx.listener(|this, _, w, cx| this.correct_now(w, cx)))),
                )
            }
            Dialog::Term {
                editing,
                rule,
                term_err,
                target_err,
                ..
            } => {
                let needs_target = *rule != Rule::Keep;
                let target_label = if *rule == Rule::Forbid {
                    "Never translate as"
                } else {
                    "Becomes"
                };
                (
                    row()
                        .gap(px(10.))
                        .child(title_el(if editing.is_some() {
                            "Edit term".into()
                        } else {
                            "Add a term".into()
                        }))
                        .child(close_btn(cx)),
                    div()
                        .child(form_field(
                            label("Rule"),
                            self.render_select(Sel::TermRule, px(200.), false, cx)
                                .into_any_element(),
                            vec![hint(rule.hint()).into_any_element()],
                        ))
                        .child(form_field(
                            req_label("Term"),
                            self.text_field(&self.term_input, None, term_err.is_some(), window, cx)
                                .into_any_element(),
                            term_err
                                .iter()
                                .map(|e| err_text(e.clone()).into_any_element())
                                .collect(),
                        ))
                        .when(needs_target, |d| {
                            d.child(form_field(
                                req_label(target_label),
                                self.text_field(
                                    &self.target_input,
                                    None,
                                    target_err.is_some(),
                                    window,
                                    cx,
                                )
                                .into_any_element(),
                                target_err
                                    .iter()
                                    .map(|e| err_text(e.clone()).into_any_element())
                                    .collect(),
                            ))
                        })
                        .child(
                            col()
                                .gap(px(6.))
                                .child(label("Applies to"))
                                .child(self.render_select(Sel::TermScope, px(200.), false, cx)),
                        ),
                    button("dlg-ok", Btn::Primary, None, Some("Save term"), false)
                        .on_click(cx.listener(|this, _, window, cx| this.save_term(window, cx))),
                )
            }
        };

        let panel = col()
            .id("dlg")
            .w(max_w)
            .max_h(relative(0.9))
            .rounded(t::R_LG)
            .border_1()
            .border_color(t::line())
            .bg(t::surface())
            .shadow(vec![gpui::BoxShadow {
                color: t::toast_shadow(),
                offset: gpui::point(px(0.), px(16.)),
                blur_radius: px(48.),
                spread_radius: px(0.),
            }])
            .child(head.px(px(20.)).pt(px(16.)).pb(px(4.)))
            .child(
                with_scroll_bar(
                    "dlg-frame",
                    div()
                        .id("dlg-body")
                        .px(px(20.))
                        .py(px(12.))
                        .overflow_y_scroll()
                        .track_scroll(&self.dialog_scroll)
                        .child(body),
                    &self.dialog_scroll,
                )
                .min_h_0(),
            )
            .child(
                row()
                    .gap(px(8.))
                    .px(px(20.))
                    .pt(px(8.))
                    .pb(px(18.))
                    .children(extra)
                    .child(fill())
                    .child(cancel(cx))
                    .child(ok),
            )
            .with_animation(
                "dlg-in",
                Animation::new(Duration::from_millis(200)).with_easing(gpui::ease_out_quint()),
                |d, delta| d.opacity(delta).mt(px(6. * (1. - delta))),
            );

        Some(
            div()
                .id("dlg-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(t::backdrop())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(panel)
                .into_any_element(),
        )
    }

    /// What Taurus is, which version this is, and whether there is a newer one.
    fn about_body(&self, cx: &mut Context<Self>) -> Div {
        let (status, color) = self.update_status();
        let installing = matches!(self.update, UpdateState::Installing { .. });
        col()
            .gap(px(18.))
            .child(
                row()
                    .gap(px(14.))
                    .child(img(APP_ICON).size(px(56.)).flex_none())
                    .child(
                        col()
                            .gap(px(2.))
                            .child(div().text_size(px(17.)).font_weight(weight(620.)).child("Taurus"))
                            .child(
                                mono(div())
                                    .text_size(t::T_SMALL)
                                    .text_color(t::text_2())
                                    .child(format!("Version {}", update::version())),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(15.))
                    .text_color(t::text_2())
                    .line_height(relative(1.6))
                    .child("A subtitle workbench for macOS: caption live audio, transcribe videos, tell speakers apart, and translate, all on your own Mac."),
            )
            .child(
                col()
                    .gap(px(10.))
                    .child(label("Updates"))
                    .child(
                        row()
                            .items_start()
                            .gap(px(12.))
                            .child(
                                fill()
                                    .text_size(t::T_CONTROL)
                                    .line_height(relative(1.5))
                                    .text_color(color)
                                    .child(status),
                            )
                            .when(installing, |d| {
                                d.child(
                                    link_button("upd-stop", "Stop")
                                        .text_size(t::T_CONTROL)
                                        .on_click(cx.listener(|this, _, _, cx| this.stop_update(cx))),
                                )
                            }),
                    )
                    .when(update::AUTOMATIC, |d| {
                        d.child(
                            switch("upd-auto", self.settings.auto_update, "Update automatically").on_click(
                                cx.listener(|this, _, _, cx| {
                                    let on = !this.settings.auto_update;
                                    this.set_auto_update(on, cx)
                                }),
                            ),
                        )
                        .child(hint(
                            "Looks for a new version once a day and installs it in the background, ready for the next time Taurus starts.",
                        ))
                    }),
            )
            .child(
                row()
                    .gap(px(16.))
                    .text_size(t::T_SMALL)
                    .child(
                        link_button("about-notes", "Release notes")
                            .on_click(cx.listener(|this, _, _, cx| this.open_release_page(cx))),
                    )
                    .child(link_button("about-repo", "GitHub").on_click(cx.listener(|_, _, _, cx| cx.open_url(update::REPO)))),
            )
    }

    /// A sentence on where updating has got to, and the colour to say it in.
    fn update_status(&self) -> (String, gpui::Hsla) {
        match &self.update {
            UpdateState::Idle => match self.settings.update_checked {
                0 => (
                    "Taurus has not looked for a new version yet.".into(),
                    t::text_2(),
                ),
                at => {
                    let when = when_label(at * 1000);
                    let when = if ["Just now", "Today", "Yesterday"]
                        .iter()
                        .any(|w| when.starts_with(w))
                    {
                        when.to_lowercase()
                    } else {
                        when
                    };
                    (
                        format!("Last looked for a new version {when}."),
                        t::text_2(),
                    )
                }
            },
            UpdateState::Checking => ("Looking for a new version…".into(), t::text_2()),
            UpdateState::UpToDate => ("Taurus is up to date.".into(), t::text_2()),
            UpdateState::Available {
                release,
                can_install: true,
            } => (
                format!("Taurus {} is available.", release.version),
                t::accent_text(),
            ),
            UpdateState::Available {
                release,
                can_install: false,
            } => {
                let why = if !release.installable() {
                    "Download it from GitHub."
                } else if self.app_bundle.is_none() {
                    "This build cannot replace itself, so download it from GitHub."
                } else {
                    "Taurus cannot replace itself where it is now. Move it to the Applications folder, or download this version from GitHub."
                };
                (
                    format!("Taurus {} is available. {why}", release.version),
                    t::warn_text(),
                )
            }
            UpdateState::Installing { release, pct, .. } => (
                format!(
                    "Downloading Taurus {}… {:.0}%",
                    release.version,
                    pct * 100.0
                ),
                t::text_2(),
            ),
            UpdateState::Ready(version) => (
                format!("Taurus {version} is installed. Restart to start using it."),
                t::accent_text(),
            ),
            UpdateState::Failed(e) => (
                format!("Could not update: {}.", e.trim_end_matches('.')),
                t::danger_text(),
            ),
        }
    }

    /// The About dialog's main button: the next step in updating.
    fn update_action(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let busy = |text: &str| button("dlg-ok", Btn::Primary, None, Some(text), true);
        match &self.update {
            UpdateState::Checking => busy("Checking…"),
            UpdateState::Installing { .. } => busy("Installing…"),
            UpdateState::Available {
                release,
                can_install: true,
            } => button(
                "dlg-ok",
                Btn::Primary,
                Some(Icon::Download),
                Some(&format!("Install {}", release.version)),
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.install_update(cx))),
            UpdateState::Available {
                can_install: false, ..
            } => button(
                "dlg-ok",
                Btn::Primary,
                Some(Icon::Download),
                Some("Download"),
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.open_release_page(cx))),
            UpdateState::Ready(_) => button(
                "dlg-ok",
                Btn::Primary,
                Some(Icon::Refresh),
                Some("Restart to update"),
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.restart_to_update(cx))),
            UpdateState::Idle | UpdateState::UpToDate | UpdateState::Failed(_) => button(
                "dlg-ok",
                Btn::Primary,
                Some(Icon::Refresh),
                Some("Check for updates"),
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.check_for_update(cx))),
        }
    }

    fn engines_body(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let Some(Dialog::Engines {
            asr,
            whisper_model,
            qwen_local_model,
            asr_test,
            mt_test,
            testing_asr,
            testing_mt,
        }) = &self.dialog
        else {
            return div();
        };
        let section = |title: &str| {
            div()
                .mb(px(10.))
                .text_size(px(15.))
                .font_weight(weight(600.))
                .child(title.to_string())
        };
        let input = |label_text: &str, state: &Entity<InputState>, cx: &Context<Self>| {
            col()
                .gap(px(6.))
                .mb(px(12.))
                .child(label(label_text.to_string()))
                .child(self.text_field(state, None, false, window, cx))
        };
        let test_row = |id: &'static str,
                        busy: bool,
                        result: &Option<Result<String, String>>,
                        cx: &mut Context<Self>,
                        on: fn(&mut Workbench, &mut Context<Workbench>)| {
            let msg = if busy {
                Some(hint("Testing…").into_any_element())
            } else {
                result.as_ref().map(|r| match r {
                    Ok(m) => div()
                        .text_size(t::T_SMALL)
                        .text_color(t::ok())
                        .child(m.clone())
                        .into_any_element(),
                    Err(e) => err_text(format!("{e}.")).into_any_element(),
                })
            };
            row()
                .gap(px(12.))
                .items_center()
                .child(
                    button(id, Btn::Ghost, None, Some("Test"), busy).when(!busy, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| on(this, cx)))
                    }),
                )
                .child(div().flex_1().min_w_0().children(msg))
        };

        let choice = *asr;
        let option = |id: &'static str,
                      value: AsrChoice,
                      title: &'static str,
                      sub: &'static str,
                      cx: &mut Context<Self>| {
            radio_row(id, choice == value, title, radio_sub(sub))
                .on_click(cx.listener(move |this, _, _, cx| this.set_engine_asr(value, cx)))
        };
        let mut speech = col()
            .gap(px(6.))
            .mb(px(12.))
            .child(option(
                "asr-whisper",
                AsrChoice::Whisper,
                "Whisper, on this Mac",
                "whisper.cpp with Metal. Nothing leaves the machine; the model downloads once.",
                cx,
            ))
            .child(option(
                "asr-qwen-local",
                AsrChoice::QwenLocal,
                "Qwen3-ASR, on this Mac",
                "ONNX Runtime on the CPU. Stronger on Chinese and noisy audio; a larger download.",
                cx,
            ))
            .child(option(
                "asr-qwen",
                AsrChoice::Qwen,
                "Qwen3-ASR, on a server",
                "Any OpenAI-compatible /audio/transcriptions endpoint, such as vLLM serving Qwen/Qwen3-ASR-1.7B.",
                cx,
            ));

        if choice == AsrChoice::Whisper {
            let models = col()
                .gap(px(6.))
                .children(crate::engine::WHISPER_MODELS.iter().map(|m| {
                    let id = m.id;
                    self.model_row(
                        id.to_string(),
                        m.id,
                        *whisper_model == m.id,
                        m.size,
                        m.note,
                        m.is_downloaded(),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.set_whisper_model(id, cx)))
                }));
            speech = speech
                .child(div().mt(px(8.)).child(label("Model")))
                .child(models);
        }
        if choice == AsrChoice::QwenLocal {
            let models = col()
                .gap(px(6.))
                .children(crate::engine::QWEN_MODELS.iter().map(|m| {
                    let id = m.id;
                    self.model_row(
                        m.download_id(),
                        m.label,
                        *qwen_local_model == m.id,
                        m.size,
                        m.note,
                        m.is_downloaded(),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.set_qwen_model(id, cx)))
                }));
            speech = speech
                .child(div().mt(px(8.)).child(label("Model")))
                .child(models);
        }

        let mut body = col().child(section("Speech recognition")).child(speech);
        if choice == AsrChoice::Qwen {
            body = body
                .child(input("Server address", &self.eng.qwen_url, cx))
                .child(input("Model", &self.eng.qwen_model, cx))
                .child(input("API key", &self.eng.qwen_key, cx));
        }
        body = body
            .child(div().mb(px(22.)).child(test_row("asr-test", *testing_asr, asr_test, cx, |this, cx| this.test_asr(cx))))
            .child(section("Translation"))
            .child(
                div()
                    .mb(px(12.))
                    .child(hint("Any OpenAI-compatible chat endpoint: Ollama, LM Studio, vLLM, OpenRouter or OpenAI. Glossary rules go into every request.")),
            )
            .child(input("Endpoint", &self.eng.mt_url, cx))
            .child(input("Model", &self.eng.mt_model, cx))
            .child(input("API key", &self.eng.mt_key, cx))
            .child(test_row("mt-test", *testing_mt, mt_test, cx, |this, cx| this.test_mt(cx)));
        body
    }

    /// One downloadable model: a radio for choosing it, and a Download or
    /// Cancel button. Clicking the row is left to the caller.
    #[allow(clippy::too_many_arguments)]
    fn model_row(
        &self,
        dl_id: String,
        title: &'static str,
        on: bool,
        size: &str,
        note: &str,
        have: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let dl = self.downloads.get(&dl_id);
        let state = match (have, dl) {
            (_, Some(d)) => format!("downloading {:.0}%", d.pct * 100.0),
            (true, None) => "downloaded".into(),
            (false, None) => "not downloaded".into(),
        };
        let action = match (have, dl.is_some()) {
            (_, true) => {
                let id = dl_id.clone();
                button(
                    SharedString::from(format!("dl-x-{dl_id}")),
                    Btn::Ghost,
                    None,
                    Some("Cancel"),
                    false,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.cancel_download(&id, cx)))
                .into_any_element()
            }
            (false, false) => {
                let id = dl_id.clone();
                button(
                    SharedString::from(format!("dl-{dl_id}")),
                    Btn::Ghost,
                    Some(Icon::Download),
                    Some("Download"),
                    false,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.download_model(&id, cx)))
                .into_any_element()
            }
            (true, false) => div().into_any_element(),
        };
        row()
            .id(SharedString::from(format!("model-{dl_id}")))
            .gap(px(8.))
            .child(
                radio_row(
                    SharedString::from(format!("pick-{dl_id}")),
                    on,
                    title,
                    mono(div())
                        .text_size(t::T_SMALL)
                        .text_color(if have { t::text_2() } else { t::text_3() })
                        .child(format!("{size} · {note} · {state}")),
                )
                .flex_1()
                .min_w_0(),
            )
            .child(
                div()
                    .w(px(112.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .child(action),
            )
    }

    // ─── toasts ────────────────────────────────────────────

    fn render_toasts(&self, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let toasts = self.toasts.iter().map(|toast| {
            let (ic, color) = match toast.kind {
                ToastKind::Ok => (Icon::Check, t::accent()),
                ToastKind::Warn => (Icon::Alert, t::warn()),
                ToastKind::Bad => (Icon::Alert, t::danger()),
            };
            let id = toast.id;
            row()
                .id(("toast", id as usize))
                .occlude()
                .gap(px(11.))
                .when(!bp.mobile(), |d| d.min_w(px(268.)).max_w(px(380.)))
                .px(px(14.))
                .py(px(11.))
                .rounded(t::R)
                .border_1()
                .border_color(t::line())
                .bg(t::surface_2())
                .shadow(vec![gpui::BoxShadow {
                    color: t::toast_shadow(),
                    offset: gpui::point(px(0.), px(8.)),
                    blur_radius: px(24.),
                    spread_radius: px(0.),
                }])
                .text_size(t::T_CONTROL)
                .child(icon(ic, px(16.), color))
                .child(fill().line_height(relative(1.45)).child(toast.msg.clone()))
                .when_some(toast.action.as_ref(), |d, (text, _)| {
                    d.child(
                        div()
                            .id(("toast-act", id as usize))
                            .flex_none()
                            .cursor_pointer()
                            .text_size(px(14.))
                            .font_weight(weight(600.))
                            .text_color(t::accent())
                            .underline()
                            .hover(|s| s.text_color(t::link_hover()))
                            .child(text.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run_toast_action(id, window, cx)
                            })),
                    )
                })
                .with_animation(
                    ("toast-in", id as usize),
                    Animation::new(Duration::from_millis(220)).with_easing(gpui::ease_out_quint()),
                    |d, delta| d.opacity(delta).mt(px(8. * (1. - delta))),
                )
        });
        let inset = if bp.mobile() { px(12.) } else { px(20.) };
        col()
            .absolute()
            .bottom(inset)
            .right(inset)
            .when(bp.mobile(), |d| d.left(inset))
            .items_end()
            .gap(px(8.))
            .children(toasts)
            .into_any_element()
    }
}

pub fn ring_shadow() -> gpui::BoxShadow {
    gpui::BoxShadow {
        color: t::accent(),
        offset: gpui::point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(1.),
    }
}

/// A coloured dot and a word or two in the top bar, for state worth a look.
fn status_button(id: &'static str, dot: gpui::Hsla, text: String, bp: Bp) -> gpui::Stateful<Div> {
    row()
        .id(id)
        .flex_none()
        .h(px(36.))
        .px(px(10.))
        .gap(px(8.))
        .rounded(t::R)
        .cursor_pointer()
        .text_size(t::T_CONTROL)
        .text_color(t::text_2())
        .hover(|s| s.bg(t::surface_2()).text_color(t::text()))
        .child(div().size(px(7.)).rounded_full().bg(dot))
        .when(!bp.tiny(), |d| d.child(text))
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bp = Bp::of(window);
        let main = match self.view {
            View::Projects => self.render_projects(bp, window, cx),
            View::Editor => self.render_editor(bp, window, cx),
            View::Glossary => self.render_glossary(bp, window, cx),
        };
        let dialog = self.render_dialog(bp, window, cx);
        let toasts = self.render_toasts(bp, cx);

        div()
            .id("workbench")
            .track_focus(&self.focus)
            .key_context("Workbench")
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let k = &ev.keystroke;
                let bare = !(k.modifiers.platform || k.modifiers.control || k.modifiers.alt);
                if k.key == "space"
                    && bare
                    && this.view == View::Editor
                    && this.dialog.is_none()
                    && this.editor.editing.is_none()
                {
                    this.play(None, cx);
                    cx.stop_propagation();
                    return;
                }
                if k.key != "escape" {
                    return;
                }
                if this.open_select.is_some() || this.new_menu {
                    this.open_select = None;
                    this.new_menu = false;
                    cx.stop_propagation();
                    cx.notify();
                } else if this.dialog.is_some() {
                    this.close_dialog(window, cx);
                    cx.stop_propagation();
                } else if this.editor.editing.is_some() {
                    this.stop_editing(window, cx);
                    cx.stop_propagation();
                }
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t::bg())
            .text_color(t::text())
            .text_size(px(16.))
            .line_height(relative(1.55))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(t::accent_wash()))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.drop_paths(paths.paths().to_vec(), cx)
            }))
            .child(self.render_topbar(bp, cx))
            .child(div().flex_1().min_h_0().overflow_hidden().child(main))
            .children(dialog)
            .child(toasts)
    }
}
