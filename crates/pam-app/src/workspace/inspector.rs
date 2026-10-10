//! The inspector: preview pane, details of the selected asset, and batch
//! tagging for a multi-selection.

use std::collections::{BTreeMap, HashSet};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Icon, IconName, Sizable as _};
use gpui_kit::{prelude::FluentBuilder as _, *};
use pam_core::catalog::Asset;
use pam_core::print_info::PrintInfo;
use pam_preview::Camera;

use crate::i18n::{self, Key};

use super::format::{format_date, group_digits};
use super::print_status::{status_buttons, status_date};
use super::Workspace;

pub(super) const INSPECTOR_WIDTH: f32 = 440.0;

/// "a, b，c、d" → trimmed, non-empty tags.
fn parse_tags(raw: &str) -> Vec<String> {
    raw.split([',', '，', '、'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

impl Workspace {
    pub(super) fn apply_tags(&mut self, raw: &str, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            return;
        };
        let _ = self.catalog.set_tags(id, &parse_tags(raw));
        self.reload(cx);
    }

    pub(super) fn add_tags_to_selection(&mut self, raw: &str, cx: &mut Context<Self>) {
        let tags = parse_tags(raw);
        if tags.is_empty() || self.selection.is_empty() {
            return;
        }
        if let Err(err) = self.catalog.add_tags(&self.selection, &tags) {
            eprintln!("add tags failed: {err}");
        }
        self.reload(cx);
    }

    fn remove_tag_from_selection(&mut self, tag: &str, cx: &mut Context<Self>) {
        if let Err(err) = self.catalog.remove_tag(&self.selection, tag) {
            eprintln!("remove tag failed: {err}");
        }
        self.reload(cx);
    }
}

pub(super) fn inspector(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    v_flex()
        .w(px(INSPECTOR_WIDTH))
        .flex_shrink_0()
        .h_full()
        .min_h_0()
        .border_l_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .child(preview_pane(this, cx))
        .child(
            // Scrolls once the details outgrow their share; the preview keeps
            // the rest (at least its minimum height).
            div()
                .id("inspector-details")
                .min_h_0()
                .max_h(relative(0.62))
                .overflow_y_scroll()
                .map(|el| {
                    if this.selection.len() > 1 {
                        el.child(batch_details(this, cx))
                    } else if let Some(asset) = this.selected_asset() {
                        el.child(asset_details(this, asset, cx))
                    } else {
                        el
                    }
                }),
        )
}

fn preview_pane(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let has_selection = this.selected.is_some();
    div()
        .id("preview")
        .relative()
        .flex_1()
        .min_h(px(240.))
        .m_3()
        .rounded_lg()
        .bg(theme.muted)
        .overflow_hidden()
        .when(has_selection, |el| el.cursor_grab())
        .on_mouse_move(cx.listener(|this, event, _, cx| this.on_preview_drag(event, cx)))
        .on_scroll_wheel(cx.listener(|this, event, _, cx| this.on_preview_scroll(event, cx)))
        .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
            if event.click_count() > 1 {
                this.camera = Camera::default();
                this.render_preview(cx);
            }
        }))
        .child(match (&this.preview_image, has_selection) {
            (Some(image), _) => img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(|| {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .child(i18n::t(Key::Failed))
                        .into_any_element()
                })
                .into_any_element(),
            (None, true) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(this.status.text())
                .into_any_element(),
            (None, false) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Icon::new(Lucide::Rotate3d).large())
                .child(i18n::t(Key::NoSelection))
                .into_any_element(),
        })
        .when(has_selection, |el| {
            el.child(
                div()
                    .absolute()
                    .bottom_2()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme.background.opacity(0.8))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(i18n::t(Key::DragHint)),
                    ),
            )
        })
}

fn asset_details(this: &Workspace, asset: &Asset, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let reveal = asset.abs_path();
    let location = asset
        .abs_path()
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    v_flex()
        .flex_shrink_0()
        .px_4()
        .pb_4()
        .gap_3()
        .child(
            v_flex()
                .min_w_0()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .truncate()
                        .child(asset.name().to_string()),
                )
                .child(
                    div()
                        .id("location")
                        .text_xs()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .tooltip({
                            let location = location.clone();
                            move |window, cx| Tooltip::new(location.clone()).build(window, cx)
                        })
                        .child(location),
                ),
        )
        .child(
            v_flex()
                .gap_1p5()
                .child(meta_row(i18n::t(Key::Format), asset.format.label(), &theme))
                .child(meta_row(
                    i18n::t(Key::Size),
                    asset
                        .bbox
                        .as_ref()
                        .map(|b| b.format_mm())
                        .unwrap_or_else(|| "—".into()),
                    &theme,
                ))
                .child(meta_row(
                    i18n::t(Key::Triangles),
                    asset
                        .triangle_count
                        .map(group_digits)
                        .unwrap_or_else(|| "—".into()),
                    &theme,
                ))
                .child(meta_row(
                    i18n::t(Key::Modified),
                    format_date(asset.mtime_ns),
                    &theme,
                ))
                .when(this.selected_copies > 1, |el| {
                    el.child(meta_row(
                        i18n::t(Key::Copies),
                        this.selected_copies.to_string(),
                        &theme,
                    ))
                }),
        )
        .when_some(
            asset.print.as_ref().filter(|p| !p.is_empty()),
            |el, print| el.child(print_info_rows(print, &theme)),
        )
        .child(Input::new(&this.tag_input).prefix(Icon::new(Lucide::Tag).small()))
        .child(
            v_flex()
                .gap_1p5()
                .child(
                    h_flex()
                        .justify_between()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(i18n::t(Key::PrintStatus))
                        .when_some(status_date(asset), |el, date| {
                            el.child(div().text_xs().child(date))
                        }),
                )
                .child(status_buttons("status", asset.status, vec![asset.id], cx)),
        )
        .child(Textarea::new(&this.note_input))
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("open")
                        .primary()
                        .flex_1()
                        .icon(IconName::ExternalLink)
                        .label(i18n::t(Key::Open))
                        .on_click(cx.listener(|this, _, _, cx| this.open_selected(cx))),
                )
                .child(
                    Button::new("reveal")
                        .outline()
                        .icon(IconName::FolderOpen)
                        .tooltip(i18n::t(Key::RevealInFileManager))
                        .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
                )
                .child({
                    let id = asset.id;
                    Button::new("trash")
                        .outline()
                        .icon(Icon::new(Lucide::Trash))
                        .tooltip(i18n::t(Key::MoveToTrash))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_trash(&[id], window, cx)
                        }))
                }),
        )
}

/// Inspector body for a multi-selection: add tags to all, remove a tag from
/// all, trash all.
fn batch_details(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let selection: HashSet<i64> = this.selection.iter().copied().collect();
    // Tags on any selected asset, with how many of them carry it.
    let mut tag_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for asset in this.assets.iter().filter(|a| selection.contains(&a.id)) {
        for tag in &asset.tags {
            *tag_counts.entry(tag.as_str()).or_default() += 1;
        }
    }
    let total = this.selection.len();
    // Highlight a status only when every selected asset has it.
    let mut statuses = this
        .assets
        .iter()
        .filter(|a| selection.contains(&a.id))
        .map(|a| a.status);
    let common_status = statuses
        .next()
        .flatten()
        .filter(|first| statuses.all(|s| s == Some(*first)));
    v_flex()
        .flex_shrink_0()
        .px_4()
        .pb_4()
        .gap_3()
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child(i18n::selected_count(total)),
        )
        .child(Input::new(&this.batch_tag_input).prefix(Icon::new(Lucide::Tag).small()))
        .when(!tag_counts.is_empty(), |el| {
            el.child(
                h_flex()
                    .flex_wrap()
                    .gap_1p5()
                    .children(tag_counts.into_iter().map(|(tag, count)| {
                        let name = tag.to_string();
                        h_flex()
                            .pl_2()
                            .gap_0p5()
                            .items_center()
                            .rounded_md()
                            .bg(theme.secondary)
                            .text_xs()
                            .child(if count == total {
                                name.clone()
                            } else {
                                format!("{name} {count}/{total}")
                            })
                            .child(
                                Button::new(SharedString::from(format!("untag-{name}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(Lucide::X))
                                    .tooltip(i18n::t(Key::RemoveTag))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_tag_from_selection(&name, cx)
                                    })),
                            )
                    })),
            )
        })
        .child(status_buttons(
            "batch-status",
            common_status,
            this.selection.clone(),
            cx,
        ))
        .child(
            Button::new("trash-selection")
                .outline()
                .icon(Icon::new(Lucide::Trash))
                .label(i18n::move_n_to_trash(total))
                .on_click(cx.listener(|this, _, window, cx| {
                    let ids = this.selection.clone();
                    this.confirm_trash(&ids, window, cx)
                })),
        )
}

/// The slicer section: whatever the 3MF says, nothing for what it doesn't.
fn print_info_rows(print: &PrintInfo, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let rows: Vec<(Key, String)> = [
        (Key::Title, print.title.clone()),
        (Key::Designer, print.designer.clone()),
        (Key::Printer, print.printer.clone()),
        (
            Key::Materials,
            (!print.materials.is_empty()).then(|| print.materials.join(", ")),
        ),
        (Key::PrintTime, print.print_time_s.map(i18n::duration)),
        (Key::Filament, print.filament_g.map(|g| format!("{g:.1} g"))),
        (
            Key::LayerHeight,
            print.layer_height_mm.map(|h| format!("{h:.2} mm")),
        ),
        (Key::Slicer, print.application.clone()),
    ]
    .into_iter()
    .filter_map(|(key, value)| Some((key, value?)))
    .collect();
    v_flex()
        .gap_1p5()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(i18n::t(Key::SlicerInfo)),
        )
        .children(
            rows.into_iter()
                .map(|(key, value)| meta_row(i18n::t(key), value, theme)),
        )
}

fn meta_row(
    label: &str,
    value: impl Into<String>,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    h_flex()
        .justify_between()
        .gap_3()
        .text_sm()
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(label.to_string()),
        )
        .child(div().truncate().child(value.into()))
}

#[cfg(test)]
mod tests {
    // gpui-kit's glob re-export shadows Rust's #[test]; use the prelude path.
    #[::core::prelude::v1::test]
    fn tags_split_on_ascii_and_cjk_commas() {
        assert_eq!(
            super::parse_tags(" a, b，c、 ,d "),
            ["a", "b", "c", "d"].map(String::from)
        );
        assert!(super::parse_tags(" , ").is_empty());
    }
}
