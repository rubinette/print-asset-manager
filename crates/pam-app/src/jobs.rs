use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use pam_core::catalog::{Asset, Catalog, ThumbState};
use pam_core::load::{extract_3mf_thumbnail, load_mesh};
use pam_core::mesh::AssetFormat;
use pam_core::paths::thumb_path;
use pam_preview::{encode_png, render_thumbnail};

pub const THUMB_CONCURRENCY: usize = 2;
const THUMB_BATCH: usize = THUMB_CONCURRENCY * 8;
const THUMB_SIZE: u32 = 256;
/// Bump when rendered thumbnails would come out differently (renderer or
/// loader changes); the app then redraws them once at startup.
/// 2: full-mesh render, near-plane clipping, 3MF modifiers hidden.
pub const THUMB_RENDER_VERSION: &str = "2";

pub fn process_thumb(catalog: &Catalog, asset: &Asset) {
    // Library went offline mid-batch: leave it pending rather than `failed`,
    // which a later scan of the unchanged file would never clear.
    if !asset.root_path.is_dir() {
        return;
    }
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

/// Process `batch` on `THUMB_CONCURRENCY` workers that each pull the next
/// asset as soon as they finish, bumping `done` per asset.
pub fn process_batch(catalog: &Catalog, batch: &[Asset], done: &AtomicU32) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..THUMB_CONCURRENCY.min(batch.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(asset) = batch.get(i) else {
                    break;
                };
                process_thumb(catalog, asset);
                done.fetch_add(1, Ordering::Relaxed);
            });
        }
    });
}

/// Generate thumbnails until no asset is pending.
pub fn drain_pending(catalog: &Catalog, done: &AtomicU32) {
    let mut last_ids = Vec::new();
    loop {
        let pending = catalog.pending_thumbs(THUMB_BATCH).unwrap_or_default();
        let ids: Vec<i64> = pending.iter().map(|a| a.id).collect();
        // Same batch again means state writes are failing; stop instead of spinning.
        if pending.is_empty() || ids == last_ids {
            break;
        }
        process_batch(catalog, &pending, done);
        last_ids = ids;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pam_core::catalog::AssetQuery;
    use pam_core::load::write_3mf_cube;
    use std::fs;
    use std::path::Path;

    fn load_first(catalog: &Catalog) -> Asset {
        catalog
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    fn cleanup(asset: &Asset) {
        if let Some(hex) = asset.sha256_hex() {
            let _ = fs::remove_file(thumb_path(&hex));
        }
    }

    #[test]
    fn stl_thumb_becomes_ready() {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/cube.stl");
        fs::copy(&src, dir.path().join("cube.stl")).unwrap();
        let catalog = Catalog::open_memory().unwrap();
        let lib = catalog.add_library(dir.path()).unwrap();
        catalog.scan_library(&lib).unwrap();
        let asset = load_first(&catalog);
        process_thumb(&catalog, &asset);
        let done = load_first(&catalog);
        assert_eq!(done.thumb_state, ThumbState::Ready);
        assert!(done.triangle_count.unwrap() >= 12);
        let hex = done.sha256_hex().unwrap();
        assert!(thumb_path(&hex).exists());
        cleanup(&done);
    }

    #[test]
    fn bad_stl_marks_failed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bad.stl"), b"not an stl").unwrap();
        let catalog = Catalog::open_memory().unwrap();
        let lib = catalog.add_library(dir.path()).unwrap();
        catalog.scan_library(&lib).unwrap();
        let asset = load_first(&catalog);
        process_thumb(&catalog, &asset);
        let done = load_first(&catalog);
        assert_eq!(done.thumb_state, ThumbState::Failed);
        assert!(done.error.is_some());
        cleanup(&done);
    }

    #[test]
    fn offline_library_stays_pending() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        let catalog = Catalog::open_memory().unwrap();
        let lib = catalog.add_library(dir.path()).unwrap();
        catalog.scan_library(&lib).unwrap();
        let asset = load_first(&catalog);
        dir.close().unwrap();
        process_thumb(&catalog, &asset);
        let after = load_first(&catalog);
        assert_eq!(after.thumb_state, ThumbState::Pending);
        assert!(after.error.is_none());
    }

    #[test]
    fn threemf_embedded_thumb() {
        let dir = tempfile::tempdir().unwrap();
        write_3mf_cube(&dir.path().join("cube.3mf"), true).unwrap();
        let catalog = Catalog::open_memory().unwrap();
        let lib = catalog.add_library(dir.path()).unwrap();
        catalog.scan_library(&lib).unwrap();
        let done = AtomicU32::new(0);
        drain_pending(&catalog, &done);
        assert_eq!(done.load(Ordering::Relaxed), 1);
        let done = load_first(&catalog);
        assert_eq!(done.thumb_state, ThumbState::Embedded);
        let hex = done.sha256_hex().unwrap();
        assert!(thumb_path(&hex).exists());
        cleanup(&done);
    }
}
