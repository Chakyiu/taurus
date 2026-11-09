use gpui::prelude::FluentBuilder as _;
use gpui::{
    Context, Div, InteractiveElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled, canvas, div, px,
    relative,
};

use crate::app::*;
use crate::data::*;
use crate::theme as t;
use crate::ui::*;

impl Workbench {
    pub(crate) fn export_body(&self, built: Option<&Built>, cx: &mut Context<Self>) -> Div {
        let e = &self.exp;
        let formats = segmented().children(Format::ALL.iter().map(|f| {
            let f = *f;
            seg_button(
                SharedString::from(format!("fmt-{f:?}")),
                None,
                &f.ext().to_uppercase(),
                e.format == f,
                false,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.exp.format = f;
                cx.notify();
            }))
        }));
        let layouts = segmented()
            .flex_wrap()
            .children(Layout::ALL.iter().map(|l| {
                let l = *l;
                seg_button(
                    SharedString::from(format!("lay-{l:?}")),
                    None,
                    match l {
                        Layout::Stacked => "Both",
                        Layout::OrigFirst => "Original on top",
                        Layout::Trans => "Translation",
                        Layout::Orig => "Original",
                    },
                    e.layout == l,
                    false,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.exp.layout = l;
                    cx.notify();
                }))
            }));
        let (translated, speakers) = self
            .current()
            .map_or((true, false), |p| (p.translate, p.has_speakers()));
        let options: [(&'static str, bool, &str, fn(&mut ExportState)); 4] = [
            (
                "exp-speakers",
                e.speakers,
                "Start lines with the speaker's name",
                |e| e.speakers = !e.speakers,
            ),
            (
                "exp-merge",
                e.merge,
                "Merge cues less than 120 ms apart",
                |e| e.merge = !e.merge,
            ),
            (
                "exp-flagged",
                e.skip_flagged,
                "Leave out lines still to check",
                |e| e.skip_flagged = !e.skip_flagged,
            ),
            ("exp-bom", e.bom, "Add a UTF-8 byte-order mark", |e| {
                e.bom = !e.bom
            }),
        ];
        let toggles: Vec<_> = options
            .into_iter()
            .filter(|(id, ..)| speakers || *id != "exp-speakers")
            .map(|(id, on, text, flip)| {
                checkbox_row(id, on, text).on_click(cx.listener(move |this, _, _, cx| {
                    flip(&mut this.exp);
                    cx.notify();
                }))
            })
            .collect();
        let summary = match built {
            Some(b) if b.count > 0 => format!("{} cues · {}", b.count, b.filename),
            _ => "Nothing to export with these options.".into(),
        };

        col()
            .gap(px(18.))
            .child(field("Format", formats))
            .when(translated, |d| {
                d.child(field(
                    "Lines",
                    col()
                        .gap(px(6.))
                        .child(layouts)
                        .child(hint(e.layout.description())),
                ))
            })
            .child(self.wrap_slider(cx))
            .child(col().gap(px(8.)).children(toggles))
            .child(
                mono(div())
                    .text_size(t::T_SMALL)
                    .text_color(t::text_2())
                    .child(summary),
            )
    }

    fn wrap_slider(&self, cx: &mut Context<Self>) -> Div {
        let wrap = self.exp.wrap;
        let ratio = (wrap - 24) as f32 / 36.0;
        let bounds = self.exp.wrap_bounds.clone();
        let entity = cx.entity().downgrade();
        let dragging = self.exp.dragging_wrap;

        let track = div()
            .id("exp-wrap")
            .relative()
            .h(px(15.))
            .w_full()
            .cursor_pointer()
            .flex()
            .items_center()
            .child(
                canvas(
                    move |b, _, _| bounds.set(b),
                    move |_, _, window, _| {
                        if !dragging {
                            return;
                        }
                        let e = entity.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, _, _, cx| {
                            e.update(cx, |this, cx| this.set_wrap_from_x(ev.position.x, cx))
                                .ok();
                        });
                        let e = entity.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                            e.update(cx, |this, cx| {
                                this.exp.dragging_wrap = false;
                                cx.notify();
                            })
                            .ok();
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(div().w_full().h(px(3.)).rounded_full().bg(t::surface_3()))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(relative(ratio))
                    .ml(px(-15. * ratio))
                    .size(px(15.))
                    .rounded_full()
                    .bg(t::accent()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.exp.dragging_wrap = true;
                    this.set_wrap_from_x(ev.position.x, cx);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                match ev.keystroke.key.as_str() {
                    "left" | "down" => this.nudge_wrap(-1, cx),
                    "right" | "up" => this.nudge_wrap(1, cx),
                    _ => {}
                }
            }));

        col()
            .gap(px(8.))
            .child(
                row()
                    .justify_between()
                    .gap(px(10.))
                    .child(label("Wrap lines at"))
                    .child(
                        mono(div())
                            .text_size(t::T_SMALL)
                            .text_color(t::text())
                            .child(format!("{wrap} characters")),
                    ),
            )
            .child(track)
    }
}
