//! Photos, README and other documents next to the selected model, shown at
//! the bottom of the inspector. Click one to open it in its default app.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Icon, Sizable as _};
use gpui_kit::{prelude::FluentBuilder as _, *};
use pam_core::open_path;
use pam_core::sidecar::{find_sidecars, read_text_preview};

use crate::i18n::{self, Key};

use super::preview::rgba_to_render_image;
use super::Workspace;

/// Thumbnail edge on screen, and the decoded size (2× for HiDPI).
const THUMB_SIZE: f32 = 72.;
const THUMB_PIXELS: u32 = 144;
/// Photos bigger than this aren't decoded.
const MAX_IMAGE_BYTES: u64 = 40 * 1024 * 1024;
const README_PREVIEW_BYTES: usize = 2048;
const README_PREVIEW_LINES: usize = 8;

#[derive(Default)]
pub(super) struct Nearby {
    /// The asset these belong to (or are being gathered for).
    pub(super) asset_id: Option<i64>,
    generation: u64,
    images: Vec<NearbyImage>,
    docs: Vec<PathBuf>,
    readme: Option<String>,
}

struct NearbyImage {
    path: PathBuf,
    /// `None` while it's still being decoded.
    image: Option<Arc<RenderImage>>,
}

impl Nearby {
    fn is_empty(&self) -> bool {
        self.images.is_empty() && self.docs.is_empty()
    }
}

/// A small RGBA thumbnail of an image file, or `None` if it can't be read.
fn decode_thumbnail(path: &Path) -> Option<Arc<RenderImage>> {
    if std::fs::metadata(path).ok()?.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    Some(rgba_to_render_image(
        image.thumbnail(THUMB_PIXELS, THUMB_PIXELS).to_rgba8(),
    ))
}

impl Workspace {
    /// Gather the selected asset's images and documents off the UI thread,
    /// then decode the images one by one so the first shows up quickly.
    pub(super) fn load_nearby(&mut self, cx: &mut Context<Self>) {
        if self.nearby.asset_id == self.selected {
            return;
        }
        let generation = self.nearby.generation + 1;
        for image in self.nearby.images.drain(..).filter_map(|i| i.image) {
            cx.drop_image(image, None);
        }
        self.nearby = Nearby {
            asset_id: self.selected,
            generation,
            ..Default::default()
        };
        let Some(model) = self.selected_asset().map(|a| a.abs_path()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let (found, readme) = cx
                .background_spawn(async move {
                    let found = find_sidecars(&model);
                    let readme = found
                        .readme
                        .as_deref()
                        .and_then(|p| read_text_preview(p, README_PREVIEW_BYTES));
                    (found, readme)
                })
                .await;
            let images = found.images.clone();
            let current = this
                .update(cx, |this, cx| {
                    if this.nearby.generation != generation {
                        return false;
                    }
                    this.nearby.images = found
                        .images
                        .into_iter()
                        .map(|path| NearbyImage { path, image: None })
                        .collect();
                    this.nearby.docs = found.docs;
                    this.nearby.readme = readme;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            for path in images {
                let decoded = cx
                    .background_spawn({
                        let path = path.clone();
                        async move { decode_thumbnail(&path) }
                    })
                    .await;
                let current = this
                    .update(cx, |this, cx| {
                        let slot = (this.nearby.generation == generation)
                            .then(|| this.nearby.images.iter().position(|i| i.path == path))
                            .flatten();
                        match (slot, decoded) {
                            (Some(ix), Some(image)) => this.nearby.images[ix].image = Some(image),
                            (Some(ix), None) => {
                                this.nearby.images.remove(ix);
                            }
                            (None, decoded) => {
                                if let Some(image) = decoded {
                                    cx.drop_image(image, None);
                                }
                                return false;
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !current {
                    break;
                }
            }
        })
        .detach();
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn open_on_click(path: &Path) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let path = path.to_path_buf();
    move |_, _, _| {
        if let Err(err) = open_path(&path) {
            eprintln!("open {} failed: {err}", path.display());
        }
    }
}

fn name_tooltip(path: &Path) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let name = file_name(path);
    move |window, cx| Tooltip::new(name.clone()).build(window, cx)
}

/// The inspector section, or nothing when the folder has none.
pub(super) fn nearby_section(this: &Workspace, cx: &mut Context<Workspace>) -> Option<AnyElement> {
    let nearby = &this.nearby;
    if nearby.asset_id != this.selected || nearby.is_empty() {
        return None;
    }
    let theme = cx.theme().clone();
    let images = nearby.images.iter().enumerate().map(|(ix, entry)| {
        div()
            .id(("nearby-image", ix))
            .size(px(THUMB_SIZE))
            .flex_shrink_0()
            .rounded_md()
            .overflow_hidden()
            .bg(theme.muted)
            .cursor_pointer()
            .tooltip(name_tooltip(&entry.path))
            .on_click(open_on_click(&entry.path))
            .child(match &entry.image {
                Some(image) => img(image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    .into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(Lucide::Image).small())
                    .into_any_element(),
            })
    });
    let docs = nearby.docs.iter().enumerate().map(|(ix, path)| {
        h_flex()
            .id(("nearby-doc", ix))
            .gap_2()
            .px_1()
            .py_0p5()
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .hover(|el| el.bg(theme.secondary))
            .on_click(open_on_click(path))
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(Lucide::FileText).small()),
            )
            .child(div().min_w_0().truncate().child(file_name(path)))
    });
    Some(
        v_flex()
            .gap_1p5()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child(i18n::t(Key::NearbyFiles)),
            )
            .when(!nearby.images.is_empty(), |el| {
                el.child(h_flex().flex_wrap().gap_1p5().children(images))
            })
            .when(!nearby.docs.is_empty(), |el| {
                el.child(v_flex().children(docs))
            })
            .when_some(nearby.readme.clone(), |el, text| {
                el.child(
                    div()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .bg(theme.secondary)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .line_clamp(README_PREVIEW_LINES)
                        .child(text),
                )
            })
            .into_any_element(),
    )
}
