use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogButtonProps, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme, Icon, IconName, Root, Selectable as _, Sizable as _, TitleBar,
    WindowExt as _,
};
use gpui_kit::{prelude::FluentBuilder as _, Focusable as _, *};
use pam_core::catalog::{Asset, AssetQuery, AssetSort, Catalog, Library, ThumbState};
use pam_core::load::load_mesh;
use pam_core::mesh::Mesh;
use pam_core::open::{move_to_trash, open_path};
use pam_core::paths::thumb_path;
use pam_core::watch::{affected_library, WatchHandle};
use pam_preview::render_mesh;
use pam_preview::Camera;

use crate::i18n::{self, Key, LanguagePref};
use crate::jobs;
use crate::{
    apply_language, apply_system_theme, AddFolder, FocusSearch, OpenSelected, SelectDown,
    SelectLeft, SelectRight, SelectUp, TrashSelected, UseChinese, UseEnglish, UseSystemLanguage,
};

/// Key context of the asset grid/list; arrow-key bindings only apply inside it.
pub const ASSET_VIEW_CONTEXT: &str = "AssetView";

const SIDEBAR_WIDTH: f32 = 208.0;
const INSPECTOR_WIDTH: f32 = 440.0;
/// Horizontal padding of the grid on each side, and the gap between cards.
const GRID_PADDING: f32 = 12.0;
const CARD_GAP: f32 = 12.0;
/// Card inner padding; the thumbnail is `card_size - 2 * CARD_PADDING` square.
const CARD_PADDING: f32 = 6.0;
/// Card height beyond its thumbnail: padding, name, and meta line.
const CARD_TEXT_HEIGHT: f32 = 48.0;
const CARD_MIN: f32 = 112.0;
const CARD_MAX: f32 = 280.0;
const CARD_DEFAULT: f32 = 156.0;
const LIST_ROW_HEIGHT: f32 = 44.0;
const PREVIEW_SIZE: u32 = 640;
/// Minimum gap between grid refreshes while thumbnails are being generated.
const THUMB_RELOAD_INTERVAL: Duration = Duration::from_millis(500);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// Triangle budget while the user is dragging the preview.
const DRAG_PREVIEW_TRIS: usize = 60_000;

/// Parsed mesh for the selected asset, kept so orbit/zoom don't reparse the file.
struct PreviewMesh {
    asset_id: i64,
    mtime_ns: i64,
    full: Arc<Mesh>,
    lod: Arc<Mesh>,
}

impl PreviewMesh {
    fn new(asset: &Asset, mesh: Mesh) -> Self {
        let full = Arc::new(mesh);
        let lod = match full.simplified(DRAG_PREVIEW_TRIS) {
            std::borrow::Cow::Borrowed(_) => full.clone(),
            std::borrow::Cow::Owned(m) => Arc::new(m),
        };
        Self {
            asset_id: asset.id,
            mtime_ns: asset.mtime_ns,
            full,
            lod,
        }
    }

    fn matches(&self, asset: &Asset) -> bool {
        self.asset_id == asset.id && self.mtime_ns == asset.mtime_ns
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum ViewMode {
    #[default]
    Grid,
    List,
}

impl ViewMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::List => "list",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw.trim() {
            "list" => Self::List,
            _ => Self::Grid,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Left,
    Right,
    Up,
    Down,
}

/// Index to select after an arrow key, or `None` if the key does nothing.
fn step_index(
    current: Option<usize>,
    len: usize,
    cols: usize,
    step: Step,
    mode: ViewMode,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(current) = current else {
        return Some(0);
    };
    let cols = cols.max(1) as isize;
    let delta = match (mode, step) {
        (ViewMode::List, Step::Left | Step::Right) => return None,
        (ViewMode::List, Step::Up) => -1,
        (ViewMode::List, Step::Down) => 1,
        (ViewMode::Grid, Step::Left) => -1,
        (ViewMode::Grid, Step::Right) => 1,
        (ViewMode::Grid, Step::Up) => -cols,
        (ViewMode::Grid, Step::Down) => cols,
    };
    let next = (current as isize + delta).clamp(0, len as isize - 1) as usize;
    (next != current).then_some(next)
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
    assets: Vec<Asset>,
    selected: Option<i64>,
    selected_library: Option<i64>,
    selected_tag: Option<String>,
    search: Entity<InputState>,
    tag_input: Entity<InputState>,
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
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(i18n::t(Key::Search)));
        let tag_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(i18n::t(Key::TagsHint)));

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
            assets: Vec::new(),
            selected: None,
            selected_library: None,
            selected_tag: None,
            search,
            tag_input,
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
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if let Ok(libs) = self.catalog.libraries() {
            self.libraries = libs;
        }
        if let Ok(tags) = self.catalog.all_tags() {
            self.tags = tags;
        }
        let query = AssetQuery {
            library_id: self.selected_library,
            tag: self.selected_tag.clone(),
            search: if self.search_text.trim().is_empty() {
                None
            } else {
                Some(self.search_text.clone())
            },
            sort: self.sort,
            ..Default::default()
        };
        match self.catalog.assets(&query) {
            Ok(assets) => self.assets = assets,
            Err(err) => eprintln!("catalog query failed: {err}"),
        }
        if let Some(id) = self.selected {
            if !self.assets.iter().any(|a| a.id == id) {
                self.selected = self.assets.first().map(|a| a.id);
            }
        }
        let cache_fresh = match (&self.preview_mesh, self.selected_asset()) {
            (Some(cached), Some(asset)) => cached.matches(asset),
            _ => false,
        };
        if !cache_fresh {
            self.preview_mesh = None;
        }
        cx.notify();
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

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.assets.iter().position(|a| a.id == id)
    }

    fn move_selection(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = step_index(
            self.selected_index(),
            self.assets.len(),
            self.grid_cols,
            step,
            self.view_mode,
        ) else {
            return;
        };
        let id = self.assets[next].id;
        self.select(id, window, cx);
        self.scroll_to_selected();
    }

    fn scroll_to_selected(&self) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let row = match self.view_mode {
            ViewMode::Grid => index / self.grid_cols.max(1),
            ViewMode::List => index,
        };
        self.asset_scroll
            .scroll_to_item(row, ScrollStrategy::Nearest);
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

    fn regenerate_thumb(&mut self, id: i64, cx: &mut Context<Self>) {
        let _ = self.catalog.set_thumb_state(id, ThumbState::Pending, None);
        self.reload(cx);
        self.kick_thumbs(cx);
    }

    /// Ask, then move the file to the system Trash and drop it from the index.
    fn confirm_trash(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(asset) = self.assets.iter().find(|a| a.id == id).cloned() else {
            return;
        };
        let this = cx.entity().downgrade();
        confirm_destructive(
            window,
            cx,
            i18n::trash_confirm(asset.name()),
            i18n::t(Key::TrashDetail),
            i18n::t(Key::MoveToTrash),
            move |window, cx| {
                let _ = this.update(cx, |this, cx| this.trash_asset(&asset, window, cx));
            },
        );
    }

    fn trash_asset(&mut self, asset: &Asset, window: &mut Window, cx: &mut Context<Self>) {
        let id = asset.id;
        let path = asset.abs_path();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { move_to_trash(&path) })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    let index = this.assets.iter().position(|a| a.id == id).unwrap_or(0);
                    if let Err(err) = this.catalog.remove_asset(id) {
                        eprintln!("remove asset failed: {err}");
                    }
                    this.reload_after_removal(index, window, cx);
                }
                Err(err) => {
                    let detail = err.to_string();
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
    fn confirm_remove_library(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
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
        if self.selected_library == Some(lib.id) {
            self.selected_library = None;
        }
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
                self.preview_mesh = None;
                if let Some(old) = self.preview_image.take() {
                    cx.drop_image(old, None);
                }
                cx.notify();
            }
        }
    }

    fn selected_asset(&self) -> Option<&Asset> {
        let id = self.selected?;
        self.assets.iter().find(|a| a.id == id)
    }

    fn set_sort(&mut self, sort: AssetSort, cx: &mut Context<Self>) {
        if self.sort == sort {
            return;
        }
        self.sort = sort;
        write_pref("sort", sort.as_str());
        self.reload(cx);
    }

    fn scan_all(&mut self, cx: &mut Context<Self>) {
        self.scan_libraries(self.libraries.clone(), cx);
    }

    fn scan_libraries(&mut self, libs: Vec<Library>, cx: &mut Context<Self>) {
        if libs.is_empty() {
            return;
        }
        let catalog = self.catalog.clone();
        self.scanning = true;
        self.scan_done = 0;
        self.scan_total = 0;
        self.status = UiStatus::Scanning;
        cx.notify();

        let progress = Arc::new(Mutex::new((0u32, 0u32)));
        let progress_scan = progress.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for lib in &libs {
                let _ = catalog.scan_library_progress(lib, |done, total| {
                    *progress_scan.lock().unwrap() = (done, total);
                });
            }
            // The thumb cache can be purged behind our back; regenerate what's gone.
            let _ = catalog.requeue_missing_thumbs(|hex| thumb_path(hex).is_file());
            let _ = done_tx.send(());
        });

        cx.spawn(async move |this, cx| {
            loop {
                match done_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {
                        let snap = *progress.lock().unwrap();
                        this.update(cx, |this, cx| {
                            this.scan_done = snap.0;
                            this.scan_total = snap.1;
                            cx.notify();
                        })
                        .ok();
                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await;
                    }
                }
            }
            this.update(cx, |this, cx| {
                this.scanning = false;
                this.status = UiStatus::Idle;
                this.reload(cx);
                this.watch_libraries();
                this.kick_thumbs(cx);
            })
            .ok();
        })
        .detach();
    }

    fn add_folder(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some(i18n::t(Key::AddFolder).into()),
        });
        cx.spawn(async move |this, cx| {
            let paths = prompt
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                let mut added = Vec::new();
                for path in paths {
                    if let Ok(lib) = this.catalog.add_library(&path) {
                        if let Some(watch) = &this.watch {
                            watch.watch(lib.root_path.clone());
                        }
                        added.push(lib);
                    }
                }
                this.libraries.extend(added.iter().cloned());
                this.scan_libraries(added, cx);
            })
            .ok();
        })
        .detach();
    }

    fn add_folder_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        let root = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.to_path_buf())
        };
        if let Ok(lib) = self.catalog.add_library(&root) {
            if let Some(watch) = &self.watch {
                watch.watch(lib.root_path.clone());
            }
            self.libraries.push(lib.clone());
            self.scan_libraries(vec![lib], cx);
        }
    }

    fn kick_thumbs(&mut self, cx: &mut Context<Self>) {
        self.thumbs_wanted = true;
        if self.thumbs_running {
            return;
        }
        self.thumbs_running = true;
        self.thumb_done = 0;
        self.thumb_total = self.catalog.pending_count().unwrap_or(0);
        let catalog = self.catalog.clone();
        let done = Arc::new(AtomicU32::new(0));
        cx.spawn(async move |this, cx| loop {
            this.update(cx, |this, _cx| {
                this.thumbs_wanted = false;
            })
            .ok();
            let (catalog2, done2) = (catalog.clone(), done.clone());
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                jobs::drain_pending(&catalog2, &done2);
                let _ = done_tx.send(());
            });
            let mut last_reload = Instant::now();
            loop {
                let finished = !matches!(done_rx.try_recv(), Err(TryRecvError::Empty));
                let n = done.load(Ordering::Relaxed);
                let refresh = finished || last_reload.elapsed() >= THUMB_RELOAD_INTERVAL;
                this.update(cx, |this, cx| {
                    let progressed = this.thumb_done != n;
                    this.thumb_done = n;
                    this.thumb_total = this.thumb_total.max(n);
                    if refresh && progressed {
                        this.reload(cx);
                    } else if progressed {
                        cx.notify();
                    }
                })
                .ok();
                if finished {
                    break;
                }
                if refresh {
                    last_reload = Instant::now();
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
            let stop = this
                .update(cx, |this, cx| {
                    if this.thumbs_wanted {
                        this.thumb_total = this
                            .thumb_total
                            .max(this.thumb_done + this.catalog.pending_count().unwrap_or(0));
                        false
                    } else {
                        this.thumbs_running = false;
                        this.reload(cx);
                        true
                    }
                })
                .unwrap_or(true);
            if stop {
                break;
            }
        })
        .detach();
    }

    fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        if self.preview_mesh.as_ref().is_some_and(|m| m.asset_id != id) {
            self.preview_mesh = None;
        }
        self.camera = Camera::default();
        self.last_preview_pos = None;
        self.preview_busy = false;
        self.preview_dirty = false;
        if let Some(old) = self.preview_image.take() {
            cx.drop_image(old, None);
        }
        if let Some(asset) = self.assets.iter().find(|a| a.id == id) {
            let tags = asset.tags.join(", ");
            self.tag_input.update(cx, |input, cx| {
                input.set_value(tags, window, cx);
            });
            if let Some(thumb) = load_thumb_render_image(asset) {
                self.preview_image = Some(thumb);
            }
        }
        self.render_preview(cx);
        cx.notify();
    }

    fn render_preview(&mut self, cx: &mut Context<Self>) {
        let Some(asset) = self.selected_asset().cloned() else {
            if let Some(old) = self.preview_image.take() {
                cx.drop_image(old, None);
            }
            return;
        };
        if self.preview_busy {
            self.preview_dirty = true;
            return;
        }
        self.preview_busy = true;
        self.preview_dirty = false;
        if self.preview_image.is_none() {
            self.status = UiStatus::Loading;
        }
        let camera = self.camera;
        let selected = asset.id;
        let background = hsla_to_rgba(cx.theme().muted);
        let dragging = self.last_preview_pos.is_some();
        let cached = self
            .preview_mesh
            .as_ref()
            .filter(|m| m.matches(&asset))
            .map(|m| {
                if dragging {
                    m.lod.clone()
                } else {
                    m.full.clone()
                }
            });
        cx.spawn(async move |this, cx| {
            let (rendered, loaded) = cx
                .background_spawn(async move {
                    let draw = |mesh: &Mesh| {
                        let img =
                            render_mesh(mesh, &camera, PREVIEW_SIZE, PREVIEW_SIZE, background);
                        Some(rgba_to_render_image(img))
                    };
                    if let Some(mesh) = cached {
                        return (draw(&mesh), None);
                    }
                    match load_mesh(&asset.abs_path()) {
                        Ok(mesh) => {
                            let entry = PreviewMesh::new(&asset, mesh);
                            let mesh = if dragging { &entry.lod } else { &entry.full };
                            (draw(mesh), Some(entry))
                        }
                        Err(err) => {
                            eprintln!("preview load failed: {err}");
                            (load_thumb_render_image(&asset), None)
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.preview_busy = false;
                if this.selected != Some(selected) {
                    if let Some(image) = rendered {
                        cx.drop_image(image, None);
                    }
                    return;
                }
                if loaded.is_some() {
                    this.preview_mesh = loaded;
                }
                if let Some(image) = rendered {
                    if let Some(old) = this.preview_image.replace(image) {
                        cx.drop_image(old, None);
                    }
                    this.status = UiStatus::Idle;
                } else if this.preview_image.is_none() {
                    this.status = UiStatus::Failed;
                }
                cx.notify();
                if this.preview_dirty {
                    this.render_preview(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn apply_tags(&mut self, raw: &str, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            return;
        };
        let tags: Vec<String> = raw
            .split(|c| c == ',' || c == '，' || c == '、')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let _ = self.catalog.set_tags(id, &tags);
        self.reload(cx);
    }

    fn open_selected(&mut self, _cx: &mut Context<Self>) {
        if let Some(asset) = self.selected_asset() {
            let _ = open_path(&asset.abs_path());
        }
    }

    fn watch_libraries(&mut self) {
        let Some(watch) = &self.watch else {
            return;
        };
        for lib in &self.libraries {
            watch.watch(lib.root_path.clone());
        }
    }

    fn start_watch_loop(&mut self, cx: &mut Context<Self>) {
        let Some(events) = self.watch.as_ref().map(|w| w.events()) else {
            return;
        };
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let mut changed = {
                let rx = events.lock().unwrap();
                let mut paths = Vec::new();
                while let Ok(p) = rx.try_recv() {
                    paths.push(p);
                }
                paths
            };
            if changed.is_empty() {
                continue;
            }
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            {
                let rx = events.lock().unwrap();
                while let Ok(p) = rx.try_recv() {
                    changed.push(p);
                }
            }
            this.update(cx, |this, cx| {
                let mut libs = Vec::new();
                for path in &changed {
                    if let Some(lib) = affected_library(path, &this.libraries) {
                        if !libs.iter().any(|l: &Library| l.id == lib.id) {
                            libs.push(lib.clone());
                        }
                    }
                }
                if !libs.is_empty() {
                    this.scan_libraries(libs, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_preview_drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            // Drag just ended: replace the low-poly frame with a full-quality one.
            if self.last_preview_pos.take().is_some() {
                self.render_preview(cx);
            }
            return;
        }
        if let Some(prev) = self.last_preview_pos {
            let dx = event.position.x.as_f32() - prev.x.as_f32();
            let dy = event.position.y.as_f32() - prev.y.as_f32();
            self.camera.orbit(dx, dy);
            self.render_preview(cx);
        }
        self.last_preview_pos = Some(event.position);
    }

    fn on_preview_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = match event.delta {
            ScrollDelta::Pixels(p) => p.y.as_f32(),
            ScrollDelta::Lines(l) => l.y * 24.0,
        };
        self.camera.zoom(delta);
        self.render_preview(cx);
    }
}

fn hsla_to_rgba(color: Hsla) -> [u8; 4] {
    let rgb = color.to_rgb();
    [
        (rgb.r * 255.0).round() as u8,
        (rgb.g * 255.0).round() as u8,
        (rgb.b * 255.0).round() as u8,
        255,
    ]
}

fn rgba_to_render_image(mut img: image::RgbaImage) -> Arc<RenderImage> {
    for px in img.pixels_mut() {
        px.0.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(img)]))
}

fn load_thumb_render_image(asset: &Asset) -> Option<Arc<RenderImage>> {
    let hex = asset.sha256_hex()?;
    let path = thumb_path(&hex);
    let img = image::open(&path).ok()?.to_rgba8();
    Some(rgba_to_render_image(img))
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        (self.grid_cols, self.grid_card_width) =
            grid_layout(window.viewport_size().width.as_f32(), self.card_size);
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &AddFolder, window, cx| this.add_folder(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSelected, _, cx| this.open_selected(cx)))
            .on_action(cx.listener(|this, _: &TrashSelected, window, cx| {
                if let Some(id) = this.selected {
                    this.confirm_trash(id, window, cx);
                }
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

fn toolbar(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
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
        .child(
            div().w(px(320.)).child(
                Input::new(&this.search)
                    .cleanable(true)
                    .prefix(Icon::new(IconName::Search).small()),
            ),
        )
        .child(
            Button::new("sort")
                .ghost()
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
            Button::new("add-folder")
                .primary()
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
        .text_xs()
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

fn sort_is_ascending(sort: AssetSort) -> bool {
    matches!(
        sort,
        AssetSort::NameAsc
            | AssetSort::ModifiedAsc
            | AssetSort::SizeAsc
            | AssetSort::TrianglesAsc
            | AssetSort::Format
    )
}

fn sidebar(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let workspace = cx.entity().downgrade();
    v_flex()
        .id("sidebar")
        .w(px(SIDEBAR_WIDTH))
        .flex_shrink_0()
        .h_full()
        .min_h_0()
        .pb_3()
        .overflow_y_scroll()
        .border_r_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .child(sidebar_section(i18n::t(Key::Folders), &theme))
        .child(nav_item(
            "nav-all",
            Lucide::Boxes.into(),
            i18n::t(Key::AllAssets).to_string(),
            None,
            this.selected_library.is_none() && this.selected_tag.is_none(),
            false,
            cx.listener(|this, _, _, cx| {
                this.selected_library = None;
                this.selected_tag = None;
                this.reload(cx);
            }),
            &theme,
        ))
        .children(this.libraries.iter().map(|lib| {
            let id = lib.id;
            let label = library_label(lib).to_string();
            let online = lib.root_path.exists();
            let selected = this.selected_library == Some(id);
            nav_item(
                format!("nav-lib-{id}"),
                if selected {
                    IconName::FolderOpen.into()
                } else {
                    IconName::Folder.into()
                },
                label,
                (!online).then(|| i18n::t(Key::Offline).to_string()),
                selected,
                !online,
                cx.listener(move |this, _, _, cx| {
                    this.selected_library = Some(id);
                    this.selected_tag = None;
                    this.reload(cx);
                }),
                &theme,
            )
            .context_menu({
                let workspace = workspace.clone();
                move |menu, _, _| {
                    let workspace = workspace.clone();
                    menu.item(
                        PopupMenuItem::new(i18n::t(Key::RemoveFolderEllipsis))
                            .icon(Icon::new(Lucide::FolderMinus))
                            .on_click(move |_, window, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.confirm_remove_library(id, window, cx)
                                });
                            }),
                    )
                }
            })
        }))
        .when(!this.tags.is_empty(), |el| {
            el.child(sidebar_section(i18n::t(Key::Tags), &theme))
        })
        .children(this.tags.iter().cloned().map(|(name, count)| {
            let selected = this.selected_tag.as_deref() == Some(name.as_str());
            let tag_name = name.clone();
            nav_item(
                format!("nav-tag-{name}"),
                Lucide::Hash.into(),
                name,
                Some(count.to_string()),
                selected,
                false,
                cx.listener(move |this, _, _, cx| {
                    this.selected_tag = Some(tag_name.clone());
                    this.selected_library = None;
                    this.reload(cx);
                }),
                &theme,
            )
        }))
}

/// Confirm a destructive action. Cancel is the default: Enter and Escape
/// both dismiss, and only clicking the danger button runs `on_confirm`.
fn confirm_destructive(
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
                        Button::new("confirm-cancel")
                            .primary()
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

fn library_label(lib: &Library) -> &str {
    lib.root_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| i18n::t(Key::FolderFallback))
}

fn sidebar_section(title: &'static str, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    div()
        .px_4()
        .pt_4()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.muted_foreground)
        .child(title)
}

#[allow(clippy::too_many_arguments)]
fn nav_item(
    id: impl Into<SharedString>,
    icon: Icon,
    label: String,
    trailing: Option<String>,
    selected: bool,
    dimmed: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    h_flex()
        .id(ElementId::Name(id.into()))
        .mx_2()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .when(dimmed, |el| el.text_color(theme.muted_foreground))
        .when(selected, |el| {
            el.bg(theme.accent).text_color(theme.accent_foreground)
        })
        .hover(|el| el.bg(theme.accent))
        .on_click(on_click)
        .child(icon.small())
        .child(div().flex_1().min_w_0().truncate().child(label))
        .when_some(trailing, |el, trailing| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(trailing),
            )
        })
}

fn card_slider_state(value: f32) -> SliderState {
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
fn grid_layout(window_width: f32, target: f32) -> (usize, f32) {
    let usable = (window_width - SIDEBAR_WIDTH - INSPECTOR_WIDTH - GRID_PADDING * 2.0).max(0.0);
    let cols = (((usable + CARD_GAP) / (target + CARD_GAP)).floor() as usize).max(1);
    let width = (usable + CARD_GAP) / cols as f32 - CARD_GAP;
    (cols, width.max(CARD_MIN))
}

fn asset_pane(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
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
                .child(i18n::t(Key::EmptyFolder))
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
    let selected = this.selected;
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
                                selected == Some(asset.id),
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

/// Click selects (and focuses the asset view for arrow keys), double-click
/// opens, right-click selects and shows the asset menu.
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
            if this.selected != Some(id) {
                this.select(id, window, cx);
            }
        });
    })
    .on_click(move |event: &ClickEvent, window, cx| {
        let dbl = event.click_count() > 1;
        let _ = on_click.update(cx, |this, cx| {
            window.focus(&this.asset_focus, cx);
            if dbl {
                this.selected = Some(id);
                this.open_selected(cx);
            } else {
                this.select(id, window, cx);
            }
        });
    })
    .context_menu(move |menu, _, _| asset_menu(menu, id, path.clone(), workspace.clone()))
}

fn asset_menu(
    menu: PopupMenu,
    id: i64,
    path: std::path::PathBuf,
    workspace: WeakEntity<Workspace>,
) -> PopupMenu {
    let (open, reveal, copy) = (path.clone(), path.clone(), path);
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
    .item({
        let workspace = workspace.clone();
        PopupMenuItem::new(i18n::t(Key::RegenerateThumb))
            .icon(IconName::RotateCw)
            .on_click(move |_, _, cx| {
                let _ = workspace.update(cx, |this, cx| this.regenerate_thumb(id, cx));
            })
    })
    .separator()
    .item(
        PopupMenuItem::new(i18n::t(Key::MoveToTrashEllipsis))
            .icon(Icon::new(Lucide::Trash))
            .on_click(move |_, window, cx| {
                let _ = workspace.update(cx, |this, cx| this.confirm_trash(id, window, cx));
            }),
    )
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
                ),
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
    let selected = this.selected;
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
                    .map(|asset| list_row(asset, selected == Some(asset.id), workspace.clone(), cx))
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
                    div()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(asset.name().to_string()),
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

fn inspector(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
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
        .when_some(this.selected_asset(), |el, asset| {
            el.child(asset_details(this, asset, cx))
        })
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
                )),
        )
        .child(Input::new(&this.tag_input).prefix(Icon::new(Lucide::Tag).small()))
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
                            this.confirm_trash(id, window, cx)
                        }))
                }),
        )
}

fn read_pref(name: &str) -> Option<String> {
    std::fs::read_to_string(pam_core::paths::data_dir().join(name)).ok()
}

fn write_pref(name: &str, value: &str) {
    let _ = std::fs::write(pam_core::paths::data_dir().join(name), value);
}

/// 1234567 → "1,234,567".
fn group_digits(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Unix nanoseconds → "YYYY-MM-DD" (UTC).
fn format_date(mtime_ns: i64) -> String {
    let days = mtime_ns.div_euclid(86_400 * 1_000_000_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
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
mod helper_tests {
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
    fn arrow_keys_move_within_grid_and_list() {
        use super::{step_index, Step, ViewMode};
        let grid = |cur, step| step_index(cur, 10, 4, step, ViewMode::Grid);
        assert_eq!(grid(None, Step::Down), Some(0));
        assert_eq!(grid(Some(1), Step::Right), Some(2));
        assert_eq!(grid(Some(1), Step::Down), Some(5));
        assert_eq!(grid(Some(5), Step::Up), Some(1));
        // Partial last row clamps to the last asset; edges are no-ops.
        assert_eq!(grid(Some(7), Step::Down), Some(9));
        assert_eq!(grid(Some(0), Step::Left), None);
        assert_eq!(grid(Some(9), Step::Right), None);

        let list = |cur, step| step_index(cur, 3, 4, step, ViewMode::List);
        assert_eq!(list(Some(0), Step::Down), Some(1));
        assert_eq!(list(Some(1), Step::Up), Some(0));
        assert_eq!(list(Some(1), Step::Right), None);
        assert_eq!(step_index(None, 0, 4, Step::Down, ViewMode::Grid), None);
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

    #[::core::prelude::v1::test]
    fn formats_counts_and_dates() {
        assert_eq!(super::group_digits(0), "0");
        assert_eq!(super::group_digits(999), "999");
        assert_eq!(super::group_digits(1_234_567), "1,234,567");
        assert_eq!(super::group_digits(-12_000), "-12,000");
        assert_eq!(super::format_date(0), "1970-01-01");
        // 2024-02-29T12:00:00Z
        assert_eq!(
            super::format_date(1_709_208_000 * 1_000_000_000),
            "2024-02-29"
        );
        // 2026-09-28T00:00:00Z
        assert_eq!(
            super::format_date(1_790_553_600 * 1_000_000_000),
            "2026-09-28"
        );
    }

    #[::core::prelude::v1::test]
    fn hsla_to_rgba_converts_black_and_white() {
        assert_eq!(super::hsla_to_rgba(gpui_kit::black()), [0, 0, 0, 255]);
        assert_eq!(super::hsla_to_rgba(gpui_kit::white()), [255, 255, 255, 255]);
    }
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
