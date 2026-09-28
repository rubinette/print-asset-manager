mod i18n;
mod jobs;
mod workspace;

use gpui_kit::component::{Root, TitleBar};
use gpui_kit::*;

use crate::i18n::{Key, LanguagePref};
use crate::workspace::Workspace;

actions!(
    print_asset_manager,
    [
        Quit,
        AddFolder,
        FocusSearch,
        OpenSelected,
        UseSystemLanguage,
        UseChinese,
        UseEnglish,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        TrashSelected
    ]
);

fn main() {
    // `AllAssets`, not `Assets`: the default bundle only has the component
    // icons, and the UI also uses Lucide ones (`gpui_kit::assets::IconName`).
    let app = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        i18n::init();
        apply_system_theme(None, cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &UseSystemLanguage, cx| apply_language(LanguagePref::System, cx));
        cx.on_action(|_: &UseChinese, cx| apply_language(LanguagePref::Chinese, cx));
        cx.on_action(|_: &UseEnglish, cx| apply_language(LanguagePref::English, cx));
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("cmd-o", AddFolder, None),
            KeyBinding::new("ctrl-o", AddFolder, None),
            KeyBinding::new("cmd-f", FocusSearch, None),
            KeyBinding::new("ctrl-f", FocusSearch, None),
            KeyBinding::new("enter", OpenSelected, None),
            KeyBinding::new("left", SelectLeft, Some(workspace::ASSET_VIEW_CONTEXT)),
            KeyBinding::new("right", SelectRight, Some(workspace::ASSET_VIEW_CONTEXT)),
            KeyBinding::new("up", SelectUp, Some(workspace::ASSET_VIEW_CONTEXT)),
            KeyBinding::new("down", SelectDown, Some(workspace::ASSET_VIEW_CONTEXT)),
            // Finder's shortcut; plain Delete for Linux file managers. Both confirm first.
            KeyBinding::new(
                "cmd-backspace",
                TrashSelected,
                Some(workspace::ASSET_VIEW_CONTEXT),
            ),
            KeyBinding::new("delete", TrashSelected, Some(workspace::ASSET_VIEW_CONTEXT)),
        ]);

        install_menus(cx);

        cx.spawn(async move |cx| {
            let mut options = TitleBar::window_options();
            options.window_bounds = Some(WindowBounds::Windowed(Bounds {
                origin: point(px(80.), px(60.)),
                size: size(px(1320.), px(860.)),
            }));
            if let Some(titlebar) = options.titlebar.as_mut() {
                titlebar.title = Some(i18n::t(Key::AppTitle).into());
            }

            cx.open_window(options, |window, cx| {
                apply_system_theme(Some(window), cx);
                window.set_window_title(i18n::t(Key::AppTitle));
                let view = cx.new(|cx| Workspace::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("open window");
        })
        .detach();
    });
}

pub(crate) fn install_menus(cx: &mut App) {
    let pref = i18n::pref();
    cx.set_menus(vec![
        Menu::new(i18n::t(Key::AppTitle)).items([
            MenuItem::action(i18n::t(Key::AddFolderEllipsis), AddFolder),
            MenuItem::separator(),
            MenuItem::action(i18n::t(Key::Quit), Quit),
        ]),
        Menu::new(i18n::t(Key::Language)).items([
            MenuItem::action(i18n::t(Key::LanguageSystem), UseSystemLanguage)
                .checked(pref == LanguagePref::System),
            MenuItem::action(i18n::t(Key::LanguageChinese), UseChinese)
                .checked(pref == LanguagePref::Chinese),
            MenuItem::action(i18n::t(Key::LanguageEnglish), UseEnglish)
                .checked(pref == LanguagePref::English),
        ]),
    ]);
}

pub(crate) fn apply_language(pref: LanguagePref, cx: &mut App) {
    i18n::set_pref(pref, true);
    install_menus(cx);
    cx.refresh_windows();
}

pub(crate) fn apply_system_theme(window: Option<&mut Window>, cx: &mut App) {
    use gpui_kit::component::Theme;
    Theme::sync_system_appearance(window, cx);
}
