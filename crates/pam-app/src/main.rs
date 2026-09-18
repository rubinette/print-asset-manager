mod i18n;
mod jobs;
mod workspace;

use gpui_kit::component::{ActiveTheme, Root, TitleBar};
use gpui_kit::*;

use crate::workspace::Workspace;

actions!(
    print_asset_manager,
    [Quit, AddFolder, FocusSearch, OpenSelected]
);

fn main() {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        if let Err(err) = init_theme(cx) {
            eprintln!("theme: {err}");
        }

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("cmd-o", AddFolder, None),
            KeyBinding::new("ctrl-o", AddFolder, None),
            KeyBinding::new("cmd-f", FocusSearch, None),
            KeyBinding::new("ctrl-f", FocusSearch, None),
            KeyBinding::new("enter", OpenSelected, None),
        ]);

        cx.set_menus(vec![Menu::new(i18n::APP_TITLE).items([
            MenuItem::action("加入資料夾…", AddFolder),
            MenuItem::separator(),
            MenuItem::action("結束", Quit),
        ])]);

        cx.spawn(async move |cx| {
            let mut options = TitleBar::window_options();
            options.window_bounds = Some(WindowBounds::Windowed(Bounds {
                origin: point(px(80.), px(60.)),
                size: size(px(1320.), px(860.)),
            }));
            if let Some(titlebar) = options.titlebar.as_mut() {
                titlebar.title = Some(i18n::APP_TITLE.into());
            }

            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| Workspace::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("open window");
        })
        .detach();
    });
}

fn init_theme(cx: &mut App) -> anyhow::Result<()> {
    use gpui_kit::component::{Theme, ThemeMode};
    Theme::change(ThemeMode::Dark, None, cx);
    Ok(())
}
