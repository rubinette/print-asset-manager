use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogButtonProps, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme, Disableable as _, Icon, IconName, Root, Selectable as _,
    Sizable as _, TitleBar, WindowExt as _,
};
use gpui_kit::{prelude::FluentBuilder as _, Focusable as _, *};
use pam_core::catalog::{Asset, AssetQuery, AssetSort, Catalog, Library, TagMode, ThumbState};
use pam_core::load::load_mesh;
use pam_core::mesh::{AssetFormat, Mesh};
use pam_core::open::{move_to_trash, open_path};
use pam_core::paths::thumb_path;
use pam_core::watch::{affected_library, WatchHandle};
use pam_preview::render_mesh;
use pam_preview::Camera;

use crate::i18n::{self, Key, LanguagePref};
use crate::jobs;
use crate::{
    apply_language, apply_system_theme, AddFolder, FocusSearch, OpenSelected, SelectAll,
    SelectDown, SelectLeft, SelectRight, SelectUp, TrashSelected, UseChinese, UseEnglish,
    UseSystemLanguage,
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
/// Triangle budget for the still preview. Past this a 640 px frame shows no
/// more detail, while a 48M-triangle scan took 2 s per frame.
const STILL_PREVIEW_TRIS: usize = 2_000_000;

/// Parsed mesh for the selected asset, kept so orbit/zoom don't reparse the file.
struct PreviewMesh {
    asset_id: i64,
    mtime_ns: i64,
    full: Arc<Mesh>,
    lod: Arc<Mesh>,
}

impl PreviewMesh {
    fn new(asset: &Asset, mesh: Mesh) -> Self {
        // Owned result drops the original, which for huge scans is most of
        // the memory held while the preview is open.
        let full = Arc::new(match mesh.simplified(STILL_PREVIEW_TRIS) {
            std::borrow::Cow::Borrowed(_) => mesh,
            std::borrow::Cow::Owned(m) => m,
        });
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

/// What the sidebar has picked; filters narrow it further.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
enum Location {
    #[default]
    All,
    /// A library, or a folder inside it (`dir` relative to its root).
    Library {
        id: i64,
        dir: Option<String>,
    },
    Duplicates,
}

/// File size ranges in MB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SizeBucket {
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

    fn range(self) -> (u64, Option<u64>) {
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
enum ModifiedWithin {
    Week,
    Month,
    Year,
}

impl ModifiedWithin {
    const ALL: [Self; 3] = [Self::Week, Self::Month, Self::Year];

    fn days(self) -> i64 {
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
struct Filters {
    formats: Vec<AssetFormat>,
    fits_bed: bool,
    file_size: Option<SizeBucket>,
    modified: Option<ModifiedWithin>,
    tags: Vec<String>,
    tag_mode: TagMode,
    untagged: bool,
    thumb_failed: bool,
}

impl Filters {
    /// How many filter groups are narrowing the view (for the toolbar badge).
    fn active_count(&self) -> usize {
        [
            !self.formats.is_empty(),
            self.fits_bed,
            self.file_size.is_some(),
            self.modified.is_some(),
            !self.tags.is_empty(),
            self.untagged || self.thumb_failed,
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
const DEFAULT_BED: [f32; 3] = [256.0, 256.0, 256.0];

/// "256x256x256" (also `×`, `*`, spaces) → X, Y, Z. All three must be positive.
fn parse_bed(raw: &str) -> Option<[f32; 3]> {
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

/// "a, b，c、d" → trimmed, non-empty tags.
fn parse_tags(raw: &str) -> Vec<String> {
    raw.split([',', '，', '、'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A folder inside a library; the root node has an empty `path`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FolderNode {
    name: String,
    /// `/`-separated, relative to the library root.
    path: String,
    /// Assets in this folder and all its subfolders.
    count: usize,
    /// Sorted by name, case-insensitively.
    children: Vec<FolderNode>,
}

/// Folder tree of a library from its assets' relative paths.
fn build_folder_tree<'a>(rel_paths: impl IntoIterator<Item = &'a str>) -> FolderNode {
    #[derive(Default)]
    struct Building {
        count: usize,
        children: BTreeMap<String, Building>,
    }
    fn finish(name: String, path: String, node: Building) -> FolderNode {
        let mut children: Vec<FolderNode> = node
            .children
            .into_iter()
            .map(|(name, child)| {
                let path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                };
                finish(name, path, child)
            })
            .collect();
        children.sort_by_key(|c| c.name.to_lowercase());
        FolderNode {
            name,
            path,
            count: node.count,
            children,
        }
    }

    let mut root = Building::default();
    for rel in rel_paths {
        root.count += 1;
        let mut node = &mut root;
        let mut parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
        parts.pop(); // the file itself
        for part in parts {
            node = node.children.entry(part.to_string()).or_default();
            node.count += 1;
        }
    }
    finish(String::new(), String::new(), root)
}

fn folder_exists(root: &FolderNode, path: &str) -> bool {
    path.split('/')
        .try_fold(root, |node, part| {
            node.children.iter().find(|c| c.name == part)
        })
        .is_some()
}

/// Subfolder rows to show under `root`, depth-first, descending only into
/// folders whose path is in `expanded`. Depth starts at 1.
fn visible_folders(
    root: &FolderNode,
    expanded: impl Fn(&str) -> bool,
) -> Vec<(usize, &FolderNode)> {
    fn walk<'a>(
        node: &'a FolderNode,
        depth: usize,
        expanded: &dyn Fn(&str) -> bool,
        out: &mut Vec<(usize, &'a FolderNode)>,
    ) {
        for child in &node.children {
            out.push((depth, child));
            if expanded(&child.path) {
                walk(child, depth + 1, expanded, out);
            }
        }
    }
    let mut out = Vec::new();
    if expanded(&root.path) {
        walk(root, 1, &expanded, &mut out);
    }
    out
}

/// Ids from `anchor` to `target` inclusive, in list order. Without a usable
/// anchor, just `target`.
fn range_ids(assets: &[Asset], anchor: Option<i64>, target: i64) -> Vec<i64> {
    let find = |id: i64| assets.iter().position(|a| a.id == id);
    let Some(to) = find(target) else {
        return Vec::new();
    };
    let from = anchor.and_then(find).unwrap_or(to);
    let (lo, hi) = (from.min(to), from.max(to));
    assets[lo..=hi].iter().map(|a| a.id).collect()
}

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
    /// Assets sharing the primary selection's content hash, itself included.
    selected_copies: u32,
    search: Entity<InputState>,
    tag_input: Entity<InputState>,
    /// Inspector input that adds tags to every selected asset.
    batch_tag_input: Entity<InputState>,
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
            selected_copies: 0,
            search,
            tag_input,
            batch_tag_input,
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
        cx.notify();
    }

    fn query(&self) -> AssetQuery {
        let (library_id, dir) = match &self.location {
            Location::Library { id, dir } => (Some(*id), dir.clone()),
            Location::All | Location::Duplicates => (None, None),
        };
        let f = &self.filters;
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
            thumb_state: f.thumb_failed.then_some(ThumbState::Failed),
            duplicates_only: self.location == Location::Duplicates,
            search: if self.search_text.trim().is_empty() {
                None
            } else {
                Some(self.search_text.clone())
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

    /// Rebuild the sidebar folder trees. Separate from `reload` because that
    /// runs every half second while thumbnails are generated.
    fn reload_folders(&mut self) {
        self.folder_trees = self
            .libraries
            .iter()
            .filter_map(|lib| {
                let paths = self.catalog.rel_paths(lib.id).ok()?;
                Some((lib.id, build_folder_tree(paths.iter().map(String::as_str))))
            })
            .collect();
        // Drop expansions of folders that no longer exist.
        let trees = &self.folder_trees;
        self.expanded.retain(|(lib, path)| {
            trees
                .get(lib)
                .is_some_and(|tree| path.is_empty() || folder_exists(tree, path))
        });
        if let Location::Library { id, dir: Some(dir) } = &self.location {
            if !trees.get(id).is_some_and(|tree| folder_exists(tree, dir)) {
                self.location = Location::Library { id: *id, dir: None };
            }
        }
    }

    fn refresh_copies(&mut self) {
        self.selected_copies = self
            .selected_asset()
            .and_then(|a| a.content_sha256.as_deref())
            .and_then(|sha| self.catalog.copies(sha).ok())
            .unwrap_or(0);
    }

    fn set_location(&mut self, location: Location, cx: &mut Context<Self>) {
        self.location = location;
        self.reload(cx);
    }

    fn toggle_expanded(&mut self, lib: i64, path: String, cx: &mut Context<Self>) {
        let key = (lib, path);
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        cx.notify();
    }

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

    /// Sidebar tag click: plain click shows just that tag; ⌘/Ctrl-click adds
    /// it to (or drops it from) the tag filter.
    fn click_tag(&mut self, name: String, additive: bool, cx: &mut Context<Self>) {
        if additive {
            if let Some(i) = self.filters.tags.iter().position(|t| *t == name) {
                self.filters.tags.remove(i);
            } else {
                self.filters.tags.push(name);
            }
        } else {
            self.filters.tags = vec![name];
            self.location = Location::All;
        }
        self.reload(cx);
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

    fn regenerate_thumbs(&mut self, ids: &[i64], cx: &mut Context<Self>) {
        for &id in ids {
            let _ = self.catalog.set_thumb_state(id, ThumbState::Pending, None);
        }
        self.reload(cx);
        self.kick_thumbs(cx);
    }

    /// What a menu or shortcut acts on for `id`: the whole selection if `id`
    /// is part of it, otherwise just `id`.
    fn targets_for(&self, id: i64) -> Vec<i64> {
        if self.selection.contains(&id) {
            self.selection.clone()
        } else {
            vec![id]
        }
    }

    /// Ask, then move the files to the system Trash and drop them from the index.
    fn confirm_trash(&mut self, ids: &[i64], window: &mut Window, cx: &mut Context<Self>) {
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
                this.reload_folders();
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

    /// Select only `id`.
    fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = vec![id];
        self.anchor = Some(id);
        self.focus_asset(id, window, cx);
    }

    /// ⌘/Ctrl-click: add `id` to the selection, or take it out.
    fn toggle_select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.anchor = Some(id);
        if let Some(i) = self.selection.iter().position(|s| *s == id) {
            self.selection.remove(i);
            if self.selected == Some(id) {
                match self.selection.last().copied() {
                    Some(next) => self.focus_asset(next, window, cx),
                    None => self.clear_selection(cx),
                }
            } else {
                cx.notify();
            }
        } else {
            self.selection.push(id);
            self.focus_asset(id, window, cx);
        }
    }

    /// Shift-click: select everything from the anchor to `id`.
    fn extend_select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = range_ids(&self.assets, self.anchor, id);
        if self.anchor.is_none() {
            self.anchor = Some(id);
        }
        self.focus_asset(id, window, cx);
    }

    fn select_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = self.assets.iter().map(|a| a.id).collect();
        match self.selected.or_else(|| self.assets.first().map(|a| a.id)) {
            Some(id) if self.selected != Some(id) => self.focus_asset(id, window, cx),
            _ => cx.notify(),
        }
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.selection.clear();
        self.preview_mesh = None;
        if let Some(old) = self.preview_image.take() {
            cx.drop_image(old, None);
        }
        cx.notify();
    }

    /// Make `id` the primary selection: preview it and load its tags into the
    /// inspector. Leaves `selection` alone.
    fn focus_asset(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.refresh_copies();
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
        let _ = self.catalog.set_tags(id, &parse_tags(raw));
        self.reload(cx);
    }

    fn add_tags_to_selection(&mut self, raw: &str, cx: &mut Context<Self>) {
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
        .child({
            let active = this.filters.active_count();
            Button::new("filter")
                .ghost()
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

/// Dropdowns under the toolbar; each button's label shows its current value.
fn filter_bar(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
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
            this.location == Location::All && this.filters.tags.is_empty(),
            false,
            cx.listener(|this, _, _, cx| {
                this.location = Location::All;
                this.filters.tags.clear();
                this.reload(cx);
            }),
            &theme,
        ))
        .when(this.duplicate_count > 0, |el| {
            el.child(nav_item(
                "nav-duplicates",
                Lucide::Files.into(),
                i18n::t(Key::Duplicates).to_string(),
                Some(this.duplicate_count.to_string()),
                this.location == Location::Duplicates,
                false,
                cx.listener(|this, _, _, cx| this.set_location(Location::Duplicates, cx)),
                &theme,
            ))
        })
        .children(
            this.libraries
                .iter()
                .flat_map(|lib| library_rows(this, lib, cx))
                .collect::<Vec<_>>(),
        )
        .when(!this.tags.is_empty(), |el| {
            el.child(sidebar_section(i18n::t(Key::Tags), &theme))
        })
        .children(this.tags.iter().cloned().map(|(name, count)| {
            let selected = this.filters.tags.contains(&name);
            let tag_name = name.clone();
            nav_item(
                format!("nav-tag-{name}"),
                Lucide::Hash.into(),
                name,
                Some(count.to_string()),
                selected,
                false,
                cx.listener(move |this, event: &ClickEvent, _, cx| {
                    this.click_tag(tag_name.clone(), event.modifiers().secondary(), cx);
                }),
                &theme,
            )
        }))
}

/// A library row and, when expanded, its subfolder rows.
fn library_rows(this: &Workspace, lib: &Library, cx: &mut Context<Workspace>) -> Vec<AnyElement> {
    let theme = cx.theme().clone();
    let workspace = cx.entity().downgrade();
    let id = lib.id;
    let online = lib.root_path.exists();
    let tree = this.folder_trees.get(&id);
    let is_expanded = |path: &str| this.expanded.contains(&(id, path.to_string()));
    let selected_dir = match &this.location {
        Location::Library { id: lib_id, dir } if *lib_id == id => Some(dir.as_deref()),
        _ => None,
    };

    let root_selected = selected_dir == Some(None);
    let root_item = nav_item(
        format!("nav-lib-{id}"),
        if root_selected {
            IconName::FolderOpen.into()
        } else {
            IconName::Folder.into()
        },
        library_label(lib).to_string(),
        if online {
            tree.map(|t| t.count.to_string())
        } else {
            Some(i18n::t(Key::Offline).to_string())
        },
        root_selected,
        !online,
        cx.listener(move |this, _, _, cx| {
            this.set_location(Location::Library { id, dir: None }, cx)
        }),
        &theme,
    )
    .ml_0p5()
    .context_menu(move |menu, _, _| {
        let workspace = workspace.clone();
        menu.item(
            PopupMenuItem::new(i18n::t(Key::RemoveFolderEllipsis))
                .icon(Icon::new(Lucide::FolderMinus))
                .on_click(move |_, window, cx| {
                    let _ = workspace
                        .update(cx, |this, cx| this.confirm_remove_library(id, window, cx));
                }),
        )
    });

    let has_children = tree.is_some_and(|t| !t.children.is_empty());
    let mut rows = vec![tree_row(
        format!("lib-{id}"),
        0,
        has_children.then(|| is_expanded("")),
        cx.listener(move |this, _, _, cx| this.toggle_expanded(id, String::new(), cx)),
        root_item,
        &theme,
    )];
    let Some(tree) = tree else {
        return rows;
    };
    for (depth, folder) in visible_folders(tree, is_expanded) {
        let path = folder.path.clone();
        let selected = selected_dir == Some(Some(path.as_str()));
        let item = nav_item(
            format!("nav-dir-{id}-{path}"),
            if selected {
                IconName::FolderOpen.into()
            } else {
                IconName::Folder.into()
            },
            folder.name.clone(),
            Some(folder.count.to_string()),
            selected,
            !online,
            cx.listener({
                let path = path.clone();
                move |this, _, _, cx| {
                    this.set_location(
                        Location::Library {
                            id,
                            dir: Some(path.clone()),
                        },
                        cx,
                    )
                }
            }),
            &theme,
        )
        .ml_0p5();
        let toggle_path = path.clone();
        rows.push(tree_row(
            format!("dir-{id}-{path}"),
            depth,
            (!folder.children.is_empty()).then(|| is_expanded(&path)),
            cx.listener(move |this, _, _, cx| this.toggle_expanded(id, toggle_path.clone(), cx)),
            item,
            &theme,
        ));
    }
    rows
}

/// Indents a sidebar item and puts a disclosure chevron in front of it.
/// `expanded` is `None` for folders with nothing to expand.
fn tree_row(
    id: String,
    depth: usize,
    expanded: Option<bool>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    item: impl IntoElement,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    h_flex()
        .pl(px(8.0 + depth as f32 * 12.0))
        .items_center()
        .child(
            div()
                .id(ElementId::Name(format!("toggle-{id}").into()))
                .size(px(16.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .when_some(expanded, |el, open| {
                    el.cursor_pointer().on_click(on_toggle).child(
                        Icon::new(if open {
                            Lucide::ChevronDown
                        } else {
                            Lucide::ChevronRight
                        })
                        .xsmall(),
                    )
                }),
        )
        .child(div().flex_1().min_w_0().child(item))
        .into_any_element()
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
    .context_menu(move |menu, _, cx| {
        let targets = workspace
            .upgrade()
            .map(|w| w.read(cx).targets_for(id))
            .unwrap_or_else(|| vec![id]);
        asset_menu(menu, targets, path.clone(), workspace.clone())
    })
}

/// Open / reveal / copy act on the clicked asset; thumbnail and trash items
/// act on every target (the selection, when the click was inside it).
fn asset_menu(
    menu: PopupMenu,
    targets: Vec<i64>,
    path: std::path::PathBuf,
    workspace: WeakEntity<Workspace>,
) -> PopupMenu {
    let (open, reveal, copy) = (path.clone(), path.clone(), path);
    let many = targets.len() > 1;
    let trash_label = if many {
        i18n::move_n_to_trash(targets.len())
    } else {
        i18n::t(Key::MoveToTrashEllipsis).to_string()
    };
    let regenerate_targets = targets.clone();
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
        .map(|el| {
            if this.selection.len() > 1 {
                el.child(batch_details(this, cx))
            } else if let Some(asset) = this.selected_asset() {
                el.child(asset_details(this, asset, cx))
            } else {
                el
            }
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
                ))
                .when(this.selected_copies > 1, |el| {
                    el.child(meta_row(
                        i18n::t(Key::Copies),
                        this.selected_copies.to_string(),
                        &theme,
                    ))
                }),
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
    fn folder_tree_counts_nested_assets_and_sorts_children() {
        use super::build_folder_tree;
        let tree = build_folder_tree([
            "top.stl",
            "b/one.stl",
            "B2/x.stl",
            "a/deep/two.stl",
            "a/deep/three.stl",
            "a/four.stl",
        ]);
        assert_eq!(tree.count, 6);
        assert_eq!(tree.path, "");
        let names: Vec<&str> = tree.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "B2"]);
        let a = &tree.children[0];
        assert_eq!((a.path.as_str(), a.count), ("a", 3));
        assert_eq!(a.children[0].path, "a/deep");
        assert_eq!(a.children[0].count, 2);
        assert!(build_folder_tree(["only.stl"]).children.is_empty());
    }

    #[::core::prelude::v1::test]
    fn visible_folders_follow_expansion() {
        use super::{build_folder_tree, folder_exists, visible_folders};
        let tree = build_folder_tree(["a/b/c/x.stl", "d/y.stl"]);
        let rows = |open: &[&str]| {
            visible_folders(&tree, |p| open.contains(&p))
                .into_iter()
                .map(|(depth, node)| (depth, node.path.clone()))
                .collect::<Vec<_>>()
        };
        assert!(rows(&[]).is_empty(), "collapsed library shows nothing");
        assert_eq!(rows(&[""]), [(1, "a".into()), (1, "d".into())]);
        assert_eq!(
            rows(&["", "a", "a/b"]),
            [
                (1, "a".into()),
                (2, "a/b".into()),
                (3, "a/b/c".into()),
                (1, "d".into())
            ]
        );
        // A collapsed parent hides expanded descendants.
        assert_eq!(rows(&["", "a/b"]), [(1, "a".into()), (1, "d".into())]);
        assert!(folder_exists(&tree, "a/b/c"));
        assert!(!folder_exists(&tree, "a/c"));
    }

    fn asset(id: i64) -> pam_core::catalog::Asset {
        pam_core::catalog::Asset {
            id,
            library_id: 1,
            root_path: "/lib".into(),
            rel_path: format!("{id}.stl"),
            format: pam_core::mesh::AssetFormat::Stl,
            size_bytes: 0,
            mtime_ns: 0,
            content_sha256: None,
            triangle_count: None,
            bbox: None,
            thumb_state: pam_core::catalog::ThumbState::Pending,
            error: None,
            tags: Vec::new(),
        }
    }

    #[::core::prelude::v1::test]
    fn shift_click_selects_range_in_list_order() {
        use super::range_ids;
        let assets: Vec<_> = [10, 20, 30, 40].into_iter().map(asset).collect();
        assert_eq!(range_ids(&assets, Some(20), 40), [20, 30, 40]);
        assert_eq!(range_ids(&assets, Some(40), 20), [20, 30, 40]);
        assert_eq!(range_ids(&assets, None, 30), [30]);
        // Anchor filtered out of the list: fall back to the clicked asset.
        assert_eq!(range_ids(&assets, Some(99), 30), [30]);
        assert!(range_ids(&assets, Some(10), 99).is_empty());
    }

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
    fn tags_split_on_ascii_and_cjk_commas() {
        assert_eq!(
            super::parse_tags(" a, b，c、 ,d "),
            ["a", "b", "c", "d"].map(String::from)
        );
        assert!(super::parse_tags(" , ").is_empty());
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
            ..Default::default()
        };
        assert_eq!(f.active_count(), 3);
        // Buckets tile the size range without gaps.
        let ranges = SizeBucket::ALL.map(SizeBucket::range);
        assert_eq!(ranges[0].0, 0);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].1, Some(pair[1].0));
        }
        assert_eq!(ranges[3].1, None);
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
