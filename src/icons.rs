//! The prototype's 24px stroke icon set, served to gpui as SVG assets.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, Hsla, Pixels, SharedString, Styled, Svg, svg};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Waveform,
    Projects,
    Captions,
    Back,
    Editor,
    Film,
    Tag,
    Download,
    Upload,
    Mic,
    Monitor,
    Crop,
    Swap,
    Play,
    Pause,
    Stop,
    Check,
    Alert,
    X,
    Plus,
    Trash,
    Search,
    Copy,
    Refresh,
    Clock,
    Chevron,
    Arc,
    Users,
    Translate,
    Summary,
}

const ALL: &[Icon] = &[
    Icon::Waveform,
    Icon::Projects,
    Icon::Captions,
    Icon::Back,
    Icon::Editor,
    Icon::Film,
    Icon::Tag,
    Icon::Download,
    Icon::Upload,
    Icon::Mic,
    Icon::Monitor,
    Icon::Crop,
    Icon::Swap,
    Icon::Play,
    Icon::Pause,
    Icon::Stop,
    Icon::Check,
    Icon::Alert,
    Icon::X,
    Icon::Plus,
    Icon::Trash,
    Icon::Search,
    Icon::Copy,
    Icon::Refresh,
    Icon::Clock,
    Icon::Chevron,
    Icon::Arc,
    Icon::Users,
    Icon::Translate,
    Icon::Summary,
];

impl Icon {
    fn name(self) -> &'static str {
        match self {
            Icon::Waveform => "waveform",
            Icon::Projects => "projects",
            Icon::Captions => "captions",
            Icon::Back => "back",
            Icon::Editor => "editor",
            Icon::Film => "film",
            Icon::Tag => "tag",
            Icon::Download => "download",
            Icon::Upload => "upload",
            Icon::Mic => "mic",
            Icon::Users => "users",
            Icon::Translate => "translate",
            Icon::Summary => "summary",
            Icon::Monitor => "monitor",
            Icon::Crop => "crop",
            Icon::Swap => "swap",
            Icon::Play => "play",
            Icon::Pause => "pause",
            Icon::Stop => "stop",
            Icon::Check => "check",
            Icon::Alert => "alert",
            Icon::X => "x",
            Icon::Plus => "plus",
            Icon::Trash => "trash",
            Icon::Search => "search",
            Icon::Copy => "copy",
            Icon::Refresh => "refresh",
            Icon::Clock => "clock",
            Icon::Chevron => "chevron",
            Icon::Arc => "arc",
        }
    }

    fn d(self) -> &'static str {
        match self {
            Icon::Waveform => "M4 10v4M8 7v10M12 3.5v17M16 8v8M20 11v2",
            Icon::Projects => {
                "M3.5 7.5a2 2 0 0 1 2-2h3.1l1.9 2.2h8a2 2 0 0 1 2 2v8.8a2 2 0 0 1-2 2H5.5a2 2 0 0 1-2-2z"
            }
            Icon::Captions => {
                "M3.5 5.5h17v13h-17zM7.6 10.4a2.4 2.4 0 1 0 0 3.2M16.4 10.4a2.4 2.4 0 1 0 0 3.2"
            }
            Icon::Back => "M19.5 12H4.5m0 0 6.3-6.3M4.5 12l6.3 6.3",
            Icon::Editor => {
                "M3.5 6h13M3.5 11h9M3.5 16h6M14 20.5l6.4-6.4a1.7 1.7 0 0 0-2.4-2.4L11.6 18l-.7 3.2z"
            }
            Icon::Film => "M3.5 5.5h17v13h-17zM3.5 9.6h17M3.5 14.4h17M7.8 5.5v13M16.2 5.5v13",
            Icon::Tag => {
                "M12.6 3.2H6.5a3 3 0 0 0-3 3v6.1a2 2 0 0 0 .6 1.5l7.3 7.3a2 2 0 0 0 2.8 0l6.1-6.1a2 2 0 0 0 0-2.8L14 3.8a2 2 0 0 0-1.4-.6zM8.2 8.4h.01"
            }
            Icon::Download => "M12 3.5v11.5m0 0 4.3-4.3M12 15l-4.3-4.3M4 19.5h16",
            Icon::Upload => "M12 20.5V9m0 0 4.3 4.3M12 9l-4.3 4.3M4 4.5h16",
            Icon::Mic => {
                "M12 3.5a3 3 0 0 1 3 3v5a3 3 0 0 1-6 0v-5a3 3 0 0 1 3-3zM5.5 11a6.5 6.5 0 0 0 13 0M12 17.5v3M9 20.5h6"
            }
            Icon::Monitor => "M3.5 4.5h17v11h-17zM9 20.5h6M12 15.5v5",
            Icon::Users => {
                "M9 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7zM2.5 20c0-3.6 2.9-6 6.5-6s6.5 2.4 6.5 6M16 4.3a3.5 3.5 0 0 1 0 6.4M18 14.4c2.1.7 3.5 2.8 3.5 5.6"
            }
            Icon::Translate => "M4 5h9M8.5 3v2M6 5c1 3.5 3.5 6 6.5 7.5M11 5c-1 4-3.5 7.5-7 9.5M12.5 20.5l4-10 4 10M14 17h5",
            Icon::Summary => "M6.5 3.5h7.2L18 8v12.5H6.5zM13.5 3.8V8H18M9 12h6M9 15.2h6M9 18.4h3.6",
            Icon::Crop => "M7.5 3v4.5H3M16.5 21v-4.5H21M3 16.5h4.5V21M21 7.5h-4.5V3",
            Icon::Swap => {
                "M7 6.5h12m0 0-3.2-3.2M19 6.5l-3.2 3.2M17 17.5H5m0 0 3.2-3.2M5 17.5l3.2 3.2"
            }
            Icon::Play => "M8 5.2 18.2 12 8 18.8z",
            Icon::Pause => "M9.2 5.5v13M14.8 5.5v13",
            Icon::Stop => "M6.8 6.8h10.4v10.4H6.8z",
            Icon::Check => "M5 12.6 9.6 17.2 19 7.4",
            Icon::Alert => "M12 4.2 21.2 19.8H2.8zM12 10v4M12 17h.01",
            Icon::X => "M6.4 6.4l11.2 11.2M17.6 6.4 6.4 17.6",
            Icon::Plus => "M12 5v14M5 12h14",
            Icon::Trash => "M4 7h16M9.6 7V4.6h4.8V7M6.6 7l1 13h8.8l1-13M10.4 11v6M13.6 11v6",
            Icon::Search => "M10.6 3.8a6.8 6.8 0 1 1 0 13.6 6.8 6.8 0 0 1 0-13.6zM15.6 15.6 21 21",
            Icon::Copy => "M9 9h10.5v11.5H9zM15 5H4.5v10",
            Icon::Refresh => "M20 12a8 8 0 1 1-2.4-5.7M20.4 3.6v4.2h-4.2",
            Icon::Clock => "M12 4a8 8 0 1 0 0 16 8 8 0 0 0 0-16zM12 7.8v4.6l3.1 2",
            Icon::Chevron => "M7.5 10l4.5 4.5 4.5-4.5",
            Icon::Arc => "M12 3a9 9 0 1 1-9 9",
        }
    }

    pub fn path(self) -> SharedString {
        format!("icons/{}.svg", self.name()).into()
    }

    fn svg_source(self) -> String {
        let (width, cap) = match self {
            Icon::Arc => ("2.6", "round"),
            _ => ("1.6", "round"),
        };
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="{width}" stroke-linecap="{cap}" stroke-linejoin="round"><path d="{}"/></svg>"##,
            self.d()
        )
    }
}

pub fn icon(icon: Icon, size: Pixels, color: Hsla) -> Svg {
    svg()
        .path(icon.path())
        .size(size)
        .flex_none()
        .text_color(color)
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let Some(name) = path
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"))
        else {
            return Ok(None);
        };
        Ok(ALL
            .iter()
            .find(|i| i.name() == name)
            .map(|i| Cow::Owned(i.svg_source().into_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path.trim_end_matches('/') == "icons" {
            Ok(ALL.iter().map(|i| i.path()).collect())
        } else {
            Ok(Vec::new())
        }
    }
}
