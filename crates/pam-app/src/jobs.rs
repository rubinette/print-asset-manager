use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

use pam_core::catalog::{Asset, Catalog, ThumbState};
use pam_core::load::{extract_3mf_thumbnail, load_mesh};
use pam_core::mesh::AssetFormat;
use pam_core::paths::thumb_path;
use pam_preview::{encode_png, render_thumbnail};

pub const THUMB_CONCURRENCY: usize = 2;
const THUMB_BATCH: usize = THUMB_CONCURRENCY * 8;
const THUMB_SIZE: u32 = 256;
/// Assets fetched per query by the hashing pass.
const HASH_BATCH: usize = 64;
/// Bump when rendered thumbnails would come out differently (renderer or
/// loader changes); the app then redraws them once at startup.
/// 2: full-mesh render, near-plane clipping, 3MF modifiers hidden.
pub const THUMB_RENDER_VERSION: &str = "2";

/// Hashes the hashing pass computed during one [`drain`], so the thumbnail
/// job for the same file doesn't read it a second time. Hashes stored by
/// earlier runs are not reused this way: thumbnails are named by hash and
/// shared by identical files, so a stale hash would put this file's
/// thumbnail over another file's.
#[derive(Default)]
pub struct FreshHashes(Mutex<HashMap<RowKey, Vec<u8>>>);

/// An asset's id, size and mtime. A scan that sees the file change gives its
/// row a new size or mtime, so a hash of the old file is never handed out.
type RowKey = (i64, u64, i64);

impl FreshHashes {
    fn key(asset: &Asset) -> RowKey {
        (asset.id, asset.size_bytes, asset.mtime_ns)
    }

    fn insert(&self, asset: &Asset, sha: Vec<u8>) {
        self.0.lock().unwrap().insert(Self::key(asset), sha);
    }

    /// The hash computed for `asset`, if neither its row nor its file has
    /// changed since.
    fn take(&self, asset: &Asset) -> Option<Vec<u8>> {
        let sha = self.0.lock().unwrap().remove(&Self::key(asset))?;
        asset.unchanged_on_disk().then_some(sha)
    }
}

pub fn process_thumb(catalog: &Catalog, asset: &Asset, fresh: &FreshHashes) {
    // Library went offline mid-batch: leave it pending rather than `failed`,
    // which a later scan of the unchanged file would never clear.
    if !asset.root_path.is_dir() {
        return;
    }
    let path = asset.abs_path();
    let sha = match fresh
        .take(asset)
        .map_or_else(|| catalog.hash_asset(asset), Ok)
    {
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
pub fn process_batch(catalog: &Catalog, batch: &[Asset], fresh: &FreshHashes, done: &AtomicU32) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..THUMB_CONCURRENCY.min(batch.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(asset) = batch.get(i) else {
                    break;
                };
                process_thumb(catalog, asset, fresh);
                done.fetch_add(1, Ordering::Relaxed);
            });
        }
    });
}

/// Generate thumbnails until no asset is pending.
fn drain_pending(catalog: &Catalog, fresh: &FreshHashes, done: &AtomicU32) {
    let mut last_ids = Vec::new();
    loop {
        let pending = catalog.pending_thumbs(THUMB_BATCH).unwrap_or_default();
        let ids: Vec<i64> = pending.iter().map(|a| a.id).collect();
        // Same batch again means state writes are failing; stop instead of spinning.
        if pending.is_empty() || ids == last_ids {
            break;
        }
        process_batch(catalog, &pending, fresh, done);
        last_ids = ids;
    }
}

/// Hash every file that has no content hash yet, bumping `hashed` per file.
/// Hashing is much faster than rendering, so running this ahead of the
/// thumbnails lets the duplicates view fill in long before they finish.
fn hash_unhashed(catalog: &Catalog, fresh: &FreshHashes, hashed: &AtomicU32) {
    let mut after = 0;
    loop {
        let batch = catalog
            .unhashed_assets(after, HASH_BATCH)
            .unwrap_or_default();
        let Some(last) = batch.last() else {
            break;
        };
        after = last.id;
        for asset in &batch {
            // A file that can't be read is left for its thumbnail job to report.
            if let Ok(sha) = catalog.hash_asset(asset) {
                fresh.insert(asset, sha);
                hashed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Hash files and generate thumbnails, side by side, until neither has work
/// left. Bumps `hashed` per file hashed and `done` per thumbnail.
pub fn drain(catalog: &Catalog, hashed: &AtomicU32, done: &AtomicU32) {
    let fresh = FreshHashes::default();
    std::thread::scope(|scope| {
        scope.spawn(|| hash_unhashed(catalog, &fresh, hashed));
        drain_pending(catalog, &fresh, done);
    });
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
        process_thumb(&catalog, &asset, &FreshHashes::default());
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
        process_thumb(&catalog, &asset, &FreshHashes::default());
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
        process_thumb(&catalog, &asset, &FreshHashes::default());
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
        drain(&catalog, &AtomicU32::new(0), &done);
        assert_eq!(done.load(Ordering::Relaxed), 1);
        let done = load_first(&catalog);
        assert_eq!(done.thumb_state, ThumbState::Embedded);
        let hex = done.sha256_hex().unwrap();
        assert!(thumb_path(&hex).exists());
        cleanup(&done);
    }

    #[test]
    fn hashing_pass_finds_duplicates_before_thumbnails() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        fs::write(dir.path().join("b.stl"), b"solid x\nendsolid x\n").unwrap();
        fs::write(dir.path().join("c.stl"), b"solid y\nendsolid y\n").unwrap();
        let catalog = Catalog::open_memory().unwrap();
        let lib = catalog.add_library(dir.path()).unwrap();
        catalog.scan_library(&lib).unwrap();
        let hashed = AtomicU32::new(0);
        hash_unhashed(&catalog, &FreshHashes::default(), &hashed);
        assert_eq!(hashed.load(Ordering::Relaxed), 3);
        assert_eq!(catalog.duplicate_count().unwrap(), 2);
        assert_eq!(catalog.pending_count().unwrap(), 3, "no thumbnails yet");
    }

    #[test]
    fn thumb_reuses_fresh_hash_only_while_file_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/cube.stl");
        let path = dir.path().join("cube.stl");
        fs::copy(&src, &path).unwrap();
        let catalog = Catalog::open_memory().unwrap();
        let lib = catalog.add_library(dir.path()).unwrap();
        catalog.scan_library(&lib).unwrap();
        let asset = load_first(&catalog);
        // A made-up hash, so the stored hash shows which one the job used.
        let made_up = vec![0xab; 32];
        let fresh = FreshHashes::default();

        fresh.insert(&asset, made_up.clone());
        process_thumb(&catalog, &asset, &fresh);
        let reused = load_first(&catalog);
        cleanup(&reused);
        assert_eq!(reused.content_sha256, Some(made_up.clone()));
        assert_eq!(reused.thumb_state, ThumbState::Ready);

        fresh.insert(&asset, made_up.clone());
        fs::write(&path, b"not an stl").unwrap();
        process_thumb(&catalog, &asset, &fresh);
        let rehashed = load_first(&catalog);
        cleanup(&rehashed);
        assert_eq!(
            rehashed.content_sha256.as_deref(),
            Some(&pam_core::hash::file_sha256(&path).unwrap()[..])
        );
        assert_eq!(rehashed.thumb_state, ThumbState::Failed);
    }
}
