//! The filter bar and the filter state it edits.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{h_flex, ActiveTheme, Disableable as _, Sizable as _};
use gpui_kit::{prelude::FluentBuilder as _, *};
use pam_core::catalog::{PrintStatus, StatusFilter, TagMode};
use pam_core::mesh::AssetFormat;

use crate::i18n::{self, Key};

use super::print_status::status_key;
use super::{write_pref, Workspace};

/// File size ranges in MB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SizeBucket {
    Under1,
    From1To10,
    From10To100,
    Over100,
}

impl SizeBucket {
    const ALL: [Self; 4] = [
        Self::Under1,
        Self::From1To10,
        Self::From10To100,
        Self::Over100,
    ];

    pub(super) fn range(self) -> (u64, Option<u64>) {
        const MB: u64 = 1024 * 1024;
        match self {
            Self::Under1 => (0, Some(MB)),
            Self::From1To10 => (MB, Some(10 * MB)),
            Self::From10To100 => (10 * MB, Some(100 * MB)),
            Self::Over100 => (100 * MB, None),
        }
    }

    fn key(self) -> Key {
        match self {
            Self::Under1 => Key::FileSizeUnder1Mb,
            Self::From1To10 => Key::FileSize1To10Mb,
            Self::From10To100 => Key::FileSize10To100Mb,
            Self::Over100 => Key::FileSizeOver100Mb,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ModifiedWithin {
    Week,
    Month,
    Year,
}

impl ModifiedWithin {
    const ALL: [Self; 3] = [Self::Week, Self::Month, Self::Year];

    pub(super) fn days(self) -> i64 {
        match self {
            Self::Week => 7,
            Self::Month => 30,
            Self::Year => 365,
        }
    }

    fn key(self) -> Key {
        match self {
            Self::Week => Key::Last7Days,
            Self::Month => Key::Last30Days,
            Self::Year => Key::LastYear,
        }
    }
}

/// Filter bar state. Not persisted.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Filters {
    pub(super) formats: Vec<AssetFormat>,
    pub(super) fits_bed: bool,
    pub(super) file_size: Option<SizeBucket>,
    pub(super) modified: Option<ModifiedWithin>,
    pub(super) tags: Vec<String>,
    pub(super) tag_mode: TagMode,
    pub(super) untagged: bool,
    pub(super) thumb_failed: bool,
    pub(super) status: Option<StatusFilter>,
}

impl Filters {
    /// How many filter groups are narrowing the view (for the toolbar badge).
    pub(super) fn active_count(&self) -> usize {
        [
            !self.formats.is_empty(),
            self.fits_bed,
            self.file_size.is_some(),
            self.modified.is_some(),
            !self.tags.is_empty(),
            self.untagged || self.thumb_failed,
            self.status.is_some(),
        ]
        .into_iter()
        .filter(|on| *on)
        .count()
    }
}

/// Build volumes offered in the filter menu (X, Y, Z mm).
const BED_PRESETS: [(&str, [f32; 3]); 5] = [
    ("Bambu Lab A1 mini", [180.0, 180.0, 180.0]),
    ("Bambu Lab A1 / P1 / X1", [256.0, 256.0, 256.0]),
    ("Prusa MK4", [250.0, 210.0, 220.0]),
    ("Creality Ender-3", [220.0, 220.0, 250.0]),
    ("Voron 2.4 350", [350.0, 350.0, 340.0]),
];
pub(super) const DEFAULT_BED: [f32; 3] = [256.0, 256.0, 256.0];

/// "256x256x256" (also `×`, `*`, spaces) → X, Y, Z. All three must be positive.
pub(super) fn parse_bed(raw: &str) -> Option<[f32; 3]> {
    let parts: Vec<f32> = raw
        .split(['x', 'X', '×', '*'])
        .map(|p| {
            p.trim()
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
        })
        .collect::<Option<_>>()?;
    <[f32; 3]>::try_from(parts).ok()
}

/// Inverse of [`parse_bed`], as stored in the `bed` pref.
fn bed_pref(bed: [f32; 3]) -> String {
    bed.map(|v| v.to_string()).join("x")
}

/// "256×256×256" for display.
fn bed_label(bed: [f32; 3]) -> String {
    bed.map(|v| v.to_string()).join("×")
}

impl Workspace {
    fn update_filters(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Filters)) {
        change(&mut self.filters);
        self.reload(cx);
    }

    fn set_bed(&mut self, bed: [f32; 3], cx: &mut Context<Self>) {
        self.bed = bed;
        write_pref("bed", &bed_pref(bed));
        self.filters.fits_bed = true;
        self.reload(cx);
    }
}

/// Dropdowns under the toolbar; each button's label shows its current value.
pub(super) fn filter_bar(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let workspace = cx.entity().downgrade();
    let f = this.filters.clone();
    let bed = this.bed;
    let tags: Vec<String> = this.tags.iter().map(|(name, _)| name.clone()).collect();

    // Menu item that applies `change` to the filters.
    fn filter_item(
        label: impl Into<SharedString>,
        checked: bool,
        workspace: &WeakEntity<Workspace>,
        change: impl Fn(&mut Filters) + 'static,
    ) -> PopupMenuItem {
        let workspace = workspace.clone();
        PopupMenuItem::new(label)
            .checked(checked)
            .on_click(move |_, _, cx| {
                let _ = workspace.update(cx, |this, cx| this.update_filters(cx, &change));
            })
    }
    fn titled(key: Key, value: Option<String>) -> String {
        match value {
            Some(value) => format!("{}: {value}", i18n::t(key)),
            None => i18n::t(key).to_string(),
        }
    }
    let dropdown = |id: &'static str, label: String, active: bool| {
        Button::new(id)
            .small()
            .when(active, |b| b.primary())
            .when(!active, |b| b.outline())
            .label(label)
            .dropdown_caret(true)
    };

    let formats_label = (!f.formats.is_empty()).then(|| {
        f.formats
            .iter()
            .map(|fmt| fmt.label())
            .collect::<Vec<_>>()
            .join(", ")
    });
    let tag_joiner = if f.tag_mode == TagMode::All {
        " + "
    } else {
        " / "
    };
    let tags_label = (!f.tags.is_empty()).then(|| f.tags.join(tag_joiner));
    let status_label = f.status.map(|s| match s {
        StatusFilter::Is(status) => i18n::t(status_key(status)).to_string(),
        StatusFilter::Unmarked => i18n::t(Key::Unmarked).to_string(),
    });
    let more_label = {
        let parts: Vec<&str> = [
            f.untagged.then(|| i18n::t(Key::Untagged)),
            f.thumb_failed.then(|| i18n::t(Key::ThumbFailed)),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    };

    h_flex()
        .w_full()
        .px_3()
        .py_1p5()
        .gap_1p5()
        .items_center()
        .flex_wrap()
        .border_b_1()
        .border_color(theme.border)
        .child(
            dropdown(
                "filter-format",
                titled(Key::Format, formats_label),
                !f.formats.is_empty(),
            )
            .dropdown_menu({
                let (workspace, formats) = (workspace.clone(), f.formats.clone());
                move |menu, _, _| {
                    [AssetFormat::ThreeMf, AssetFormat::Stl, AssetFormat::Obj]
                        .into_iter()
                        .fold(menu, |menu, fmt| {
                            menu.item(filter_item(
                                fmt.label(),
                                formats.contains(&fmt),
                                &workspace,
                                move |f| match f.formats.iter().position(|x| *x == fmt) {
                                    Some(i) => {
                                        f.formats.remove(i);
                                    }
                                    None => f.formats.push(fmt),
                                },
                            ))
                        })
                }
            }),
        )
        .child(
            dropdown(
                "filter-bed",
                if f.fits_bed {
                    i18n::fits_bed(&bed_label(bed))
                } else {
                    i18n::t(Key::Size).to_string()
                },
                f.fits_bed,
            )
            .dropdown_menu({
                let (workspace, fits) = (workspace.clone(), f.fits_bed);
                move |menu, _, _| {
                    let menu = menu
                        .item(filter_item(i18n::t(Key::AnySize), !fits, &workspace, |f| {
                            f.fits_bed = false
                        }))
                        .item(filter_item(
                            i18n::fits_bed(&bed_label(bed)),
                            fits,
                            &workspace,
                            |f| f.fits_bed = true,
                        ))
                        .separator()
                        .label(i18n::t(Key::PrinterBed));
                    BED_PRESETS.iter().fold(menu, |menu, (name, preset)| {
                        let (workspace, preset) = (workspace.clone(), *preset);
                        menu.item(
                            PopupMenuItem::new(format!("{name} ({})", bed_label(preset)))
                                .checked(preset == bed)
                                .on_click(move |_, _, cx| {
                                    let _ =
                                        workspace.update(cx, |this, cx| this.set_bed(preset, cx));
                                }),
                        )
                    })
                }
            }),
        )
        .child(
            dropdown(
                "filter-file-size",
                titled(
                    Key::FileSize,
                    f.file_size.map(|b| i18n::t(b.key()).to_string()),
                ),
                f.file_size.is_some(),
            )
            .dropdown_menu({
                let (workspace, current) = (workspace.clone(), f.file_size);
                move |menu, _, _| {
                    let menu = menu.item(filter_item(
                        i18n::t(Key::AnyFileSize),
                        current.is_none(),
                        &workspace,
                        |f| f.file_size = None,
                    ));
                    SizeBucket::ALL.into_iter().fold(menu, |menu, bucket| {
                        menu.item(filter_item(
                            i18n::t(bucket.key()),
                            current == Some(bucket),
                            &workspace,
                            move |f| f.file_size = Some(bucket),
                        ))
                    })
                }
            }),
        )
        .child(
            dropdown(
                "filter-modified",
                titled(
                    Key::Modified,
                    f.modified.map(|m| i18n::t(m.key()).to_string()),
                ),
                f.modified.is_some(),
            )
            .dropdown_menu({
                let (workspace, current) = (workspace.clone(), f.modified);
                move |menu, _, _| {
                    let menu = menu.item(filter_item(
                        i18n::t(Key::AnyTime),
                        current.is_none(),
                        &workspace,
                        |f| f.modified = None,
                    ));
                    ModifiedWithin::ALL.into_iter().fold(menu, |menu, within| {
                        menu.item(filter_item(
                            i18n::t(within.key()),
                            current == Some(within),
                            &workspace,
                            move |f| f.modified = Some(within),
                        ))
                    })
                }
            }),
        )
        .child(
            dropdown(
                "filter-tags",
                titled(Key::Tags, tags_label),
                !f.tags.is_empty(),
            )
            .dropdown_menu({
                let (workspace, selected, mode) = (workspace.clone(), f.tags.clone(), f.tag_mode);
                move |menu, _, _| {
                    let menu = menu
                        .item(filter_item(
                            i18n::t(Key::TagMatchAll),
                            mode == TagMode::All,
                            &workspace,
                            |f| f.tag_mode = TagMode::All,
                        ))
                        .item(filter_item(
                            i18n::t(Key::TagMatchAny),
                            mode == TagMode::Any,
                            &workspace,
                            |f| f.tag_mode = TagMode::Any,
                        ))
                        .separator();
                    if tags.is_empty() {
                        return menu.label(i18n::t(Key::NoTagsYet));
                    }
                    tags.iter().fold(menu, |menu, name| {
                        let tag = name.clone();
                        menu.item(filter_item(
                            name.clone(),
                            selected.contains(name),
                            &workspace,
                            move |f| match f.tags.iter().position(|t| *t == tag) {
                                Some(i) => {
                                    f.tags.remove(i);
                                }
                                None => f.tags.push(tag.clone()),
                            },
                        ))
                    })
                }
            }),
        )
        .child(
            dropdown(
                "filter-status",
                titled(Key::PrintStatus, status_label),
                f.status.is_some(),
            )
            .dropdown_menu({
                let (workspace, current) = (workspace.clone(), f.status);
                move |menu, _, _| {
                    let menu = menu.item(filter_item(
                        i18n::t(Key::AnyStatus),
                        current.is_none(),
                        &workspace,
                        |f| f.status = None,
                    ));
                    PrintStatus::ALL
                        .map(StatusFilter::Is)
                        .into_iter()
                        .chain([StatusFilter::Unmarked])
                        .fold(menu, |menu, filter| {
                            let label = match filter {
                                StatusFilter::Is(status) => i18n::t(status_key(status)),
                                StatusFilter::Unmarked => i18n::t(Key::Unmarked),
                            };
                            menu.item(filter_item(
                                label,
                                current == Some(filter),
                                &workspace,
                                move |f| f.status = Some(filter),
                            ))
                        })
                }
            }),
        )
        .child(
            dropdown(
                "filter-more",
                titled(Key::MoreFilters, more_label),
                f.untagged || f.thumb_failed,
            )
            .dropdown_menu({
                let (workspace, untagged, failed) = (workspace.clone(), f.untagged, f.thumb_failed);
                move |menu, _, _| {
                    menu.item(filter_item(
                        i18n::t(Key::Untagged),
                        untagged,
                        &workspace,
                        |f| f.untagged = !f.untagged,
                    ))
                    .item(filter_item(
                        i18n::t(Key::ThumbFailed),
                        failed,
                        &workspace,
                        |f| f.thumb_failed = !f.thumb_failed,
                    ))
                }
            }),
        )
        .child(div().flex_1())
        .child(
            Button::new("filter-clear")
                .ghost()
                .small()
                .label(i18n::t(Key::ClearFilters))
                .disabled(f == Filters::default())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_filters(cx, |f| *f = Filters::default())
                })),
        )
}

#[cfg(test)]
mod tests {
    // gpui-kit's glob re-export shadows Rust's #[test]; use the prelude path.
    #[::core::prelude::v1::test]
    fn bed_pref_parses_and_roundtrips() {
        use super::{bed_label, bed_pref, parse_bed, BED_PRESETS};
        assert_eq!(parse_bed("256x256x256"), Some([256.0; 3]));
        assert_eq!(parse_bed(" 250 × 210 × 220\n"), Some([250.0, 210.0, 220.0]));
        assert_eq!(parse_bed("180*180*180"), Some([180.0; 3]));
        assert_eq!(parse_bed("256x256"), None);
        assert_eq!(parse_bed("256x0x256"), None);
        assert_eq!(parse_bed("abc"), None);
        for (_, bed) in BED_PRESETS {
            assert_eq!(parse_bed(&bed_pref(bed)), Some(bed));
        }
        assert_eq!(bed_label([250.0, 210.0, 220.0]), "250×210×220");
    }

    #[::core::prelude::v1::test]
    fn filters_count_active_groups() {
        use super::{Filters, SizeBucket};
        assert_eq!(Filters::default().active_count(), 0);
        let f = Filters {
            fits_bed: true,
            file_size: Some(SizeBucket::Over100),
            untagged: true,
            thumb_failed: true,
            status: Some(pam_core::catalog::StatusFilter::Unmarked),
            ..Default::default()
        };
        assert_eq!(f.active_count(), 4);
        // Buckets tile the size range without gaps.
        let ranges = SizeBucket::ALL.map(SizeBucket::range);
        assert_eq!(ranges[0].0, 0);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].1, Some(pair[1].0));
        }
        assert_eq!(ranges[3].1, None);
    }
}
