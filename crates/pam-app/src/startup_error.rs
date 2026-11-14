//! The window shown instead of the workspace when the catalog can't be opened.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{v_flex, ActiveTheme, Root, TitleBar};
use gpui_kit::*;

use crate::i18n::{self, Key};
use crate::Quit;

pub struct StartupError {
    body: Key,
    detail: String,
    db_path: PathBuf,
}

impl StartupError {
    pub fn new(error: &pam_core::error::Error, db_path: PathBuf) -> Self {
        let body = match error {
            pam_core::error::Error::CatalogTooNew { .. } => Key::CatalogTooNewBody,
            _ => Key::CatalogOpenFailedBody,
        };
        Self {
            body,
            detail: error.to_string(),
            db_path,
        }
    }
}

impl Render for StartupError {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new())
            .child(
                v_flex()
                    .flex_1()
                    .p_6()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(i18n::t(Key::CatalogOpenFailed)),
                    )
                    .child(div().child(i18n::t(self.body)))
                    .child(
                        v_flex()
                            .gap_1()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(self.db_path.display().to_string())
                            .child(self.detail.clone()),
                    )
                    .child(div().flex_1())
                    .child(
                        div().flex().justify_end().child(
                            Button::new("startup-error-quit")
                                .primary()
                                .label(i18n::t(Key::Quit))
                                .on_click(|_, _, cx| cx.dispatch_action(&Quit)),
                        ),
                    ),
            )
    }
}

pub fn open_window(error: pam_core::error::Error, db_path: PathBuf, cx: &mut App) {
    eprintln!("cannot open catalog {}: {error}", db_path.display());
    let mut options = TitleBar::window_options();
    options.window_bounds = Some(WindowBounds::Windowed(Bounds::centered(
        None,
        size(px(520.), px(300.)),
        cx,
    )));
    options.app_id = Some("print-asset-manager".into());
    let view = StartupError::new(&error, db_path);
    cx.open_window(options, |window, cx| {
        crate::apply_system_theme(Some(window), cx);
        window.set_window_title(i18n::t(Key::AppTitle));
        let view = cx.new(|_| view);
        cx.new(|cx| Root::new(view, window, cx))
    })
    .expect("open window");
}

#[cfg(test)]
mod tests {
    // Named imports: a glob would bring in `gpui_kit::test`, which shadows
    // the `#[test]` that `#[gpui_kit::test]` expands to.
    use std::path::PathBuf;

    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{px, size, AppContext as _, TestAppContext};
    use pam_core::error::Error;

    use super::StartupError;
    use crate::i18n::Key;

    #[test]
    fn newer_catalog_gets_its_own_explanation() {
        let newer = Error::CatalogTooNew {
            found: 3,
            supported: 1,
        };
        let view = StartupError::new(&newer, PathBuf::from("/x/catalog.sqlite"));
        assert_eq!(view.body, Key::CatalogTooNewBody);
        assert!(view.detail.contains("schema 3"));

        let other = Error::Message("disk on fire".into());
        let view = StartupError::new(&other, PathBuf::from("/x/catalog.sqlite"));
        assert_eq!(view.body, Key::CatalogOpenFailedBody);
        assert_eq!(view.detail, "disk on fire");
    }

    #[gpui_kit::test]
    fn error_window_renders(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(520.), px(300.)), |window, cx| {
            let error = Error::CatalogTooNew {
                found: 3,
                supported: 1,
            };
            let view = cx.new(|_| StartupError::new(&error, PathBuf::from("/x/catalog.sqlite")));
            Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}
