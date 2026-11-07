mod editor;
mod export;
mod extract;
mod glossary;
mod live;
mod projects;

use gpui::{Div, Styled};

use crate::ui::col;

pub fn screen() -> Div {
    col().size_full().min_h_0()
}
