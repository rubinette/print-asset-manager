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

MSRV is Rust 1.95, the oldest the locked GPUI crates compile with; CI's `msrv` job checks it, so bump it there and in `Cargo.toml` together. Linux builds need `libxkbcommon`, `fontconfig`, Vulkan/OpenGL drivers, and Wayland or X11 dev packages; macOS needs Xcode CLT (Metal).

## Architecture

Three crates, layered strictly `pam-app → pam-preview → pam-core`:

- **`pam-core`** — no UI. `Catalog` wraps a single `rusqlite::Connection` behind a `Mutex` (shared as `Arc<Catalog>` across threads). Schema lives in the `SCHEMA` const in `catalog.rs` and is applied with `CREATE ... IF NOT EXISTS` on every open — there is no migration system, so schema changes to existing tables need care. `load.rs` parses STL/OBJ/3MF into a triangle-soup `Mesh`; 3MF loading follows build items and Production-extension components across parts and skips `type="other"`/`support`/`solidsupport` objects (Bambu/Orca modifiers, negative parts); `extract_3mf_thumbnail` pulls slicer-embedded PNGs (e.g. `Metadata/plate_1.png`). `sidecar.rs` finds the photos and documents next to a model (its folder, image subfolders, and the folder above a `files`-style folder; a folder with many models only contributes files named after the model). `watch.rs` runs a `notify` watcher on its own thread with a command channel; `affected_library` maps an event path to the library with the longest matching root. `paths.rs` owns all on-disk locations (via `directories::ProjectDirs`).
- **`pam-preview`** — pure CPU software rasterizer (no GPU). `render_thumbnail` / `render_mesh` with an orbit `Camera`, output `image::RgbaImage`, `encode_png`. It draws every triangle it is given (near-plane clipped, 1/depth z-buffer); callers pass a `Mesh::simplified` LOD when they need speed (`PreviewMesh`: 2M still / 60k drag). `simplified` uses vertex clustering — never go back to keeping every Nth triangle, which renders as see-through confetti. Bump `jobs::THUMB_RENDER_VERSION` when rendered thumbnails would change; startup then requeues `ready`/`failed` thumbs once.
- **`pam-app`** — GPUI UI. `main.rs` sets up actions, keybindings, menus, window. `workspace.rs` is the single root view holding all UI state (the `Workspace` struct, `reload`/`query`, and the root layout); its submodules in `workspace/` each add an `impl Workspace` block plus the render functions for one area: `toolbar`, `filters`, `sidebar`, `asset_view` (grid/list, cards, context menu), `selection`, `inspector`, `preview`, `library` (add/scan/watch folders, thumbnail queue), `removal` (trash, remove folder), `print_status` (status buttons, debounced note saving), `saved` (saved searches), `nearby` (sidecar images/docs in the inspector, decoded off-thread one at a time), `format`. Struct fields stay private to `workspace` and are visible to its submodules; items a sibling module needs are `pub(super)`. `jobs.rs` is the thumbnail pipeline; `i18n.rs` holds all strings. Destructive actions go through `confirm_destructive` (in-app gpui-kit dialog where Enter and Escape both cancel), not native `window.prompt`, whose NSAlert makes the first button the Return default; dialogs only paint because `Workspace::render` includes `Root::render_dialog_layer`. GPUI ranks a key binding with no context as the deepest match, so an unscoped single-key binding (Enter, arrows, Delete) beats every text field's own; scope those to `ASSET_VIEW_CONTEXT`.

### Data flow

1. **Scan** (`Catalog::scan_library_progress`): walk library root, upsert assets keyed by `(library_id, rel_path)`. Unchanged `size_bytes` + `mtime_ns` → skipped; changed → reset to `thumb_state='pending'` and clear `content_sha256`. Files no longer present are deleted, except renames/moves inside the library: `plan_moves` pairs a vanished file with a new one (unique size+mtime+format match, else content hash) and `relink_moved` points the old row at the new path, so everything keyed by asset id (tags etc.) stays. Other vanished files with tags or an `asset_notes` row (print status + note) are stashed by content hash in `detached_meta` (also on `remove_library`), and `hash_asset` gives them back to the next asset hashed with that content (or, when the scan that stashes them finds an already-hashed copy without user data, e.g. a cross-volume move whose destination was hashed first, that copy gets them right away); any new per-asset user data must be added to `stash_user_data` / `reattach_user_data`. If the root doesn't exist (e.g. offline NAS), the index is left untouched — don't "fix" this into deleting assets.
2. **Hashes and thumbnails** (`jobs::drain`, after every scan) run side by side. The hashing pass (`hash_unhashed`, paging `unhashed_assets` by id) stores SHA-256 for every asset without one, so the duplicates view is complete long before thumbnails are. A third pass (`read_print_info`, paging `assets_without_print_info`) reads 3MF slicer metadata with `print_info::read_3mf_print_info` into the `print_info` table: one row per 3MF even when empty, so it isn't re-read; a changed file's row is deleted by the scan, and bumping `PRINT_INFO_VERSION` re-reads all of them. Thumbnails (`process_thumb`, `THUMB_CONCURRENCY` at a time from `pending_thumbs`): get the hash (the pass's in-memory `FreshHashes` entry if the file's size and mtime are unchanged, else hash the file; never reuse a hash stored by an earlier run, since a stale one would overwrite another file's shared thumbnail) → for 3MF try embedded PNG first (`ThumbState::Embedded`), else load mesh and CPU-render (`Ready`); failures set `Failed` + `error`. `hash_asset` stores only while the row's size and mtime match the asset it was given, so a hash of an old version can't land after a rescan. Offline libraries (root missing) are excluded from `pending_thumbs` / `pending_count` / `unhashed_assets` and `process_thumb` returns early for them — never mark an unreadable-because-offline file `Failed`, since a rescan of the unchanged file won't clear it. Thumbnails are content-addressed: `<cache>/thumbs/<sha256hex>.png`; the OS may purge that dir, so each scan calls `requeue_missing_thumbs`.
3. **Search**: `assets_fts` is a *contentless* FTS5 table (`content=''`, `contentless_delete=1`) keyed by `assets.id` rowid. It is **not** maintained by triggers — code that inserts/deletes assets or changes tags (`upsert_asset`, stale removal in scan, `set_tags`, `remove_asset`, `remove_library` — the FK cascade does not reach FTS) must manually delete/insert the FTS row. Keep this in sync when adding new write paths.
4. **UI concurrency**: `Workspace` offloads scans, thumbnail batches, and preview renders via `std::thread::spawn` / `background_spawn`, and polls/awaits with `cx.spawn` + `background_executor().timer(...)`, then updates `this` and calls `cx.notify()`. The preview is re-rendered on camera drag/scroll and on system appearance change (background color follows theme).

### Persistence

- Catalog DB: `paths::catalog_db_path()` (`data_dir()/catalog.sqlite`, WAL mode).
- Small prefs are plain-text files in `data_dir()`: `locale` (language pref) and `sort` (`AssetSort::as_str`).
- Saved searches: `saved_searches` table, `query` is `SavedQuery` JSON (folder, search text, `Filters::to_json`). Parsing is tolerant, so an unknown or removed filter value is dropped instead of losing the whole entry; keep it that way when filters change.
- Thumbnails and `preview.png` in `cache_dir()`.

### i18n

Every user-facing string is a `Key` variant; `lookup` in `i18n.rs` is an exhaustive `match (Language, Key)`, so adding a key requires both Chinese and English arms (the compiler enforces it). Language pref is System / Chinese / English; `apply_kit_locale` also sets gpui-kit's component locale (`en` / `zh-TW`). Changing language reinstalls menus and refreshes windows.

## Testing notes

- Test fixtures are generated in temp dirs by the tests themselves (`write_3mf_cube`, inline ASCII STL/OBJ); `testdata/cube.stl` is the only checked-in mesh. Don't commit large binaries.
- Use `Catalog::open_memory()` in tests. Note `thumb_path` writes into the **real** user cache dir, so thumbnail tests clean up after themselves (`cleanup` in `jobs.rs`) — follow that pattern.
- In `pam-app`, `use gpui_kit::*` shadows the `#[test]` attribute. Use `#[::core::prelude::v1::test]` for plain tests in modules that glob-import gpui-kit, and `#[gpui_kit::test]` with `TestAppContext` for UI/layout tests (`gpui-kit` `test-support` feature is enabled in dev-deps).
