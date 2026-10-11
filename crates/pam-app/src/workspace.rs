mod asset_view;
mod filters;
mod format;
mod inspector;
mod library;
mod nearby;
mod preview;
mod print_status;
mod removal;
mod saved;
mod selection;
mod sidebar;
mod toolbar;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Root, TitleBar};
use gpui_kit::{prelude::FluentBuilder as _, Focusable as _, *};
use pam_core::catalog::{Asset, AssetQuery, AssetSort, Catalog, Library, ThumbState};
use pam_core::open::open_path;
use pam_core::watch::WatchHandle;
use pam_preview::Camera;

use crate::i18n::{self, Key, LanguagePref};
use crate::jobs;
use crate::{
    apply_language, apply_system_theme, AddFolder, FocusSearch, OpenSelected, TrashSelected,
    UseChinese, UseEnglish, UseSystemLanguage,
};

use asset_view::{
    asset_pane, card_slider_state, grid_layout, ViewMode, CARD_DEFAULT, CARD_MAX, CARD_MIN,
};
use filters::{filter_bar, parse_bed, Filters, PrintTimeBucket, SizeBucket, DEFAULT_BED};
use inspector::inspector;
use nearby::Nearby;
use preview::PreviewMesh;
use saved::{NameTarget, SavedEntry};
use sidebar::{sidebar, FolderNode, Location};
use toolbar::toolbar;

/// Key context of the asset grid/list; arrow-key bindings only apply inside it.
pub const ASSET_VIEW_CONTEXT: &str = "AssetView";

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiStatus {
    Idle,
    Scanning,
    Loading,
    Failed,
}

impl UiStatus {
    fn text(self) -> SharedString {
        match self {
            Self::Idle => "".into(),
            Self::Scanning => i18n::t(Key::Scanning).into(),
            Self::Loading => i18n::t(Key::Loading).into(),
            Self::Failed => i18n::t(Key::Failed).into(),
        }
    }
}

pub struct Workspace {
    catalog: Arc<Catalog>,
    libraries: Vec<Library>,
    tags: Vec<(String, i64)>,
    /// Filament types in 3MF slicer settings, with file counts.
    materials: Vec<(String, i64)>,
    assets: Vec<Asset>,
    /// Primary selection: previewed, shown in the inspector, moved by arrows.
    selected: Option<i64>,
    /// Every selected asset, in the order picked; includes `selected`.
    selection: Vec<i64>,
    /// Where Shift-click ranges start.
    anchor: Option<i64>,
    location: Location,
    filters: Filters,
    show_filters: bool,
    /// Build volume for the "fits bed" filter, from the `bed` pref.
    bed: [f32; 3],
    /// Per library; rebuilt after scans and removals, not on every reload.
    folder_trees: HashMap<i64, FolderNode>,
    /// Expanded tree nodes as (library id, folder path); "" is the library itself.
    expanded: HashSet<(i64, String)>,
    duplicate_count: u32,
    saved: Vec<SavedEntry>,
    /// Name input of the save/rename dialog, and what it is naming.
    saved_name_input: Entity<InputState>,
    naming: Option<NameTarget>,
    /// Assets sharing the primary selection's content hash, itself included.
    selected_copies: u32,
    search: Entity<InputState>,
    tag_input: Entity<InputState>,
    /// Inspector input that adds tags to every selected asset.
    batch_tag_input: Entity<InputState>,
    note_input: Entity<TextareaState>,
    /// Asset whose note `note_input` holds, and the note as last saved.
    note_asset: Option<(i64, String)>,
    note_generation: u64,
    search_text: String,
    search_generation: u64,
    sort: AssetSort,
    scanning: bool,
    status: UiStatus,
    camera: Camera,
    preview_image: Option<Arc<RenderImage>>,
    preview_mesh: Option<PreviewMesh>,
    preview_busy: bool,
    preview_dirty: bool,
    last_preview_pos: Option<Point<Pixels>>,
    /// The camera moved since the mouse went down, so the frame is low-poly.
    preview_dragged: bool,
    /// Images and documents next to the selected asset.
    nearby: Nearby,
    watch: Option<WatchHandle>,
    scan_done: u32,
    scan_total: u32,
    thumb_done: u32,
    thumb_total: u32,
    thumbs_running: bool,
    thumbs_wanted: bool,
    view_mode: ViewMode,
    card_size: f32,
    card_slider: Entity<SliderState>,
    /// Columns in the last rendered grid; arrow up/down moves by this much.
    grid_cols: usize,
    /// Card width in the last rendered grid, stretched to fill the row.
    grid_card_width: f32,
    asset_focus: FocusHandle,
    asset_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let catalog =
            Arc::new(Catalog::open(&pam_core::paths::catalog_db_path()).expect("open catalog"));
        // Redraw app-rendered thumbnails once after the renderer changed; the
        // startup scan below then kicks the thumbnail queue.
        if read_pref("thumb_version").as_deref().map(str::trim) != Some(jobs::THUMB_RENDER_VERSION)
            && catalog.requeue_rendered_thumbs().is_ok()
        {
            write_pref("thumb_version", jobs::THUMB_RENDER_VERSION);
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(i18n::t(Key::Search)));
        let tag_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(i18n::t(Key::TagsHint)));
        let batch_tag_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(i18n::t(Key::AddTagsHint)));
        let note_input = cx.new(|cx| print_status::new_note_input(window, cx));
        let saved_name_input = cx.new(|cx| InputState::new(window, cx));

        let card_size = read_pref("card_size")
            .and_then(|raw| raw.trim().parse::<f32>().ok())
            .unwrap_or(CARD_DEFAULT)
            .clamp(CARD_MIN, CARD_MAX);
        let card_slider = cx.new(|_| card_slider_state(card_size));

        let mut subs = Vec::new();
        subs.push(cx.subscribe(
            &card_slider,
            |this, _, event: &SliderEvent, cx| match event {
                SliderEvent::Change(value) => {
                    this.card_size = value.start();
                    cx.notify();
                }
                SliderEvent::Release(value) => {
                    write_pref("card_size", &value.start().to_string());
                }
            },
        ));
        subs.push(
            cx.subscribe_in(&search, window, |this, state, event, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.search_text = state.read(cx).value().to_string();
                    this.schedule_search_reload(cx);
                }
            }),
        );
        subs.push(
            cx.subscribe_in(&tag_input, window, |this, state, event, _window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let value = state.read(cx).value().to_string();
                    this.apply_tags(&value, cx);
                }
            }),
        );
        subs.push(cx.subscribe_in(
            &batch_tag_input,
            window,
            |this, state, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let value = state.read(cx).value().to_string();
                    this.add_tags_to_selection(&value, cx);
                    state.update(cx, |input, cx| input.set_value("", window, cx));
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &note_input,
            window,
            |this, _, event, _window, cx| match event {
                InputEvent::Change => this.schedule_note_save(cx),
                InputEvent::Blur => this.save_note(cx),
                _ => {}
            },
        ));
        subs.push(
            cx.subscribe_in(&saved_name_input, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.commit_name(window, cx);
                }
            }),
        );
        subs.push(cx.observe_window_appearance(window, |this, window, cx| {
            apply_system_theme(Some(window), cx);
            if this.selected.is_some() {
                this.render_preview(cx);
            }
            cx.notify();
        }));

        let mut this = Self {
            catalog,
            libraries: Vec::new(),
            tags: Vec::new(),
            materials: Vec::new(),
            assets: Vec::new(),
            selected: None,
            selection: Vec::new(),
            anchor: None,
            location: Location::All,
            filters: Filters::default(),
            show_filters: false,
            bed: read_pref("bed")
                .and_then(|raw| parse_bed(&raw))
                .unwrap_or(DEFAULT_BED),
            folder_trees: HashMap::new(),
            expanded: HashSet::new(),
            duplicate_count: 0,
            saved: Vec::new(),
            saved_name_input,
            naming: None,
            selected_copies: 0,
            search,
            tag_input,
            batch_tag_input,
            note_input,
            note_asset: None,
            note_generation: 0,
            search_text: String::new(),
            search_generation: 0,
            sort: read_pref("sort")
                .map(|raw| AssetSort::parse(&raw))
                .unwrap_or_default(),
            scanning: false,
            status: UiStatus::Idle,
            camera: Camera::default(),
            preview_image: None,
            preview_mesh: None,
            preview_busy: false,
            preview_dirty: false,
            last_preview_pos: None,
            preview_dragged: false,
            nearby: Nearby::default(),
            watch: WatchHandle::start().ok(),
            scan_done: 0,
            scan_total: 0,
            thumb_done: 0,
            thumb_total: 0,
            thumbs_running: false,
            thumbs_wanted: false,
            view_mode: read_pref("view")
                .map(|raw| ViewMode::parse(&raw))
                .unwrap_or_default(),
            card_size,
            card_slider,
            grid_cols: 1,
            grid_card_width: card_size,
            asset_focus: cx.focus_handle(),
            asset_scroll: UniformListScrollHandle::new(),
            _subscriptions: subs,
        };
        this.reload(cx);
        this.reload_folders();
        this.start_watch_loop(cx);
        this.watch_libraries();
        this.scan_all(cx);
        this
    }

    fn apply_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.set_window_title(i18n::t(Key::AppTitle));
        self.search.update(cx, |input, cx| {
            input.set_placeholder(i18n::t(Key::Search), window, cx);
        });
        self.tag_input.update(cx, |input, cx| {
            input.set_placeholder(i18n::t(Key::TagsHint), window, cx);
        });
        self.batch_tag_input.update(cx, |input, cx| {
            input.set_placeholder(i18n::t(Key::AddTagsHint), window, cx);
        });
        self.note_input.update(cx, |input, cx| {
            input.set_placeholder(i18n::t(Key::NoteHint), window, cx);
        });
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if let Ok(libs) = self.catalog.libraries() {
            self.libraries = libs;
        }
        if let Ok(tags) = self.catalog.all_tags() {
            // A tag deleted elsewhere (its last asset gone) stops filtering.
            self.filters
                .tags
                .retain(|t| tags.iter().any(|(name, _)| name == t));
            self.tags = tags;
        }
        if let Ok(materials) = self.catalog.all_materials() {
            self.materials = materials;
        }
        if let Ok(n) = self.catalog.duplicate_count() {
            self.duplicate_count = n;
        }
        match self.catalog.assets(&self.query()) {
            Ok(assets) => self.assets = assets,
            Err(err) => eprintln!("catalog query failed: {err}"),
        }
        if let Some(id) = self.selected {
            if !self.assets.iter().any(|a| a.id == id) {
                self.selected = self.assets.first().map(|a| a.id);
            }
        }
        let present: HashSet<i64> = self.assets.iter().map(|a| a.id).collect();
        self.selection.retain(|id| present.contains(id));
        if self.selection.is_empty() {
            self.selection.extend(self.selected);
        }
        self.refresh_copies();
        let cache_fresh = match (&self.preview_mesh, self.selected_asset()) {
            (Some(cached), Some(asset)) => cached.matches(asset),
            _ => false,
        };
        if !cache_fresh {
            self.preview_mesh = None;
        }
        self.reload_saved();
        cx.notify();
    }

    fn query(&self) -> AssetQuery {
        self.query_for(&self.location, &self.filters, &self.search_text)
    }

    /// The catalog query for a sidebar location, filters and search text.
    fn query_for(&self, location: &Location, f: &Filters, search: &str) -> AssetQuery {
        let (library_id, dir) = match location {
            Location::Library { id, dir } => (Some(*id), dir.clone()),
            Location::All | Location::Duplicates => (None, None),
        };
        AssetQuery {
            library_id,
            dir,
            tags: f.tags.clone(),
            tag_mode: f.tag_mode,
            formats: f.formats.clone(),
            fits_bed: f.fits_bed.then_some(self.bed),
            file_size: f.file_size.map(SizeBucket::range),
            modified_since_ns: f
                .modified
                .map(|m| now_ns() - m.days() * 86_400 * 1_000_000_000),
            untagged: f.untagged,
            print_status: f.status,
            materials: f.materials.clone(),
            print_time: f.print_time.map(PrintTimeBucket::range),
            thumb_state: f.thumb_failed.then_some(ThumbState::Failed),
            duplicates_only: *location == Location::Duplicates,
            search: if search.trim().is_empty() {
                None
            } else {
                Some(search.to_string())
            },
            sort: self.sort,
            ..Default::default()
        }
    }

    /// Anything besides the sidebar location narrowing the list, so an empty
    /// result means "no matches" rather than "empty folder".
    fn is_filtered(&self) -> bool {
        self.filters != Filters::default()
            || !self.search_text.trim().is_empty()
            || self.location == Location::Duplicates
    }

    fn refresh_copies(&mut self) {
        self.selected_copies = self
            .selected_asset()
            .and_then(|a| a.content_sha256.as_deref())
            .and_then(|sha| self.catalog.copies(sha).ok())
            .unwrap_or(0);
    }

    /// Reload after typing pauses, so each keystroke doesn't re-query the catalog.
    fn schedule_search_reload(&mut self, cx: &mut Context<Self>) {
        self.search_generation += 1;
        let generation = self.search_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                if this.search_generation == generation {
                    this.reload(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn selected_asset(&self) -> Option<&Asset> {
        let id = self.selected?;
        self.assets.iter().find(|a| a.id == id)
    }

    fn open_selected(&mut self, _cx: &mut Context<Self>) {
        if let Some(asset) = self.selected_asset() {
            let _ = open_path(&asset.abs_path());
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        (self.grid_cols, self.grid_card_width) =
            grid_layout(window.viewport_size().width.as_f32(), self.card_size);
        if self.note_asset.as_ref().map(|(id, _)| *id) != self.selected {
            cx.defer_in(window, |this, window, cx| {
                this.sync_inspector_inputs(window, cx)
            });
        }
        if self.nearby.asset_id != self.selected {
            cx.defer_in(window, |this, _, cx| this.load_nearby(cx));
        }
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &AddFolder, window, cx| this.add_folder(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSelected, _, cx| this.open_selected(cx)))
            .on_action(cx.listener(|this, _: &TrashSelected, window, cx| {
                let ids = this.selection.clone();
                this.confirm_trash(&ids, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                window.focus(&this.search.read(cx).focus_handle(cx), cx);
            }))
            .on_action(cx.listener(|this, _: &UseSystemLanguage, window, cx| {
                apply_language(LanguagePref::System, cx);
                this.apply_locale(window, cx);
            }))
            .on_action(cx.listener(|this, _: &UseChinese, window, cx| {
                apply_language(LanguagePref::Chinese, cx);
                this.apply_locale(window, cx);
            }))
            .on_action(cx.listener(|this, _: &UseEnglish, window, cx| {
                apply_language(LanguagePref::English, cx);
                this.apply_locale(window, cx);
            }))
            .child(title_bar(cx))
            .child(toolbar(self, cx))
            .when(self.show_filters, |el| el.child(filter_bar(self, cx)))
            .child(
                // Stretching row, not `h_flex()`: that helper centers on Y and
                // collapses children without an explicit height to 0px, which
                // hid the asset grid even when the catalog had hundreds of rows.
                div()
                    .id("body")
                    .flex()
                    .flex_row()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar(self, cx))
                    .child(asset_pane(self, cx))
                    .child(inspector(self, cx)),
            )
            .on_drop(cx.listener(|this, files: &ExternalPaths, _, cx| {
                for path in files.paths() {
                    this.add_folder_path(path, cx);
                }
            }))
            // gpui-kit's Root doesn't paint dialogs itself; the root view must.
            .children(Root::render_dialog_layer(window, cx))
    }
}

fn title_bar(_cx: &mut Context<Workspace>) -> impl IntoElement {
    TitleBar::new().child(
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .pr_2()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(i18n::t(Key::AppTitle)),
            ),
    )
}

fn read_pref(name: &str) -> Option<String> {
    std::fs::read_to_string(pam_core::paths::data_dir().join(name)).ok()
}

fn write_pref(name: &str, value: &str) {
    let _ = std::fs::write(pam_core::paths::data_dir().join(name), value);
}

#[cfg(test)]
mod layout_tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        div, prelude::*, px, size, uniform_list, AppContext, Context, TestAppContext,
        TestSupportExt, Window,
    };

    struct ThreePaneGrid;

    impl gpui_kit::Render for ThreePaneGrid {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui_kit::IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(div().h(px(48.)).child("toolbar"))
                .child(
                    div()
                        .id("body")
                        .flex()
                        .flex_row()
                        .items_stretch()
                        .flex_1()
                        .min_h_0()
                        .child(div().w(px(80.)).h_full().child("side"))
                        .child(
                            div()
                                .id("asset-grid-pane")
                                .flex_1()
                                .h_full()
                                .min_w_0()
                                .min_h_0()
                                .overflow_hidden()
                                .test_support()
                                .child(
                                    uniform_list("rows", 12, |range, _, _| {
                                        range
                                            .map(|i| {
                                                div()
                                                    .id(gpui_kit::ElementId::Integer(i as u64))
                                                    .h(px(40.))
                                                    .test_support()
                                                    .child(format!("card {i}"))
                                            })
                                            .collect()
                                    })
                                    .size_full()
                                    .h_full(),
                                ),
                        )
                        .child(div().w(px(80.)).h_full().child("inspector")),
                )
        }
    }

    #[gpui_kit::test]
    fn asset_grid_pane_fills_body_height(cx: &mut TestAppContext) {
        let handle = cx.open_window(size(px(800.), px(400.)), |_, _| ThreePaneGrid);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let pane = window.find("asset-grid-pane");
            assert!(
                pane.bounds().size.height > px(100.),
                "grid pane collapsed to {:?}",
                pane.bounds().size
            );
            let card = window.find(0usize);
            assert!(
                card.bounds().size.height > px(0.),
                "first card was not laid out: {:?}",
                card.bounds().size
            );
        })
        .unwrap();
    }
}

#[cfg(test)]
mod theme_tests {
    use crate::apply_system_theme;
    use gpui_kit::component::{ActiveTheme, Theme, ThemeMode};
    use gpui_kit::{div, prelude::*, px, size, Context, TestAppContext, Window};

    struct ThemeHost;

    impl gpui_kit::Render for ThemeHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui_kit::IntoElement {
            div().size_full().bg(cx.theme().background)
        }
    }

    #[gpui_kit::test]
    fn apply_system_theme_matches_window_appearance(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(320.), px(240.)), |window, cx| {
            apply_system_theme(Some(window), cx);
            ThemeHost
        });
        cx.update_window(handle.into(), |_, window, cx| {
            Theme::change(ThemeMode::Dark, Some(window), cx);
            assert_eq!(Theme::global(cx).mode, ThemeMode::Dark);
            apply_system_theme(Some(window), cx);
            assert_eq!(Theme::global(cx).mode, ThemeMode::from(window.appearance()));
        })
        .unwrap();
    }
}
