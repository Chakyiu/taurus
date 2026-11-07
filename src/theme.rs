//! Design tokens from `design/styles.css`: a dark low-chroma tool surface with
//! one cyan accent reserved for live and active state.

use gpui::{App, FontWeight, Hsla, Pixels, px, rgb, rgba};

macro_rules! tokens {
    ($($name:ident = $hex:expr;)*) => {
        $(
            #[inline]
            pub fn $name() -> Hsla {
                rgb($hex).into()
            }
        )*
    };
}

tokens! {
    // surfaces
    bg = 0x0E1216;
    surface = 0x161B21;
    surface_2 = 0x1D242B;
    surface_3 = 0x242D36;
    line = 0x2A333C;
    line_soft = 0x202830;

    // ink
    text = 0xE6EDF3;
    text_2 = 0x9AA9B6;
    text_3 = 0x8191A1;

    // accent + state
    accent = 0x35C8D8;
    accent_hover = 0x4BD8E7;
    accent_dim = 0x1E7F8C;
    accent_ink = 0x04222A;
    accent_wash = 0x12323A;
    accent_text = 0x9BE6F0;
    link_hover = 0x6FE2EF;

    ok = 0x5FBF8B;

    warn = 0xE3A84C;
    warn_text = 0xF0C987;

    danger = 0xE8706A;
    danger_hover = 0xF08A85;
    danger_ink = 0x230B0A;
    danger_wash = 0x3A1B1A;
    danger_text = 0xF6A6A2;
}

pub fn backdrop() -> Hsla {
    rgba(0x04070AAD).into()
}

pub fn toast_shadow() -> Hsla {
    rgba(0x00000066).into()
}

// type scale
pub const T_HEAD: Pixels = px(21.);
pub const T_SMALL: Pixels = px(13.);
pub const T_CONTROL: Pixels = px(14.5);

// radii
pub const R_SM: Pixels = px(5.);
pub const R: Pixels = px(7.);
pub const R_LG: Pixels = px(10.);

pub const MONO: &str = "Menlo";

pub fn weight(w: f32) -> FontWeight {
    FontWeight(w)
}

/// Point the gpui-component text inputs at the same tokens so editable
/// fields read as part of this surface rather than a stock theme.
pub fn apply_component_theme(cx: &mut App) {
    use gpui_component::theme::{Theme, ThemeMode};
    Theme::change(ThemeMode::Dark, None, cx);
    let theme = Theme::global_mut(cx);
    theme.font_size = px(14.5);
    theme.mono_font_family = MONO.into();
    theme.radius = R;
    theme.radius_lg = R_LG;
    theme.shadow = false;
    let c = &mut theme.colors;
    c.background = surface();
    c.foreground = text();
    c.muted = surface_2();
    c.muted_foreground = text_3();
    c.input = line();
    c.border = line();
    c.ring = accent();
    c.caret = accent();
    c.selection = Hsla {
        a: 0.55,
        ..accent_dim()
    };
    c.popover = surface_2();
    c.popover_foreground = text();
    c.scrollbar = gpui::transparent_black();
    c.scrollbar_thumb = surface_3();
    c.scrollbar_thumb_hover = rgb(0x33404B).into();
    c.accent = surface_3();
    c.accent_foreground = text();
    c.primary = accent();
    c.primary_foreground = accent_ink();
}
