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

const SEARCH_FROM: usize = 10;

impl Workbench {
    pub(crate) fn render_glossary(
        &mut self,
        bp: Bp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.glossary.is_empty() {
            return screen()
                .child(empty_state(
                    "No terms yet",
                    "Add the names, product words and units you keep correcting by hand. Every translation follows them.",
                    vec![button("gl-empty-add", Btn::Primary, Some(Icon::Plus), Some("Add term"), false)
                        .on_click(cx.listener(|this, _, w, cx| this.open_term_dialog(None, w, cx)))
                        .into_any_element()],
                ))
                .into_any_element();
        }

        let terms: Vec<Term> = self.visible_terms(cx).into_iter().cloned().collect();
        let none_match = terms.is_empty();
        let rows: Vec<AnyElement> = terms.iter().map(|g| self.term_row(g, bp, cx)).collect();
        let search = (self.glossary.len() >= SEARCH_FROM).then(|| {
            div()
                .w(px(260.))
                .mb(px(8.))
                .child(self.text_field(&self.gl_search, Some(Icon::Search), false, window, cx))
        });

        screen()
            .child(
                div()
                    .id("gl-scroll")
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

    fn term_row(&self, g: &Term, bp: Bp, cx: &mut Context<Self>) -> AnyElement {
        let group = SharedString::from(format!("gl-{}", g.id));
        let (e, d) = (g.id.clone(), g.id.clone());
        let target = match g.rule {
            Rule::Keep => div().text_color(t::text_3()).child("kept as is"),
            Rule::Forbid => div()
                .text_color(t::danger_text())
                .child(format!("never “{}”", g.target)),
            Rule::Prefer => div().text_color(t::accent_text()).child(g.target.clone()),
        };
        let mut meta = Vec::new();
        if let Scope::Project(_) = g.scope {
            meta.push(self.scope_label(&g.scope));
        }
        if g.hits > 0 {
            meta.push(format!("used {}×", g.hits));
        }
        let delete = button(
            SharedString::from(format!("gl-del-{}", g.id)),
            Btn::IconDanger,
            Some(Icon::Trash),
            None,
            false,
        )
        .when(!bp.mobile(), |r| r.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.ask_delete_term(&d, cx)
        }));

        row()
            .id(SharedString::from(format!("gl-row-{}", g.id)))
            .group(group)
            .gap(px(14.))
            .px(px(12.))
            .py(px(10.))
            .rounded(t::R)
            .cursor_pointer()
            .hover(|s| s.bg(t::surface()))
            .on_click(cx.listener(move |this, _, w, cx| this.open_term_dialog(Some(e.clone()), w, cx)))
            .child(
                row()
                    .flex_1()
                    .min_w_0()
                    .flex_wrap()
                    .gap(px(10.))
                    .text_size(px(15.))
                    .child(div().font_weight(weight(550.)).child(g.term.clone()))
                    .child(div().text_color(t::text_3()).child("→"))
                    .child(target),
            )
            .when(!meta.is_empty(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(t::T_SMALL)
                        .text_color(t::text_3())
                        .whitespace_nowrap()
                        .child(meta.join(" · ")),
                )
            })
            .child(delete)
            .into_any_element()
    }
}
