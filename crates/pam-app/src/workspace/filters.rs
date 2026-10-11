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

    fn name(self) -> &'static str {
        match self {
            Self::Under1 => "under1mb",
            Self::From1To10 => "1to10mb",
            Self::From10To100 => "10to100mb",
            Self::Over100 => "over100mb",
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

/// Estimated print time ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrintTimeBucket {
    Under1h,
    From1To4h,
    From4To12h,
    Over12h,
}

impl PrintTimeBucket {
    const ALL: [Self; 4] = [
        Self::Under1h,
        Self::From1To4h,
        Self::From4To12h,
        Self::Over12h,
    ];

    /// Seconds: at least `.0`, below `.1`.
    pub(super) fn range(self) -> (u64, Option<u64>) {
        const HOUR: u64 = 3_600;
        match self {
            Self::Under1h => (0, Some(HOUR)),
            Self::From1To4h => (HOUR, Some(4 * HOUR)),
            Self::From4To12h => (4 * HOUR, Some(12 * HOUR)),
            Self::Over12h => (12 * HOUR, None),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Under1h => "under1h",
            Self::From1To4h => "1to4h",
            Self::From4To12h => "4to12h",
            Self::Over12h => "over12h",
        }
    }

    fn key(self) -> Key {
        match self {
            Self::Under1h => Key::PrintTimeUnder1h,
            Self::From1To4h => Key::PrintTime1To4h,
            Self::From4To12h => Key::PrintTime4To12h,
            Self::Over12h => Key::PrintTimeOver12h,
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

    fn name(self) -> &'static str {
        match self {
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
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
    /// Filament types; any of them matches.
    pub(super) materials: Vec<String>,
    pub(super) print_time: Option<PrintTimeBucket>,
}

impl Filters {
    /// As stored in a saved search. Unset filters are left out.
    pub(super) fn to_json(&self) -> serde_json::Value {
        use serde_json::{json, Map, Value};
        let mut out = Map::new();
        let mut put = |key: &str, value: Value| {
            out.insert(key.into(), value);
        };
        if !self.formats.is_empty() {
            put(
                "formats",
                json!(self.formats.iter().map(|f| f.as_str()).collect::<Vec<_>>()),
            );
        }
        if self.fits_bed {
            put("fits_bed", json!(true));
        }
        if let Some(bucket) = self.file_size {
            put("file_size", json!(bucket.name()));
        }
        if let Some(within) = self.modified {
            put("modified", json!(within.name()));
        }
        if !self.tags.is_empty() {
            put("tags", json!(self.tags));
            put(
                "tag_mode",
                json!(if self.tag_mode == TagMode::Any {
                    "any"
                } else {
                    "all"
                }),
            );
        }
        if self.untagged {
            put("untagged", json!(true));
        }
        if self.thumb_failed {
            put("thumb_failed", json!(true));
        }
        if let Some(status) = self.status {
            put(
                "status",
                json!(match status {
                    StatusFilter::Is(status) => status.as_str(),
                    StatusFilter::Unmarked => "unmarked",
                }),
            );
        }
        if !self.materials.is_empty() {
            put("materials", json!(self.materials));
        }
        if let Some(bucket) = self.print_time {
            put("print_time", json!(bucket.name()));
        }
        Value::Object(out)
    }

    /// Inverse of [`Self::to_json`]; anything missing or unknown is unset.
    pub(super) fn from_json(value: &serde_json::Value) -> Self {
        let text = |key: &str| value.get(key).and_then(|v| v.as_str());
        let flag = |key: &str| value.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
        let list = |key: &str| -> Vec<String> {
            value
                .get(key)
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| i.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            formats: list("formats")
                .iter()
                .filter_map(|f| AssetFormat::parse(f))
                .collect(),
            fits_bed: flag("fits_bed"),
            file_size: text("file_size")
                .and_then(|n| SizeBucket::ALL.into_iter().find(|b| b.name() == n)),
            modified: text("modified")
                .and_then(|n| ModifiedWithin::ALL.into_iter().find(|m| m.name() == n)),
            tags: list("tags"),
            tag_mode: if text("tag_mode") == Some("any") {
                TagMode::Any
            } else {
                TagMode::All
            },
            untagged: flag("untagged"),
            thumb_failed: flag("thumb_failed"),
            status: text("status").and_then(|s| match s {
                "unmarked" => Some(StatusFilter::Unmarked),
                other => PrintStatus::parse(other).map(StatusFilter::Is),
            }),
            materials: list("materials"),
            print_time: text("print_time")
                .and_then(|n| PrintTimeBucket::ALL.into_iter().find(|b| b.name() == n)),
        }
    }

    /// Labels of the filters that are set, for naming a saved search.
    pub(super) fn labels(&self, bed: [f32; 3]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        out.extend(self.formats.iter().map(|f| f.label().to_string()));
        if self.fits_bed {
            out.push(i18n::fits_bed(&bed_label(bed)));
        }
        out.extend(self.file_size.map(|b| i18n::t(b.key()).to_string()));
        out.extend(self.modified.map(|m| i18n::t(m.key()).to_string()));
        out.extend(self.tags.iter().cloned());
        if self.untagged {
            out.push(i18n::t(Key::Untagged).to_string());
        }
        if self.thumb_failed {
            out.push(i18n::t(Key::ThumbFailed).to_string());
        }
        out.extend(self.status.map(|s| match s {
            StatusFilter::Is(status) => i18n::t(status_key(status)).to_string(),
            StatusFilter::Unmarked => i18n::t(Key::Unmarked).to_string(),
        }));
        out.extend(self.materials.iter().cloned());
        out.extend(self.print_time.map(|b| i18n::t(b.key()).to_string()));
        out
    }

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
            !self.materials.is_empty(),
            self.print_time.is_some(),
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
    // Selected materials stay listed (so they can be unticked) even when no
    // file has them right now, e.g. from a saved search.
    let mut materials = this.materials.clone();
    for name in &f.materials {
        if !materials.iter().any(|(m, _)| m == name) {
            materials.push((name.clone(), 0));
        }
    }

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
            .when(active, |b| b.secondary())
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
    let materials_label = (!f.materials.is_empty()).then(|| f.materials.join(" / "));
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
        .bg(theme.muted)
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
                "filter-materials",
                titled(Key::Materials, materials_label),
                !f.materials.is_empty(),
            )
            .dropdown_menu({
                let (workspace, selected) = (workspace.clone(), f.materials.clone());
                move |menu, _, _| {
                    if materials.is_empty() {
                        return menu.label(i18n::t(Key::NoMaterialsYet));
                    }
                    materials.iter().fold(menu, |menu, (name, count)| {
                        let material = name.clone();
                        menu.item(filter_item(
                            format!("{name} ({count})"),
                            selected.contains(name),
                            &workspace,
                            move |f| match f.materials.iter().position(|m| *m == material) {
                                Some(i) => {
                                    f.materials.remove(i);
                                }
                                None => f.materials.push(material.clone()),
                            },
                        ))
                    })
                }
            }),
        )
        .child(
            dropdown(
                "filter-print-time",
                titled(
                    Key::PrintTime,
                    f.print_time.map(|b| i18n::t(b.key()).to_string()),
                ),
                f.print_time.is_some(),
            )
            .dropdown_menu({
                let (workspace, current) = (workspace.clone(), f.print_time);
                move |menu, _, _| {
                    let menu = menu.item(filter_item(
                        i18n::t(Key::AnyPrintTime),
                        current.is_none(),
                        &workspace,
                        |f| f.print_time = None,
                    ));
                    PrintTimeBucket::ALL.into_iter().fold(menu, |menu, bucket| {
                        menu.item(filter_item(
                            i18n::t(bucket.key()),
                            current == Some(bucket),
                            &workspace,
                            move |f| f.print_time = Some(bucket),
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
    fn filters_roundtrip_through_json() {
        use super::{Filters, ModifiedWithin, PrintTimeBucket, SizeBucket};
        use pam_core::catalog::{PrintStatus, StatusFilter, TagMode};
        use pam_core::mesh::AssetFormat;
        assert_eq!(
            Filters::from_json(&Filters::default().to_json()),
            Filters::default()
        );
        assert_eq!(Filters::default().to_json().to_string(), "{}");
        let all = Filters {
            formats: vec![AssetFormat::ThreeMf, AssetFormat::Obj],
            fits_bed: true,
            file_size: Some(SizeBucket::From10To100),
            modified: Some(ModifiedWithin::Month),
            tags: vec!["船".into(), "calibration".into()],
            tag_mode: TagMode::Any,
            untagged: true,
            thumb_failed: true,
            status: Some(StatusFilter::Is(PrintStatus::Failed)),
            materials: vec!["PETG".into()],
            print_time: Some(PrintTimeBucket::From4To12h),
        };
        assert_eq!(Filters::from_json(&all.to_json()), all);
        for (status, raw) in [
            (StatusFilter::Unmarked, "unmarked"),
            (StatusFilter::Is(PrintStatus::Todo), "todo"),
        ] {
            let f = Filters {
                status: Some(status),
                ..Default::default()
            };
            assert_eq!(f.to_json()["status"], raw);
            assert_eq!(Filters::from_json(&f.to_json()), f);
        }
        // Unknown values (say, from a newer version) are dropped, not errors.
        let odd =
            serde_json::json!({"formats": ["step", "stl"], "file_size": "huge", "fits_bed": 1});
        let parsed = Filters::from_json(&odd);
        assert_eq!(parsed.formats, [AssetFormat::Stl]);
        assert_eq!((parsed.file_size, parsed.fits_bed), (None, false));
    }

    #[::core::prelude::v1::test]
    fn print_time_buckets_tile_the_range() {
        use super::PrintTimeBucket;
        let ranges = PrintTimeBucket::ALL.map(PrintTimeBucket::range);
        assert_eq!(ranges[0].0, 0);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].1, Some(pair[1].0));
        }
        assert_eq!(ranges[3].1, None);
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
