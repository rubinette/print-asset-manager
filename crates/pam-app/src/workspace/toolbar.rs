//! Search, sort, filter toggle, view switch, thumbnail size and activity.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::slider::Slider;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    h_flex, ActiveTheme, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use pam_core::catalog::AssetSort;

use crate::i18n::{self, Key};
use crate::margin;

use super::asset_view::ViewMode;
use super::saved::NameTarget;
use super::{write_pref, Workspace};

impl Workspace {
    pub(super) fn set_sort(&mut self, sort: AssetSort, cx: &mut Context<Self>) {
        if self.sort == sort {
            return;
        }
        self.sort = sort;
        write_pref("sort", sort.as_str());
        self.reload(cx);
    }

    fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_mode = mode;
        write_pref("view", mode.as_str());
        self.scroll_to_selected();
        cx.notify();
    }
}

pub(super) fn toolbar(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let current_sort = this.sort;
    let workspace = cx.entity().downgrade();
    let grid = this.view_mode == ViewMode::Grid;
    h_flex()
        .w_full()
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.muted)
        .child(
            div().w(px(320.)).child(
                Input::new(&this.search)
                    .small()
                    .cleanable(true)
                    .prefix(Icon::new(IconName::Search).small()),
            ),
        )
        .child(
            Button::new("sort")
                .ghost()
                .small()
                .icon(if sort_is_ascending(current_sort) {
                    IconName::SortAscending
                } else {
                    IconName::SortDescending
                })
                .label(i18n::t(i18n::sort_key(current_sort)))
                .tooltip(i18n::t(Key::Sort))
                .dropdown_caret(true)
                .dropdown_menu(move |menu, _, _| {
                    AssetSort::ALL.iter().copied().fold(menu, |menu, sort| {
                        let workspace = workspace.clone();
                        menu.item(
                            PopupMenuItem::new(i18n::t(i18n::sort_key(sort)))
                                .checked(sort == current_sort)
                                .on_click(move |_, _, cx| {
                                    let _ = workspace.update(cx, |this, cx| {
                                        this.set_sort(sort, cx);
                                    });
                                }),
                        )
                    })
                }),
        )
        .child({
            let active = this.filters.active_count();
            Button::new("filter")
                .ghost()
                .small()
                .icon(Lucide::ListFilter)
                .label(if active > 0 {
                    format!("{} · {active}", i18n::t(Key::Filter))
                } else {
                    i18n::t(Key::Filter).to_string()
                })
                .selected(this.show_filters || active > 0)
                .toggled(this.show_filters)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_filters = !this.show_filters;
                    cx.notify();
                }))
        })
        .child(
            Button::new("save-search")
                .ghost()
                .small()
                .icon(Lucide::BookmarkPlus)
                .tooltip(i18n::t(Key::SaveSearchEllipsis))
                .disabled(!this.can_save_search())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_name_dialog(NameTarget::New, window, cx)
                })),
        )
        .child(div().flex_1())
        .child(activity(this, &theme))
        .child(
            h_flex()
                .gap_0p5()
                .child(
                    Button::new("view-grid")
                        .ghost()
                        .small()
                        .icon(Lucide::LayoutGrid)
                        .selected(grid)
                        .toggled(grid)
                        .tooltip(i18n::t(Key::ViewGrid))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_view_mode(ViewMode::Grid, cx)),
                        ),
                )
                .child(
                    Button::new("view-list")
                        .ghost()
                        .small()
                        .icon(Lucide::LayoutList)
                        .selected(!grid)
                        .toggled(!grid)
                        .tooltip(i18n::t(Key::ViewList))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_view_mode(ViewMode::List, cx)),
                        ),
                ),
        )
        .when(grid, |el| {
            el.child(
                h_flex()
                    .id("thumb-size")
                    .gap_1p5()
                    .items_center()
                    .tooltip(|window, cx| Tooltip::new(i18n::t(Key::ThumbSize)).build(window, cx))
                    .child(
                        Icon::new(Lucide::Image)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    )
                    .child(div().w(px(96.)).child(Slider::new(&this.card_slider))),
            )
        })
        .child(
            // Outlined: the inspector's Open is this screen's one primary action.
            Button::new("add-folder")
                .outline()
                .small()
                .label(i18n::t(Key::AddFolder))
                .icon(IconName::Plus)
                .on_click(cx.listener(|this, _, window, cx| this.add_folder(window, cx))),
        )
}

/// Scan/thumbnail progress while busy, otherwise the asset count.
fn activity(this: &Workspace, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let (label, progress) = if this.scanning {
        if this.scan_total > 0 {
            (
                i18n::scan_progress(this.scan_done, this.scan_total),
                Some(percent(this.scan_done, this.scan_total)),
            )
        } else {
            (i18n::t(Key::Scanning).to_string(), None)
        }
    } else if this.thumbs_running && this.thumb_total > 0 {
        (
            i18n::thumb_progress(this.thumb_done, this.thumb_total),
            Some(percent(this.thumb_done, this.thumb_total)),
        )
    } else {
        (i18n::asset_count(this.assets.len()), None)
    };
    let busy = this.scanning || (this.thumbs_running && this.thumb_total > 0);
    h_flex()
        .gap_2()
        .items_center()
        .text_size(margin::UI_XS)
        .text_color(theme.muted_foreground)
        .child(label)
        .when(busy, |el| {
            el.child(
                div().w(px(96.)).child(
                    Progress::new("activity")
                        .loading(progress.is_none())
                        .value(progress.unwrap_or_default()),
                ),
            )
        })
}

fn percent(done: u32, total: u32) -> f32 {
    if total == 0 {
        0.
    } else {
        done as f32 / total as f32 * 100.
    }
}

pub(super) fn sort_is_ascending(sort: AssetSort) -> bool {
    matches!(
        sort,
        AssetSort::NameAsc
            | AssetSort::ModifiedAsc
            | AssetSort::SizeAsc
            | AssetSort::TrianglesAsc
            | AssetSort::Format
    )
}
