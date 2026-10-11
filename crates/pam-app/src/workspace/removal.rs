//! Destructive actions: moving assets to the Trash and forgetting library
//! folders, both confirmed first.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogButtonProps, DialogFooter};
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use pam_core::catalog::{Asset, Library};
use pam_core::open::move_to_trash;

use crate::i18n::{self, Key};

use super::sidebar::{library_label, Location};
use super::Workspace;

impl Workspace {
    /// Ask, then move the files to the system Trash and drop them from the index.
    pub(super) fn confirm_trash(
        &mut self,
        ids: &[i64],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let assets: Vec<Asset> = self
            .assets
            .iter()
            .filter(|a| ids.contains(&a.id))
            .cloned()
            .collect();
        let (title, detail) = match assets.as_slice() {
            [] => return,
            [one] => (i18n::trash_confirm(one.name()), i18n::t(Key::TrashDetail)),
            many => (
                i18n::trash_confirm_many(many.len()),
                i18n::t(Key::TrashDetailMany),
            ),
        };
        let this = cx.entity().downgrade();
        confirm_destructive(
            window,
            cx,
            title,
            detail,
            i18n::t(Key::MoveToTrash),
            move |window, cx| {
                let assets = assets.clone();
                let _ = this.update(cx, |this, cx| this.trash_assets(assets, window, cx));
            },
        );
    }

    /// Trash each file in turn; the ones that moved leave the index, and any
    /// failures are reported together.
    fn trash_assets(&mut self, assets: Vec<Asset>, window: &mut Window, cx: &mut Context<Self>) {
        let index = assets
            .iter()
            .filter_map(|asset| self.assets.iter().position(|a| a.id == asset.id))
            .min()
            .unwrap_or(0);
        cx.spawn_in(window, async move |this, cx| {
            let results: Vec<(i64, String, Result<(), String>)> = cx
                .background_spawn(async move {
                    assets
                        .iter()
                        .map(|a| {
                            let result = move_to_trash(&a.abs_path()).map_err(|e| e.to_string());
                            (a.id, a.name().to_string(), result)
                        })
                        .collect()
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let mut failures = Vec::new();
                let mut removed = false;
                for (id, name, result) in results {
                    match result {
                        Ok(()) => {
                            removed = true;
                            if let Err(err) = this.catalog.remove_asset(id) {
                                eprintln!("remove asset failed: {err}");
                            }
                        }
                        Err(err) => failures.push(format!("{name}: {err}")),
                    }
                }
                if removed {
                    this.reload_folders();
                    this.reload_after_removal(index, window, cx);
                }
                if !failures.is_empty() {
                    let detail = failures.join("\n");
                    window.open_alert_dialog(cx, move |alert, _, _| {
                        alert
                            .title(i18n::t(Key::TrashFailed))
                            .description(detail.clone())
                            .button_props(DialogButtonProps::default().ok_text(i18n::t(Key::Ok)))
                    });
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask, then forget a library folder. Nothing on disk is touched.
    pub(super) fn confirm_remove_library(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lib) = self.libraries.iter().find(|l| l.id == id).cloned() else {
            return;
        };
        let this = cx.entity().downgrade();
        confirm_destructive(
            window,
            cx,
            i18n::remove_folder_confirm(library_label(&lib)),
            i18n::t(Key::RemoveFolderDetail),
            i18n::t(Key::RemoveFolder),
            move |window, cx| {
                let _ = this.update(cx, |this, cx| this.remove_library(&lib, window, cx));
            },
        );
    }

    fn remove_library(&mut self, lib: &Library, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = self.catalog.remove_library(lib.id) {
            eprintln!("remove library failed: {err}");
            return;
        }
        if let Some(watch) = &self.watch {
            watch.unwatch(lib.root_path.clone());
        }
        if matches!(self.location, Location::Library { id, .. } if id == lib.id) {
            self.location = Location::All;
        }
        self.libraries.retain(|l| l.id != lib.id);
        self.reload_folders();
        let index = self.selected_index().unwrap_or(0);
        self.reload_after_removal(index, window, cx);
    }

    /// Reload after assets left the index. If the selection went with them,
    /// select whatever now sits at `index` (so repeated deletes keep flowing),
    /// or clear the preview when nothing is left. `reload` alone would swap
    /// the id without refreshing the preview and tag input.
    fn reload_after_removal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.selected;
        self.reload(cx);
        let Some(previous) = previous else {
            return;
        };
        if self.assets.iter().any(|a| a.id == previous) {
            return;
        }
        let last = self.assets.len().saturating_sub(1);
        match self.assets.get(index.min(last)).map(|a| a.id) {
            Some(next) => self.select(next, window, cx),
            None => {
                self.selected = None;
                self.selection.clear();
                self.anchor = None;
                self.preview_mesh = None;
                if let Some(old) = self.preview_image.take() {
                    cx.drop_image(old, None);
                }
                cx.notify();
            }
        }
    }
}

/// Confirm a destructive action. Cancel is the default: Enter and Escape
/// both dismiss, and only clicking the danger button runs `on_confirm`.
pub(super) fn confirm_destructive(
    window: &mut Window,
    cx: &mut App,
    title: String,
    detail: &'static str,
    confirm_label: &'static str,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
) {
    let on_confirm = std::rc::Rc::new(on_confirm);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_confirm = on_confirm.clone();
        alert
            .title(title.clone())
            .description(detail)
            // The dialog maps Enter to OK; with this custom footer OK only closes.
            .on_ok(|_, _, _| true)
            .footer(
                DialogFooter::new()
                    .child(
                        // Outlined, not the accent: in the vermilion theme an
                        // accent fill reads like the red destructive button.
                        Button::new("confirm-cancel")
                            .outline()
                            .label(i18n::t(Key::Cancel))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("confirm-destructive")
                            .danger()
                            .label(confirm_label)
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                on_confirm(window, cx);
                            }),
                    ),
            )
    });
}

#[cfg(test)]
mod dialog_tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::component::{Root, WindowExt as _};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{div, prelude::*, px, size, AppContext, Context, TestAppContext, Window};

    struct Host;

    impl gpui_kit::Render for Host {
        fn render(
            &mut self,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            div()
                .size_full()
                .children(Root::render_dialog_layer(window, cx))
        }
    }

    #[gpui_kit::test]
    fn destructive_confirm_defaults_to_cancel(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(640.), px(480.)), |window, cx| {
            Root::new(cx.new(|_| Host), window, cx)
        });
        let confirmed = Rc::new(Cell::new(0u32));
        let open = |cx: &mut TestAppContext| {
            let confirmed = confirmed.clone();
            cx.update_window(handle.into(), |_, window, cx| {
                super::confirm_destructive(
                    window,
                    cx,
                    "Move?".into(),
                    "detail",
                    "Move to Trash",
                    move |_, _| confirmed.set(confirmed.get() + 1),
                );
                window.render_frame(cx);
                assert!(window.has_active_dialog(cx));
            })
            .unwrap();
        };
        let dialog_open = |cx: &mut TestAppContext| {
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                window.has_active_dialog(cx)
            })
            .unwrap()
        };

        for key in ["enter", "escape"] {
            open(cx);
            cx.simulate_keystrokes(handle.into(), key);
            cx.run_until_parked();
            assert!(!dialog_open(cx), "{key} should close the dialog");
            assert_eq!(confirmed.get(), 0, "{key} must not confirm");
        }

        open(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("confirm-destructive", cx)
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!dialog_open(cx));
        assert_eq!(confirmed.get(), 1);
    }
}
