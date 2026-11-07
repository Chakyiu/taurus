mod app;
mod capture;
mod data;
#[cfg(test)]
mod e2e;
mod engine;
mod icons;
mod live;
mod media;
mod player;
mod shell;
mod store;
mod subs;
mod theme;
mod ui;
mod views;
mod work;

use gpui::{
    App, AppContext as _, Application, Bounds, Focusable as _, KeyBinding, TitlebarOptions,
    WindowBounds, WindowOptions, actions, point, px, size,
};

actions!(taurus, [Quit]);

/// Debug builds honour `TAURUS_SIZE=WxH` for screenshots at other breakpoints.
fn initial_size() -> (f32, f32) {
    let parsed = cfg!(debug_assertions)
        .then(|| std::env::var("TAURUS_SIZE").ok())
        .flatten()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        });
    parsed.unwrap_or((1440., 900.))
}

fn main() {
    Application::new()
        .with_assets(icons::Assets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);
            theme::apply_component_theme(cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let (w, h) = initial_size();
            let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Taurus".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(14.))),
                }),
                window_min_size: Some(size(px(360.), px(560.))),
                app_id: Some("dev.taurus.workbench".into()),
                ..Default::default()
            };

            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| app::Workbench::new(window, cx));
                window.focus(&view.focus_handle(cx));
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            })
            .expect("open the Taurus window");
            cx.activate(true);
        });
}
