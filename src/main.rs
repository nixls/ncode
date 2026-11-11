#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod claude;
mod git;
mod icons;
mod model;
mod state;
mod store;
mod ui;

use gpui_kit::component::{Theme, TitleBar};
use gpui_kit::*;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("ncode=info")).init();
    let store = match store::Store::open_default() {
        Ok(store) => store,
        Err(err) => {
            eprintln!("ncode: cannot open database in {}: {err:#}", store::data_dir().display());
            std::process::exit(1);
        }
    };

    gpui_kit::application().with_assets(icons::AppAssets).run(move |cx| {
        gpui_kit::init(cx);
        let app = cx.new(|cx| state::AppState::new(store, cx));
        cx.on_app_quit({
            let app = app.clone();
            move |cx| {
                app.update(cx, |a, _| a.shutdown());
                async {}
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.), px(480.))),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            window.set_window_title("ncode");
            cx.new(|cx| ui::Workspace::new(app.clone(), window, cx))
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
