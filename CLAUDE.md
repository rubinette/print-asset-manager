# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

列印素材庫 (Print Asset Library): a native Rust + GPUI (via `gpui-kit`) desktop app that indexes 3MF / STL / OBJ files **in place** (originals are never moved or copied — the sole exception is the user-confirmed "Move to Trash" action, via `open::move_to_trash`), generates thumbnails, supports search/tags/sort, and shows an interactive rotatable preview. Targets macOS and Linux (Wayland/X11); Windows is out of scope for v1, as are STEP/IGES, slicing, and printer dispatch. README and UI strings are primarily Traditional Chinese.

## Commands

```bash
cargo run -p pam-app --release          # run the app (binary: print-asset-manager)
cargo build                             # default-members = pam-app only
cargo test --workspace                  # all tests (plain `cargo test` only tests pam-app)
cargo test -p pam-core                  # one crate
cargo test -p pam-core catalog::tests::  # one module; append a test name for a single test
cargo clippy --workspace --all-targets
cargo fmt --all
```

MSRV is Rust 1.85. Linux builds need `libxkbcommon`, Vulkan/OpenGL drivers, and Wayland or X11 dev packages; macOS needs Xcode CLT (Metal).

## Architecture

Three crates, layered strictly `pam-app → pam-preview → pam-core`:

- **`pam-core`** — no UI. `Catalog` wraps a single `rusqlite::Connection` behind a `Mutex` (shared as `Arc<Catalog>` across threads). Schema lives in the `SCHEMA` const in `catalog.rs` and is applied with `CREATE ... IF NOT EXISTS` on every open — there is no migration system, so schema changes to existing tables need care. `load.rs` parses STL/OBJ/3MF into a triangle-soup `Mesh`; `extract_3mf_thumbnail` pulls slicer-embedded PNGs (e.g. `Metadata/plate_1.png`). `watch.rs` runs a `notify` watcher on its own thread with a command channel; `affected_library` maps an event path to the library with the longest matching root. `paths.rs` owns all on-disk locations (via `directories::ProjectDirs`).
- **`pam-preview`** — pure CPU software rasterizer (no GPU). `render_thumbnail` / `render_mesh` with an orbit `Camera`, output `image::RgbaImage`, `encode_png`.
- **`pam-app`** — GPUI UI. `main.rs` sets up actions, keybindings, menus, window. `workspace.rs` is the single root view holding all UI state; `jobs.rs` is the thumbnail pipeline; `i18n.rs` holds all strings. Destructive actions go through `confirm_destructive` (in-app gpui-kit dialog where Enter and Escape both cancel), not native `window.prompt`, whose NSAlert makes the first button the Return default; dialogs only paint because `Workspace::render` includes `Root::render_dialog_layer`.

### Data flow

1. **Scan** (`Catalog::scan_library_progress`): walk library root, upsert assets keyed by `(library_id, rel_path)`. Unchanged `size_bytes` + `mtime_ns` → skipped; changed → reset to `thumb_state='pending'` and clear `content_sha256`. Files no longer present are deleted. If the root doesn't exist (e.g. offline NAS), the index is left untouched — don't "fix" this into deleting assets.
2. **Thumbnails** (`jobs::process_thumb`, `THUMB_CONCURRENCY` at a time from `pending_thumbs`): hash file (SHA-256) → for 3MF try embedded PNG first (`ThumbState::Embedded`), else load mesh and CPU-render (`Ready`); failures set `Failed` + `error`. Offline libraries (root missing) are excluded from `pending_thumbs` / `pending_count` and `process_thumb` returns early for them — never mark an unreadable-because-offline file `Failed`, since a rescan of the unchanged file won't clear it. Thumbnails are content-addressed: `<cache>/thumbs/<sha256hex>.png`; the OS may purge that dir, so each scan calls `requeue_missing_thumbs`.
3. **Search**: `assets_fts` is a *contentless* FTS5 table (`content=''`, `contentless_delete=1`) keyed by `assets.id` rowid. It is **not** maintained by triggers — code that inserts/deletes assets or changes tags (`upsert_asset`, stale removal in scan, `set_tags`, `remove_asset`, `remove_library` — the FK cascade does not reach FTS) must manually delete/insert the FTS row. Keep this in sync when adding new write paths.
4. **UI concurrency**: `Workspace` offloads scans, thumbnail batches, and preview renders via `std::thread::spawn` / `background_spawn`, and polls/awaits with `cx.spawn` + `background_executor().timer(...)`, then updates `this` and calls `cx.notify()`. The preview is re-rendered on camera drag/scroll and on system appearance change (background color follows theme).

### Persistence

- Catalog DB: `paths::catalog_db_path()` (`data_dir()/catalog.sqlite`, WAL mode).
- Small prefs are plain-text files in `data_dir()`: `locale` (language pref) and `sort` (`AssetSort::as_str`).
- Thumbnails and `preview.png` in `cache_dir()`.

### i18n

Every user-facing string is a `Key` variant; `lookup` in `i18n.rs` is an exhaustive `match (Language, Key)`, so adding a key requires both Chinese and English arms (the compiler enforces it). Language pref is System / Chinese / English; `apply_kit_locale` also sets gpui-kit's component locale (`en` / `zh-TW`). Changing language reinstalls menus and refreshes windows.

## Testing notes

- Test fixtures are generated in temp dirs by the tests themselves (`write_3mf_cube`, inline ASCII STL/OBJ); `testdata/cube.stl` is the only checked-in mesh. Don't commit large binaries.
- Use `Catalog::open_memory()` in tests. Note `thumb_path` writes into the **real** user cache dir, so thumbnail tests clean up after themselves (`cleanup` in `jobs.rs`) — follow that pattern.
- In `pam-app`, `use gpui_kit::*` shadows the `#[test]` attribute. Use `#[::core::prelude::v1::test]` for plain tests in modules that glob-import gpui-kit, and `#[gpui_kit::test]` with `TestAppContext` for UI/layout tests (`gpui-kit` `test-support` feature is enabled in dev-deps).
