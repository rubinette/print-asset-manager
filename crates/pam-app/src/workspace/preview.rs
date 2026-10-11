//! Interactive 3D preview of the selected asset, rendered off the UI thread.

use std::sync::Arc;

use gpui_kit::component::ActiveTheme;
use gpui_kit::*;
use pam_core::catalog::Asset;
use pam_core::load::load_mesh;
use pam_core::mesh::Mesh;
use pam_core::paths::thumb_path;
use pam_preview::render_mesh;

use super::{UiStatus, Workspace};

const PREVIEW_SIZE: u32 = 640;

/// Triangle budget while the user is dragging the preview.
const DRAG_PREVIEW_TRIS: usize = 60_000;
/// Triangle budget for the still preview. Past this a 640 px frame shows no
/// more detail, while a 48M-triangle scan took 2 s per frame.
const STILL_PREVIEW_TRIS: usize = 2_000_000;

/// Parsed mesh for the selected asset, kept so orbit/zoom don't reparse the file.
pub(super) struct PreviewMesh {
    pub(super) asset_id: i64,
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

    pub(super) fn matches(&self, asset: &Asset) -> bool {
        self.asset_id == asset.id && self.mtime_ns == asset.mtime_ns
    }
}

impl Workspace {
    pub(super) fn render_preview(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn on_preview_drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
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

    pub(super) fn on_preview_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
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

pub(super) fn rgba_to_render_image(mut img: image::RgbaImage) -> Arc<RenderImage> {
    for px in img.pixels_mut() {
        px.0.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(img)]))
}

pub(super) fn load_thumb_render_image(asset: &Asset) -> Option<Arc<RenderImage>> {
    let hex = asset.sha256_hex()?;
    let path = thumb_path(&hex);
    let img = image::open(&path).ok()?.to_rgba8();
    Some(rgba_to_render_image(img))
}

#[cfg(test)]
mod tests {
    // gpui-kit's glob re-export shadows Rust's #[test]; use the prelude path.
    #[::core::prelude::v1::test]
    fn hsla_to_rgba_converts_black_and_white() {
        assert_eq!(super::hsla_to_rgba(gpui_kit::black()), [0, 0, 0, 255]);
        assert_eq!(super::hsla_to_rgba(gpui_kit::white()), [255, 255, 255, 255]);
    }
}
