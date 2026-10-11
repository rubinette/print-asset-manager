//! The asset pane: grid and list views, their cards and rows, and the
//! asset context menu.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::slider::SliderState;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Icon, IconName, Sizable as _};
use gpui_kit::{prelude::FluentBuilder as _, *};
use pam_core::catalog::{Asset, AssetSort, PrintStatus, ThumbState};
use pam_core::open::open_path;
use pam_core::paths::thumb_path;

use crate::i18n::{self, Key};
use crate::{SelectAll, SelectDown, SelectLeft, SelectRight, SelectUp};

use super::format::{format_date, group_digits};
use super::inspector::INSPECTOR_WIDTH;
use super::print_status::{status_badge, status_icon, status_key};
use super::selection::Step;
use super::sidebar::SIDEBAR_WIDTH;
use super::toolbar::sort_is_ascending;
use super::{Workspace, ASSET_VIEW_CONTEXT};

/// Horizontal padding of the grid on each side, and the gap between cards.
const GRID_PADDING: f32 = 12.0;
const CARD_GAP: f32 = 12.0;
/// Card inner padding; the thumbnail is `card_size - 2 * CARD_PADDING` square.
const CARD_PADDING: f32 = 6.0;
/// Card height beyond its thumbnail: padding, name, and meta line.
const CARD_TEXT_HEIGHT: f32 = 48.0;
pub(super) const CARD_MIN: f32 = 112.0;
pub(super) const CARD_MAX: f32 = 280.0;
pub(super) const CARD_DEFAULT: f32 = 156.0;
const LIST_ROW_HEIGHT: f32 = 44.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum ViewMode {
    #[default]
    Grid,
    List,
}

impl ViewMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::List => "list",
        }
    }

    pub(super) fn parse(raw: &str) -> Self {
        match raw.trim() {
            "list" => Self::List,
            _ => Self::Grid,
        }
    }
}

pub(super) fn card_slider_state(value: f32) -> SliderState {
    // `max` before `min`: each setter re-clamps against the other bound, and
    // the default max is 100, below CARD_MIN, so the reverse order panics.
    SliderState::new()
        .max(CARD_MAX)
        .min(CARD_MIN)
        .step(4.)
        .default_value(value)
}

/// Columns that fit cards of at least `target` width, and the card width that
/// makes those columns fill the row exactly.
pub(super) fn grid_layout(window_width: f32, target: f32) -> (usize, f32) {
    let usable = (window_width - SIDEBAR_WIDTH - INSPECTOR_WIDTH - GRID_PADDING * 2.0).max(0.0);
    let cols = (((usable + CARD_GAP) / (target + CARD_GAP)).floor() as usize).max(1);
    let width = (usable + CARD_GAP) / cols as f32 - CARD_GAP;
    (cols, width.max(CARD_MIN))
}

pub(super) fn asset_pane(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    v_flex()
        .id("asset-grid-pane")
        .key_context(ASSET_VIEW_CONTEXT)
        .track_focus(&this.asset_focus)
        .on_action(cx.listener(|this, _: &SelectLeft, window, cx| {
            this.move_selection(Step::Left, window, cx)
        }))
        .on_action(cx.listener(|this, _: &SelectRight, window, cx| {
            this.move_selection(Step::Right, window, cx)
        }))
        .on_action(
            cx.listener(|this, _: &SelectUp, window, cx| this.move_selection(Step::Up, window, cx)),
        )
        .on_action(cx.listener(|this, _: &SelectDown, window, cx| {
            this.move_selection(Step::Down, window, cx)
        }))
        .on_action(cx.listener(|this, _: &SelectAll, window, cx| this.select_all(window, cx)))
        .flex_1()
        .h_full()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
        .bg(theme.background)
        .child(if this.assets.is_empty() && this.libraries.is_empty() {
            empty_state(cx).into_any_element()
        } else if this.assets.is_empty() {
            v_flex()
                .flex_1()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Icon::new(Lucide::FolderSearch).large())
                .child(i18n::t(if this.is_filtered() {
                    Key::NoMatches
                } else {
                    Key::EmptyFolder
                }))
                .into_any_element()
        } else {
            match this.view_mode {
                ViewMode::Grid => grid(this, cx).into_any_element(),
                ViewMode::List => list(this, cx).into_any_element(),
            }
        })
}

fn grid(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let cols = this.grid_cols.max(1);
    let rows = this.assets.len().div_ceil(cols);
    let assets = this.assets.clone();
    let selection: Arc<HashSet<i64>> = Arc::new(this.selection.iter().copied().collect());
    let size = this.grid_card_width;
    let workspace = cx.entity().downgrade();
    uniform_list("asset-grid", rows, move |range, _window, cx| {
        range
            .map(|row| {
                h_flex()
                    .w_full()
                    .h(px(size - CARD_PADDING * 2.0 + CARD_TEXT_HEIGHT + CARD_GAP))
                    .items_start()
                    .gap(px(CARD_GAP))
                    .px(px(GRID_PADDING))
                    .pt(px(CARD_GAP))
                    .children((0..cols).filter_map(|col| {
                        assets.get(row * cols + col).map(|asset| {
                            card(
                                asset,
                                selection.contains(&asset.id),
                                size,
                                workspace.clone(),
                                cx,
                            )
                        })
                    }))
            })
            .collect()
    })
    .track_scroll(&this.asset_scroll)
    .id("asset-grid")
    .size_full()
    .h_full()
}

fn empty_state(cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .child(
            div()
                .p_4()
                .rounded_full()
                .bg(theme.muted)
                .text_color(theme.muted_foreground)
                .child(Icon::new(Lucide::Package).large()),
        )
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child(i18n::t(Key::EmptyTitle)),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_sm()
                .text_center()
                .text_color(theme.muted_foreground)
                .child(i18n::t(Key::EmptyBody)),
        )
        .child(
            Button::new("empty-add")
                .primary()
                .icon(IconName::Plus)
                .label(i18n::t(Key::AddFolder))
                .on_click(cx.listener(|this, _, window, cx| this.add_folder(window, cx))),
        )
}

/// Click selects (and focuses the asset view for arrow keys); ⌘/Ctrl-click
/// toggles, Shift-click selects a range; double-click opens. Right-click on
/// a selected asset keeps the selection, otherwise selects just that one.
fn asset_interactions(
    el: Stateful<Div>,
    asset: &Asset,
    workspace: WeakEntity<Workspace>,
) -> impl IntoElement {
    let id = asset.id;
    let path = asset.abs_path();
    let on_right = workspace.clone();
    let on_click = workspace.clone();
    el.on_mouse_down(MouseButton::Right, move |_, window, cx| {
        let _ = on_right.update(cx, |this, cx| {
            if !this.selection.contains(&id) {
                this.select(id, window, cx);
            }
        });
    })
    .on_click(move |event: &ClickEvent, window, cx| {
        let dbl = event.click_count() > 1;
        let modifiers = event.modifiers();
        let _ = on_click.update(cx, |this, cx| {
            window.focus(&this.asset_focus, cx);
            if dbl {
                this.selected = Some(id);
                this.open_selected(cx);
            } else if modifiers.shift {
                this.extend_select(id, window, cx);
            } else if modifiers.secondary() {
                this.toggle_select(id, window, cx);
            } else {
                this.select(id, window, cx);
            }
        });
    })
    .context_menu(move |menu, window, cx| {
        let (targets, all_favorite) = workspace
            .upgrade()
            .map(|w| {
                let this = w.read(cx);
                let targets = this.targets_for(id);
                let all_favorite = this.all_favorite(&targets);
                (targets, all_favorite)
            })
            .unwrap_or_else(|| (vec![id], false));
        asset_menu(
            menu,
            targets,
            all_favorite,
            path.clone(),
            workspace.clone(),
            window,
            cx,
        )
    })
}

/// Open / reveal / copy act on the clicked asset; favorite, thumbnail and
/// trash items act on every target (the selection, when the click was inside
/// it).
fn asset_menu(
    menu: PopupMenu,
    targets: Vec<i64>,
    all_favorite: bool,
    path: std::path::PathBuf,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let (open, reveal, copy) = (path.clone(), path.clone(), path);
    let many = targets.len() > 1;
    let trash_label = if many {
        i18n::move_n_to_trash(targets.len())
    } else {
        i18n::t(Key::MoveToTrashEllipsis).to_string()
    };
    let regenerate_targets = targets.clone();
    let status_targets = targets.clone();
    let status_workspace = workspace.clone();
    let favorite_targets = targets.clone();
    menu.item(
        PopupMenuItem::new(i18n::t(Key::Open))
            .icon(IconName::ExternalLink)
            .on_click(move |_, _, _| {
                let _ = open_path(&open);
            }),
    )
    .item(
        PopupMenuItem::new(i18n::t(Key::RevealInFileManager))
            .icon(IconName::FolderOpen)
            .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
    )
    .item(
        PopupMenuItem::new(i18n::t(Key::CopyPath))
            .icon(IconName::Copy)
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy.display().to_string()))
            }),
    )
    .separator()
    .submenu_with_icon(
        Some(Icon::new(Lucide::Printer)),
        i18n::t(Key::PrintStatus),
        window,
        cx,
        move |menu, _, _| {
            let set = |status: Option<PrintStatus>| {
                let (workspace, ids) = (status_workspace.clone(), status_targets.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = workspace.update(cx, |this, cx| this.set_status(&ids, status, cx));
                }
            };
            PrintStatus::ALL
                .into_iter()
                .fold(menu, |menu, status| {
                    menu.item(
                        PopupMenuItem::new(i18n::t(status_key(status)))
                            .icon(status_icon(status))
                            .on_click(set(Some(status))),
                    )
                })
                .separator()
                .item(PopupMenuItem::new(i18n::t(Key::ClearStatus)).on_click(set(None)))
        },
    )
    .item({
        let workspace = workspace.clone();
        PopupMenuItem::new(i18n::t(if all_favorite {
            Key::RemoveFromFavorites
        } else {
            Key::AddToFavorites
        }))
        .icon(Icon::new(if all_favorite {
            Lucide::StarOff
        } else {
            Lucide::Star
        }))
        .on_click(move |_, window, cx| {
            let _ = workspace.update(cx, |this, cx| {
                this.toggle_favorite(&favorite_targets, window, cx)
            });
        })
    })
    .item({
        let workspace = workspace.clone();
        PopupMenuItem::new(i18n::t(if many {
            Key::RegenerateThumbs
        } else {
            Key::RegenerateThumb
        }))
        .icon(IconName::RotateCw)
        .on_click(move |_, _, cx| {
            let _ = workspace.update(cx, |this, cx| {
                this.regenerate_thumbs(&regenerate_targets, cx)
            });
        })
    })
    .separator()
    .item(
        PopupMenuItem::new(trash_label)
            .icon(Icon::new(Lucide::Trash))
            .on_click(move |_, window, cx| {
                let _ = workspace.update(cx, |this, cx| this.confirm_trash(&targets, window, cx));
            }),
    )
}

/// The filled star on starred cards, list rows and the inspector button.
pub(super) fn favorite_icon(theme: &gpui_kit::component::Theme) -> Icon {
    Icon::new(Lucide::StarFill)
        .xsmall()
        .text_color(theme.warning)
}

fn card(
    asset: &Asset,
    selected: bool,
    size: f32,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let el = v_flex()
        .id(ElementId::Integer(asset.id as u64))
        .w(px(size))
        .p(px(CARD_PADDING))
        .gap_1p5()
        .rounded_lg()
        .cursor_pointer()
        .border_1()
        .border_color(if selected {
            theme.primary
        } else {
            transparent_black()
        })
        .when(selected, |el| el.bg(theme.secondary))
        .hover(|el| el.bg(theme.secondary))
        .child(
            div()
                .relative()
                .w_full()
                .h(px(size - CARD_PADDING * 2.0))
                .rounded_md()
                .bg(theme.muted)
                .overflow_hidden()
                .child(thumb_content(asset, &theme))
                .child(
                    div()
                        .absolute()
                        .top_1()
                        .left_1()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.background.opacity(0.85))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(asset.format.label()),
                )
                .when(asset.favorite || asset.status.is_some(), |el| {
                    el.child(
                        h_flex()
                            .absolute()
                            .top_1()
                            .right_1()
                            .p_0p5()
                            .gap_0p5()
                            .items_center()
                            .rounded_full()
                            .bg(theme.background.opacity(0.85))
                            .when(asset.favorite, |el| el.child(favorite_icon(&theme)))
                            .children(status_badge(asset, &theme)),
                    )
                }),
        )
        .child(
            v_flex()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(asset.name().to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(card_meta(asset)),
                ),
        );
    asset_interactions(el, asset, workspace)
}

/// "12,345 △ · 60×31×48 mm", or whatever of it is known yet.
fn card_meta(asset: &Asset) -> String {
    let parts: Vec<String> = [
        asset
            .triangle_count
            .map(|n| format!("{} △", group_digits(n))),
        asset.bbox.as_ref().map(|b| b.format_mm()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" · ")
    }
}

fn thumb_content(asset: &Asset, theme: &gpui_kit::component::Theme) -> AnyElement {
    let placeholder = |icon: Icon, color: Hsla| {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(color)
            .child(icon)
            .into_any_element()
    };
    match (asset.sha256_hex(), asset.thumb_state) {
        (Some(hex), ThumbState::Ready | ThumbState::Embedded) => {
            // No exists() check: this runs per item per frame, and `with_fallback`
            // covers a missing file. GPUI treats `img(String)` as an embedded
            // asset, so pass a PathBuf.
            img(thumb_path(&hex))
                .size_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(|| {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(Lucide::ImageOff))
                        .into_any_element()
                })
                .into_any_element()
        }
        (_, ThumbState::Failed) => placeholder(Icon::new(Lucide::ImageOff), theme.danger),
        _ => placeholder(Icon::new(Lucide::Box), theme.muted_foreground),
    }
}

fn list(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let assets = this.assets.clone();
    let selection: HashSet<i64> = this.selection.iter().copied().collect();
    let workspace = cx.entity().downgrade();
    let sort = this.sort;
    v_flex()
        .size_full()
        .child(
            h_flex()
                .h(px(32.))
                .flex_shrink_0()
                .px_3()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(div().w(px(32.)))
                .child(header_cell(
                    Key::Name,
                    (AssetSort::NameAsc, AssetSort::NameDesc),
                    sort,
                    None,
                    cx,
                ))
                .child(header_cell(
                    Key::Format,
                    (AssetSort::Format, AssetSort::Format),
                    sort,
                    Some(LIST_FORMAT_WIDTH),
                    cx,
                ))
                .child(header_cell(
                    Key::Size,
                    (AssetSort::SizeDesc, AssetSort::SizeAsc),
                    sort,
                    Some(LIST_SIZE_WIDTH),
                    cx,
                ))
                .child(header_cell(
                    Key::Triangles,
                    (AssetSort::TrianglesDesc, AssetSort::TrianglesAsc),
                    sort,
                    Some(LIST_TRIANGLES_WIDTH),
                    cx,
                ))
                .child(header_cell(
                    Key::Modified,
                    (AssetSort::ModifiedDesc, AssetSort::ModifiedAsc),
                    sort,
                    Some(LIST_MODIFIED_WIDTH),
                    cx,
                )),
        )
        .child(
            uniform_list("asset-list", assets.len(), move |range, _window, cx| {
                range
                    .filter_map(|i| assets.get(i))
                    .map(|asset| {
                        list_row(asset, selection.contains(&asset.id), workspace.clone(), cx)
                    })
                    .collect()
            })
            .track_scroll(&this.asset_scroll)
            .flex_1()
            .min_h_0(),
        )
}

const LIST_FORMAT_WIDTH: f32 = 56.0;
const LIST_SIZE_WIDTH: f32 = 148.0;
const LIST_TRIANGLES_WIDTH: f32 = 88.0;
const LIST_MODIFIED_WIDTH: f32 = 88.0;

/// Clicking toggles between the column's two sorts; the active one shows an arrow.
fn header_cell(
    label: Key,
    sorts: (AssetSort, AssetSort),
    current: AssetSort,
    width: Option<f32>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let active = current == sorts.0 || current == sorts.1;
    let next = if current == sorts.0 { sorts.1 } else { sorts.0 };
    h_flex()
        .id(ElementId::Name(format!("col-{label:?}").into()))
        .gap_1()
        .items_center()
        .cursor_pointer()
        .map(|el| match width {
            Some(w) => el.w(px(w)).flex_shrink_0(),
            None => el.flex_1().min_w_0(),
        })
        .on_click(cx.listener(move |this, _, _, cx| this.set_sort(next, cx)))
        .child(i18n::t(label))
        .when(active, |el| {
            el.child(
                Icon::new(if sort_is_ascending(current) {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall(),
            )
        })
}

fn list_row(
    asset: &Asset,
    selected: bool,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let folder = Path::new(&asset.rel_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let fixed = |w: f32| div().w(px(w)).flex_shrink_0().truncate();
    let el = h_flex()
        .id(ElementId::Integer(asset.id as u64))
        // Without a definite width the row sizes to its content, so the name
        // column can't shrink and long names push the other columns out of line.
        .w_full()
        .h(px(LIST_ROW_HEIGHT))
        .px_3()
        .gap_3()
        .items_center()
        .text_sm()
        .cursor_pointer()
        .border_b_1()
        .border_color(theme.border.opacity(0.5))
        .when(selected, |el| el.bg(theme.secondary))
        .hover(|el| el.bg(theme.secondary))
        .child(
            div()
                .size(px(32.))
                .flex_shrink_0()
                .rounded_sm()
                .bg(theme.muted)
                .overflow_hidden()
                .child(thumb_content(asset, &theme)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .min_w_0()
                        .children(status_badge(asset, &theme))
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(asset.name().to_string()),
                        )
                        .when(asset.favorite, |el| {
                            el.child(favorite_icon(&theme).flex_shrink_0())
                        }),
                )
                .when(!folder.is_empty(), |el| {
                    el.child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(folder),
                    )
                }),
        )
        .child(
            fixed(LIST_FORMAT_WIDTH)
                .text_color(theme.muted_foreground)
                .child(asset.format.label()),
        )
        .child(
            fixed(LIST_SIZE_WIDTH).child(
                asset
                    .bbox
                    .as_ref()
                    .map(|b| b.format_mm())
                    .unwrap_or_else(|| "—".into()),
            ),
        )
        .child(
            fixed(LIST_TRIANGLES_WIDTH).child(
                asset
                    .triangle_count
                    .map(group_digits)
                    .unwrap_or_else(|| "—".into()),
            ),
        )
        .child(
            fixed(LIST_MODIFIED_WIDTH)
                .text_color(theme.muted_foreground)
                .child(format_date(asset.mtime_ns)),
        );
    asset_interactions(el, asset, workspace)
}

#[cfg(test)]
mod tests {
    // gpui-kit's glob re-export shadows Rust's #[test]; use the prelude path.
    #[::core::prelude::v1::test]
    fn grid_keeps_at_least_one_column() {
        use super::{grid_layout, CARD_MIN};
        assert_eq!(grid_layout(0.0, 156.0), (1, CARD_MIN));
        assert_eq!(grid_layout(1320.0, 156.0).0, 3);
        assert!(grid_layout(2400.0, 156.0).0 > 3);
        // Bigger cards, fewer columns.
        assert!(grid_layout(1600.0, 280.0).0 < grid_layout(1600.0, 112.0).0);
    }

    #[::core::prelude::v1::test]
    fn grid_cards_stretch_to_fill_the_row() {
        use super::{grid_layout, CARD_GAP, GRID_PADDING, INSPECTOR_WIDTH, SIDEBAR_WIDTH};
        for (window, target) in [(1320.0, 156.0), (1900.0, 200.0), (2560.0, 112.0)] {
            let (cols, width) = grid_layout(window, target);
            assert!(width >= target, "{window}/{target}: {width} < {target}");
            assert!(
                width < target + target + CARD_GAP,
                "{window}/{target}: too wide"
            );
            let usable = window - SIDEBAR_WIDTH - INSPECTOR_WIDTH - GRID_PADDING * 2.0;
            let used = cols as f32 * width + (cols - 1) as f32 * CARD_GAP;
            assert!(
                (used - usable).abs() < 0.01,
                "{window}/{target}: {used} vs {usable}"
            );
        }
    }

    #[::core::prelude::v1::test]
    fn card_slider_builds_over_full_range() {
        for value in [super::CARD_MIN, super::CARD_DEFAULT, super::CARD_MAX] {
            let state = super::card_slider_state(value);
            assert_eq!(state.min_value(), super::CARD_MIN);
            assert_eq!(state.max_value(), super::CARD_MAX);
            assert_eq!(state.value().start(), value);
        }
    }

    #[::core::prelude::v1::test]
    fn view_mode_roundtrip() {
        use super::ViewMode;
        assert_eq!(ViewMode::parse(ViewMode::List.as_str()), ViewMode::List);
        assert_eq!(ViewMode::parse(ViewMode::Grid.as_str()), ViewMode::Grid);
        assert_eq!(ViewMode::parse("bogus"), ViewMode::Grid);
    }
}
