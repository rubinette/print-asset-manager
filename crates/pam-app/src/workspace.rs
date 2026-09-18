use std::path::Path;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Icon, IconName, Sizable as _, TitleBar};
use gpui_kit::{prelude::FluentBuilder as _, Focusable as _, *};
use pam_core::catalog::{Asset, AssetQuery, Catalog, Library, ThumbState};
use pam_core::load::load_mesh;
use pam_core::open::open_path;
use pam_core::paths::thumb_path;
use pam_core::watch::{affected_library, WatchHandle};
use pam_preview::render_mesh;
use pam_preview::Camera;

use crate::i18n;
use crate::jobs::{self, THUMB_CONCURRENCY};
use crate::{AddFolder, FocusSearch, OpenSelected};

const CARD: f32 = 156.0;
const PREVIEW_SIZE: u32 = 480;

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
    scanning: bool,
    status: SharedString,
    camera: Camera,
    preview_image: Option<Arc<RenderImage>>,
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
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let catalog =
            Arc::new(Catalog::open(&pam_core::paths::catalog_db_path()).expect("open catalog"));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(i18n::SEARCH));
        let tag_input = cx.new(|cx| InputState::new(window, cx).placeholder(i18n::TAGS_HINT));

        let mut subs = Vec::new();
        subs.push(
            cx.subscribe_in(&search, window, |this, state, event, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.search_text = state.read(cx).value().to_string();
                    this.reload(cx);
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
            scanning: false,
            status: "".into(),
            camera: Camera::default(),
            preview_image: None,
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
            _subscriptions: subs,
        };
        this.reload(cx);
        this.start_watch_loop(cx);
        this.watch_libraries();
        this.scan_all(cx);
        this
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
        cx.notify();
    }

    fn selected_asset(&self) -> Option<&Asset> {
        let id = self.selected?;
        self.assets.iter().find(|a| a.id == id)
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
        self.status = i18n::SCANNING.into();
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
                this.status = "".into();
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
            prompt: Some(i18n::ADD_FOLDER.into()),
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
        cx.spawn(async move |this, cx| loop {
            let pending = catalog
                .pending_thumbs(THUMB_CONCURRENCY)
                .unwrap_or_default();
            if pending.is_empty() {
                let stop = this
                    .update(cx, |this, cx| {
                        if this.thumbs_wanted {
                            this.thumbs_wanted = false;
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
                continue;
            }
            let n = pending.len() as u32;
            this.update(cx, |this, _cx| {
                this.thumbs_wanted = false;
            })
            .ok();
            let catalog2 = catalog.clone();
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                jobs::process_batch(&catalog2, &pending);
                let _ = done_tx.send(());
            });
            loop {
                match done_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {
                        cx.background_executor()
                            .timer(Duration::from_millis(50))
                            .await;
                    }
                }
            }
            this.update(cx, |this, cx| {
                this.thumb_done += n;
                this.thumb_total = this.thumb_total.max(this.thumb_done);
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
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
            self.status = i18n::LOADING.into();
        }
        let camera = self.camera;
        let selected = asset.id;
        cx.spawn(async move |this, cx| {
            let rendered = cx
                .background_spawn(async move {
                    let mesh = load_mesh(&asset.abs_path()).ok()?;
                    let img = render_mesh(&mesh, &camera, PREVIEW_SIZE, PREVIEW_SIZE);
                    Some(rgba_to_render_image(img))
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
                if let Some(image) = rendered {
                    if let Some(old) = this.preview_image.replace(image) {
                        cx.drop_image(old, None);
                    }
                    this.status = "".into();
                } else if this.preview_image.is_none() {
                    this.status = i18n::FAILED.into();
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
            self.last_preview_pos = None;
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

fn rgba_to_render_image(mut img: image::RgbaImage) -> Arc<RenderImage> {
    for px in img.pixels_mut() {
        px.0.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(img)]))
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &AddFolder, window, cx| this.add_folder(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSelected, _, cx| this.open_selected(cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                window.focus(&this.search.read(cx).focus_handle(cx), cx);
            }))
            .child(title_bar(cx))
            .child(toolbar(self, cx))
            .child(
                h_flex()
                    .id("body")
                    .flex_1()
                    .min_h_0()
                    .child(sidebar(self, cx))
                    .child(grid(self, window, cx))
                    .child(inspector(self, cx)),
            )
            .on_drop(cx.listener(|this, files: &ExternalPaths, _, cx| {
                for path in files.paths() {
                    this.add_folder_path(path, cx);
                }
            }))
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
                    .child(i18n::APP_TITLE),
            ),
    )
}

fn toolbar(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    h_flex()
        .w_full()
        .px_3()
        .py_2()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div().w(px(360.)).child(
                Input::new(&this.search)
                    .cleanable(true)
                    .prefix(Icon::new(IconName::Search).small()),
            ),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(if this.scanning {
                    if this.scan_total > 0 {
                        format!("掃描 {} / {}", this.scan_done, this.scan_total)
                    } else {
                        i18n::SCANNING.to_string()
                    }
                } else if this.thumbs_running && this.thumb_total > 0 {
                    format!("縮圖 {} / {}", this.thumb_done, this.thumb_total)
                } else {
                    format!("{} 個素材", this.assets.len())
                }),
        )
        .child(
            Button::new("add-folder")
                .primary()
                .label(i18n::ADD_FOLDER)
                .icon(IconName::Plus)
                .on_click(cx.listener(|this, _, window, cx| this.add_folder(window, cx))),
        )
}

fn sidebar(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    v_flex()
        .w(px(220.))
        .h_full()
        .min_h_0()
        .border_r_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .child(sidebar_section(i18n::FOLDERS))
        .child(nav_item(
            i18n::ALL_ASSETS,
            this.selected_library.is_none() && this.selected_tag.is_none(),
            cx.listener(|this, _, _, cx| {
                this.selected_library = None;
                this.selected_tag = None;
                this.reload(cx);
            }),
        ))
        .children(this.libraries.iter().map(|lib| {
            let id = lib.id;
            let label = lib
                .root_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("資料夾")
                .to_string();
            let online = lib.root_path.exists();
            let selected = this.selected_library == Some(id);
            nav_item(
                &format!(
                    "{}{}",
                    label,
                    if online {
                        String::new()
                    } else {
                        format!(" · {}", i18n::OFFLINE)
                    }
                ),
                selected,
                cx.listener(move |this, _, _, cx| {
                    this.selected_library = Some(id);
                    this.selected_tag = None;
                    this.reload(cx);
                }),
            )
        }))
        .child(sidebar_section(i18n::TAGS))
        .children(this.tags.iter().cloned().map(|(name, count)| {
            let selected = this.selected_tag.as_deref() == Some(name.as_str());
            let label = format!("{name}  {count}");
            let tag_name = name.clone();
            nav_item(
                &label,
                selected,
                cx.listener(move |this, _, _, cx| {
                    this.selected_tag = Some(tag_name.clone());
                    this.selected_library = None;
                    this.reload(cx);
                }),
            )
        }))
}

fn sidebar_section(title: &'static str) -> impl IntoElement {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .child(title)
}

fn nav_item(
    label: &str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(SharedString::from(format!("nav-{label}")))
        .px_3()
        .py_1()
        .cursor_pointer()
        .rounded_md()
        .mx_2()
        .when(selected, |el| el.bg(gpui_kit::hsla(0.62, 0.4, 0.32, 0.45)))
        .hover(|el| el.bg(gpui_kit::hsla(0.62, 0.2, 0.28, 0.35)))
        .on_click(on_click)
        .child(label.to_string())
}

fn grid_columns(window: &Window) -> usize {
    let usable = window.viewport_size().width.as_f32() - 220.0 - 300.0 - 24.0;
    ((usable / (CARD + 12.0)).floor() as usize).max(1)
}

fn grid(this: &Workspace, window: &mut Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .bg(theme.background)
        .child(if this.assets.is_empty() && this.libraries.is_empty() {
            empty_state(cx).into_any_element()
        } else if this.assets.is_empty() {
            div()
                .flex_1()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("此資料夾沒有 3MF / STL / OBJ")
                .into_any_element()
        } else {
            let cols = grid_columns(window);
            let rows = this.assets.len().div_ceil(cols);
            let assets = this.assets.clone();
            let selected = this.selected;
            let workspace = cx.entity().downgrade();
            uniform_list("asset-grid", rows, move |range, _window, cx| {
                range
                    .map(|row| {
                        h_flex()
                            .w_full()
                            .h(px(180.))
                            .gap_3()
                            .px_3()
                            .py_2()
                            .children((0..cols).filter_map(|col| {
                                let idx = row * cols + col;
                                assets.get(idx).map(|asset| {
                                    card(asset, selected == Some(asset.id), workspace.clone(), cx)
                                })
                            }))
                    })
                    .collect()
            })
            .size_full()
            .flex_1()
            .into_any_element()
        })
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
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child(i18n::EMPTY_TITLE),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_color(theme.muted_foreground)
                .child(i18n::EMPTY_BODY),
        )
        .child(
            Button::new("empty-add")
                .primary()
                .label(i18n::ADD_FOLDER)
                .on_click(cx.listener(|this, _, window, cx| this.add_folder(window, cx))),
        )
}

fn card(
    asset: &Asset,
    selected: bool,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let id = asset.id;
    let name = asset.name().to_string();
    let thumb = asset
        .sha256_hex()
        .map(|h| thumb_path(&h))
        .filter(|p| p.exists());
    v_flex()
        .id(ElementId::Integer(id as u64))
        .w(px(CARD))
        .h(px(168.))
        .gap_1()
        .p_1()
        .rounded_md()
        .cursor_pointer()
        .border_1()
        .border_color(if selected {
            theme.primary
        } else {
            theme.border
        })
        .bg(if selected {
            theme.secondary
        } else {
            theme.background
        })
        .on_click(move |event: &ClickEvent, window, cx| {
            let dbl = event.click_count() > 1;
            let _ = workspace.update(cx, |this, cx| {
                if dbl {
                    this.selected = Some(id);
                    this.open_selected(cx);
                } else {
                    this.select(id, window, cx);
                }
            });
        })
        .child(
            div()
                .w_full()
                .h(px(118.))
                .rounded_sm()
                .bg(theme.muted)
                .overflow_hidden()
                .child(match (&thumb, asset.thumb_state) {
                    (Some(path), ThumbState::Ready | ThumbState::Embedded) => {
                        img(path.to_string_lossy().to_string())
                            .size_full()
                            .object_fit(ObjectFit::Contain)
                            .into_any_element()
                    }
                    (_, ThumbState::Failed) => div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(i18n::FAILED)
                        .into_any_element(),
                    _ => div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(asset.format.label())
                        .into_any_element(),
                }),
        )
        .child(
            div()
                .text_xs()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(name),
        )
}

fn inspector(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    v_flex()
        .w(px(300.))
        .h_full()
        .min_h_0()
        .border_l_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .p_3()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(i18n::PREVIEW),
        )
        .child(preview_pane(this, cx))
        .child(if let Some(asset) = this.selected_asset() {
            asset_meta(this, asset, cx).into_any_element()
        } else {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(i18n::NO_SELECTION)
                .into_any_element()
        })
}

fn preview_pane(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    div()
        .id("preview")
        .w_full()
        .h(px(240.))
        .rounded_md()
        .bg(theme.muted)
        .overflow_hidden()
        .on_mouse_move(cx.listener(|this, event, _, cx| this.on_preview_drag(event, cx)))
        .on_scroll_wheel(cx.listener(|this, event, _, cx| this.on_preview_scroll(event, cx)))
        .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
            if event.click_count() > 1 {
                this.camera = Camera::default();
                this.render_preview(cx);
            }
        }))
        .child(if let Some(image) = &this.preview_image {
            img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(this.status.clone())
                .into_any_element()
        })
}

fn asset_meta(this: &Workspace, asset: &Asset, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    v_flex()
        .gap_2()
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(asset.name().to_string()),
        )
        .child(meta_row(i18n::FORMAT, asset.format.label(), &theme))
        .child(meta_row(
            i18n::SIZE,
            asset
                .bbox
                .as_ref()
                .map(|b| b.format_mm())
                .unwrap_or_else(|| "—".into()),
            &theme,
        ))
        .child(meta_row(
            i18n::TRIANGLES,
            asset
                .triangle_count
                .map(|n| format!("{n}"))
                .unwrap_or_else(|| "—".into()),
            &theme,
        ))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(i18n::DRAG_HINT),
        )
        .child(Input::new(&this.tag_input))
        .child(
            Button::new("open")
                .primary()
                .label(i18n::OPEN)
                .on_click(cx.listener(|this, _, _, cx| this.open_selected(cx))),
        )
}

fn meta_row(
    label: &str,
    value: impl Into<String>,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    h_flex()
        .justify_between()
        .text_sm()
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(label.to_string()),
        )
        .child(value.into())
}
