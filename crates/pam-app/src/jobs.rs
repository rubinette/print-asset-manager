use pam_core::catalog::{Asset, Catalog, ThumbState};
use pam_core::load::{extract_3mf_thumbnail, load_mesh};
use pam_core::mesh::AssetFormat;
use pam_core::paths::thumb_path;
use pam_preview::{encode_png, render_thumbnail};

pub const THUMB_CONCURRENCY: usize = 2;
const THUMB_SIZE: u32 = 256;

pub fn process_thumb(catalog: &Catalog, asset: &Asset) {
    let path = asset.abs_path();
    let sha = match catalog.hash_asset(asset) {
        Ok(s) => s,
        Err(err) => {
            let _ = catalog.set_thumb_state(asset.id, ThumbState::Failed, Some(&err.to_string()));
            return;
        }
    };
    let hex = pam_core::paths::hex_sha256(&sha);
    let dest = thumb_path(&hex);

    if asset.format == AssetFormat::ThreeMf {
        if let Ok(Some(png)) = extract_3mf_thumbnail(&path) {
            if std::fs::write(&dest, &png).is_ok() {
                if let Ok(mesh) = load_mesh(&path) {
                    let _ = catalog.set_mesh_meta(
                        asset.id,
                        &sha,
                        mesh.triangle_count() as i64,
                        &mesh.bbox,
                    );
                }
                let _ = catalog.set_thumb_state(asset.id, ThumbState::Embedded, None);
                return;
            }
        }
    }

    match load_mesh(&path) {
        Ok(mesh) => {
            let _ = catalog.set_mesh_meta(asset.id, &sha, mesh.triangle_count() as i64, &mesh.bbox);
            let img = render_thumbnail(&mesh, THUMB_SIZE);
            let bytes = encode_png(&img);
            if std::fs::write(&dest, bytes).is_ok() {
                let _ = catalog.set_thumb_state(asset.id, ThumbState::Ready, None);
            } else {
                let _ = catalog.set_thumb_state(asset.id, ThumbState::Failed, Some("write thumb"));
            }
        }
        Err(err) => {
            let _ = catalog.set_thumb_state(asset.id, ThumbState::Failed, Some(&err.to_string()));
        }
    }
}

pub fn process_batch(catalog: &Catalog, batch: &[Asset]) {
    std::thread::scope(|scope| {
        for asset in batch {
            scope.spawn(|| process_thumb(catalog, asset));
        }
    });
}
