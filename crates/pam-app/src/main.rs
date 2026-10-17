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
        SelectAll,
        ToggleFavorite,
        TrashSelected
    ]
);

fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-o", AddFolder, None),
        KeyBinding::new("ctrl-o", AddFolder, None),
        KeyBinding::new("cmd-f", FocusSearch, None),
        KeyBinding::new("ctrl-f", FocusSearch, None),
        // Scoped like the rest: unscoped, it would outrank Enter in every text
        // field (GPUI ranks context-free bindings deepest) and open the file.
        KeyBinding::new("enter", OpenSelected, Some(workspace::ASSET_VIEW_CONTEXT)),
        KeyBinding::new("left", SelectLeft, Some(workspace::ASSET_VIEW_CONTEXT)),
        KeyBinding::new("right", SelectRight, Some(workspace::ASSET_VIEW_CONTEXT)),
        KeyBinding::new("up", SelectUp, Some(workspace::ASSET_VIEW_CONTEXT)),
        KeyBinding::new("down", SelectDown, Some(workspace::ASSET_VIEW_CONTEXT)),
        // Scoped to the asset view so text fields keep their own select-all.
        KeyBinding::new("cmd-a", SelectAll, Some(workspace::ASSET_VIEW_CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(workspace::ASSET_VIEW_CONTEXT)),
        // Finder's shortcut; plain Delete for Linux file managers. Both confirm first.
        KeyBinding::new(
            "cmd-backspace",
            TrashSelected,
            Some(workspace::ASSET_VIEW_CONTEXT),
        ),
        KeyBinding::new("delete", TrashSelected, Some(workspace::ASSET_VIEW_CONTEXT)),
        // Plain key, but only while the grid/list has focus, not in text fields.
        KeyBinding::new("f", ToggleFavorite, Some(workspace::ASSET_VIEW_CONTEXT)),
    ]
}

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
        cx.bind_keys(key_bindings());

        install_menus(cx);

        cx.spawn(async move |cx| {
            let mut options = TitleBar::window_options();
            options.window_bounds = Some(WindowBounds::Windowed(Bounds {
                origin: point(px(80.), px(60.)),
                size: size(px(1320.), px(860.)),
            }));
            // Matches the Linux desktop entry, so the window gets its icon.
            options.app_id = Some("print-asset-manager".into());
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

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use gpui_kit::component::input::{Input, InputEvent, InputState};
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        div, prelude::*, px, size, AppContext, Context, Entity, FocusHandle, Subscription,
        TestAppContext, Window,
    };

    use super::{key_bindings, OpenSelected};
    use crate::workspace::ASSET_VIEW_CONTEXT;

    /// An asset view and a text field side by side, like the workspace.
    struct Host {
        assets: FocusHandle,
        input: Entity<InputState>,
        opened: Rc<Cell<u32>>,
        entered: Rc<Cell<u32>>,
        _sub: Subscription,
    }

    impl gpui_kit::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui_kit::IntoElement {
            let opened = self.opened.clone();
            div()
                .size_full()
                .on_action(move |_: &OpenSelected, _, _| opened.set(opened.get() + 1))
                .child(
                    div()
                        .key_context(ASSET_VIEW_CONTEXT)
                        .track_focus(&self.assets)
                        .child("assets"),
                )
                .child(Input::new(&self.input))
        }
    }

    #[gpui_kit::test]
    fn enter_opens_only_from_the_asset_view(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(key_bindings());
        });
        let host = Rc::new(RefCell::new(None));
        let handle = cx.open_window(size(px(640.), px(480.)), {
            let host = host.clone();
            move |window, cx| {
                let view = cx.new(|cx| {
                    let input = cx.new(|cx| InputState::new(window, cx));
                    let entered = Rc::new(Cell::new(0));
                    let sub = cx.subscribe(&input, {
                        let entered = entered.clone();
                        move |_, _, event: &InputEvent, _| {
                            if matches!(event, InputEvent::PressEnter { .. }) {
                                entered.set(entered.get() + 1);
                            }
                        }
                    });
                    Host {
                        assets: cx.focus_handle(),
                        input,
                        opened: Rc::new(Cell::new(0)),
                        entered,
                        _sub: sub,
                    }
                });
                *host.borrow_mut() = Some(view.clone());
                Root::new(view, window, cx)
            }
        });
        let host = host.borrow().clone().unwrap();
        let counts = |cx: &mut TestAppContext| {
            host.read_with(cx, |host, _| (host.opened.get(), host.entered.get()))
        };

        cx.update_window(handle.into(), |_, window, cx| {
            let input = host.read(cx).input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        cx.simulate_keystrokes(handle.into(), "enter");
        cx.run_until_parked();
        assert_eq!(counts(cx), (0, 1), "Enter in a text field submits it");

        cx.update_window(handle.into(), |_, window, cx| {
            window.focus(&host.read(cx).assets.clone(), cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.simulate_keystrokes(handle.into(), "enter");
        cx.run_until_parked();
        assert_eq!(counts(cx), (1, 1), "Enter in the asset view opens");
    }
}
