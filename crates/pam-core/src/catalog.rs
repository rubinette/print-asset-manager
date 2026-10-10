use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use walkdir::WalkDir;

use crate::error::Result;
use crate::hash::file_sha256;
use crate::mesh::{AssetFormat, BBox};
use crate::print_info::{PrintInfo, PRINT_INFO_VERSION};

/// Files written per scan transaction.
const SCAN_CHUNK: usize = 500;
/// Most new files hashed to find where one vanished file went.
const MOVE_HASH_CANDIDATES: usize = 8;
/// Days a vanished file's tags wait in `detached_meta` for it to reappear.
const DETACHED_KEEP_DAYS: i64 = 90;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS libraries (
  id INTEGER PRIMARY KEY,
  root_path TEXT NOT NULL UNIQUE,
  added_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS assets (
  id INTEGER PRIMARY KEY,
  library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
  rel_path TEXT NOT NULL,
  format TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  content_sha256 BLOB,
  triangle_count INTEGER,
  bbox_min_x REAL, bbox_min_y REAL, bbox_min_z REAL,
  bbox_max_x REAL, bbox_max_y REAL, bbox_max_z REAL,
  thumb_state TEXT NOT NULL DEFAULT 'pending',
  error TEXT,
  UNIQUE(library_id, rel_path)
);

CREATE TABLE IF NOT EXISTS tags (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS asset_tags (
  asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
  tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  PRIMARY KEY (asset_id, tag_id)
);

-- Print status and note per asset. Its own table rather than assets columns:
-- there are no migrations, and CREATE TABLE IF NOT EXISTS reaches catalogs
-- made before it existed. A row is dropped once both are cleared.
CREATE TABLE IF NOT EXISTS asset_notes (
  asset_id INTEGER PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
  status TEXT,
  status_at INTEGER,
  note TEXT NOT NULL DEFAULT ''
);

-- Slicer metadata read from 3MF files (`print_info.rs`). Every 3MF gets a
-- row once read, empty or not, so it isn't read again; rows written by an
-- older PRINT_INFO_VERSION are read again. `materials` is a JSON array.
CREATE TABLE IF NOT EXISTS print_info (
  asset_id INTEGER PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  title TEXT,
  designer TEXT,
  application TEXT,
  printer TEXT,
  materials TEXT NOT NULL DEFAULT '[]',
  print_time_s INTEGER,
  filament_g REAL,
  layer_height REAL
);

-- Searches saved to the sidebar. `query` is the app's own encoding of the
-- folder, search text and filters; the catalog only stores it.
CREATE TABLE IF NOT EXISTS saved_searches (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  query TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

-- User data (tags, ...) of files that left the index, keyed by content hash,
-- as JSON. A file moved to another library gets it back once it is hashed
-- there. Moves inside one library keep their row instead (`relink_moved`).
CREATE TABLE IF NOT EXISTS detached_meta (
  content_sha256 BLOB PRIMARY KEY,
  data TEXT NOT NULL,
  detached_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_assets_thumb_state ON assets(thumb_state);
CREATE INDEX IF NOT EXISTS idx_asset_tags_tag ON asset_tags(tag_id);
CREATE INDEX IF NOT EXISTS idx_assets_sha ON assets(content_sha256);

CREATE VIRTUAL TABLE IF NOT EXISTS assets_fts USING fts5(
  name, rel_path, tags, content='', contentless_delete=1
);
"#;

#[derive(Clone, Debug)]
pub struct Library {
    pub id: i64,
    pub root_path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbState {
    Pending,
    Ready,
    Failed,
    Embedded,
}

impl ThumbState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Embedded => "embedded",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            "embedded" => Self::Embedded,
            _ => Self::Pending,
        }
    }
}

/// Where an asset stands in the user's printing, set by hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintStatus {
    Todo,
    Printed,
    Failed,
}

impl PrintStatus {
    pub const ALL: [Self; 3] = [Self::Todo, Self::Printed, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Printed => "printed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "todo" => Some(Self::Todo),
            "printed" => Some(Self::Printed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// [`AssetQuery::print_status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusFilter {
    Is(PrintStatus),
    /// No print status set.
    Unmarked,
}

#[derive(Clone, Debug)]
pub struct Asset {
    pub id: i64,
    pub library_id: i64,
    pub root_path: PathBuf,
    pub rel_path: String,
    pub format: AssetFormat,
    pub size_bytes: u64,
    pub mtime_ns: i64,
    pub content_sha256: Option<Vec<u8>>,
    pub triangle_count: Option<i64>,
    pub bbox: Option<BBox>,
    pub thumb_state: ThumbState,
    pub error: Option<String>,
    pub tags: Vec<String>,
    pub status: Option<PrintStatus>,
    /// Unix seconds when `status` was last set.
    pub status_at: Option<i64>,
    /// Free text; empty when there is none.
    pub note: String,
    /// Slicer metadata of a 3MF, once read; `None` for other formats.
    pub print: Option<PrintInfo>,
}

impl Asset {
    pub fn abs_path(&self) -> PathBuf {
        self.root_path.join(&self.rel_path)
    }

    pub fn name(&self) -> &str {
        Path::new(&self.rel_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.rel_path)
    }

    pub fn sha256_hex(&self) -> Option<String> {
        self.content_sha256
            .as_ref()
            .map(|b| crate::paths::hex_sha256(b))
    }

    /// Whether the file on disk still has the size and mtime this row was
    /// read with, the same test a scan uses to skip unchanged files.
    pub fn unchanged_on_disk(&self) -> bool {
        std::fs::metadata(self.abs_path())
            .is_ok_and(|meta| meta.len() == self.size_bytes && mtime_ns(&meta) == self.mtime_ns)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AssetSort {
    #[default]
    NameAsc,
    NameDesc,
    ModifiedDesc,
    ModifiedAsc,
    SizeDesc,
    SizeAsc,
    TrianglesDesc,
    TrianglesAsc,
    Format,
}

impl AssetSort {
    pub const ALL: [Self; 9] = [
        Self::NameAsc,
        Self::NameDesc,
        Self::ModifiedDesc,
        Self::ModifiedAsc,
        Self::SizeDesc,
        Self::SizeAsc,
        Self::TrianglesDesc,
        Self::TrianglesAsc,
        Self::Format,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NameAsc => "name",
            Self::NameDesc => "name-desc",
            Self::ModifiedDesc => "modified",
            Self::ModifiedAsc => "modified-asc",
            Self::SizeDesc => "size",
            Self::SizeAsc => "size-asc",
            Self::TrianglesDesc => "triangles",
            Self::TrianglesAsc => "triangles-asc",
            Self::Format => "format",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "name-desc" => Self::NameDesc,
            "modified" | "modified-desc" => Self::ModifiedDesc,
            "modified-asc" => Self::ModifiedAsc,
            "size" | "size-desc" => Self::SizeDesc,
            "size-asc" => Self::SizeAsc,
            "triangles" | "triangles-desc" => Self::TrianglesDesc,
            "triangles-asc" => Self::TrianglesAsc,
            "format" => Self::Format,
            _ => Self::NameAsc,
        }
    }

    /// `group_by_hash` keeps identical files next to each other, ahead of the
    /// chosen sort (for the duplicates view).
    fn push_order_by(self, sql: &mut String, group_by_hash: bool) {
        // Filename is the last `/`-separated component. Missing bbox / triangle
        // counts sort last in both directions.
        const NAME: &str =
            "replace(a.rel_path, rtrim(a.rel_path, replace(a.rel_path, '/', '')), '') COLLATE NOCASE";
        sql.push_str(" ORDER BY ");
        if group_by_hash {
            sql.push_str("a.content_sha256, ");
        }
        match self {
            Self::NameAsc => {
                sql.push_str(NAME);
                sql.push_str(" ASC, a.rel_path COLLATE NOCASE ASC");
            }
            Self::NameDesc => {
                sql.push_str(NAME);
                sql.push_str(" DESC, a.rel_path COLLATE NOCASE DESC");
            }
            Self::ModifiedDesc => {
                sql.push_str("a.mtime_ns DESC, ");
                sql.push_str(NAME);
                sql.push_str(" ASC, a.rel_path COLLATE NOCASE ASC");
            }
            Self::ModifiedAsc => {
                sql.push_str("a.mtime_ns ASC, ");
                sql.push_str(NAME);
                sql.push_str(" ASC, a.rel_path COLLATE NOCASE ASC");
            }
            Self::SizeDesc => {
                sql.push_str("(a.bbox_min_x IS NULL), ");
                sql.push_str("((a.bbox_max_x - a.bbox_min_x) * (a.bbox_max_y - a.bbox_min_y) * (a.bbox_max_z - a.bbox_min_z)) DESC, ");
                sql.push_str(NAME);
                sql.push_str(" ASC, a.rel_path COLLATE NOCASE ASC");
            }
            Self::SizeAsc => {
                sql.push_str("(a.bbox_min_x IS NULL), ");
                sql.push_str("((a.bbox_max_x - a.bbox_min_x) * (a.bbox_max_y - a.bbox_min_y) * (a.bbox_max_z - a.bbox_min_z)) ASC, ");
                sql.push_str(NAME);
                sql.push_str(" ASC, a.rel_path COLLATE NOCASE ASC");
            }
            Self::TrianglesDesc => {
                sql.push_str("(a.triangle_count IS NULL), a.triangle_count DESC, ");
                sql.push_str(NAME);
                sql.push_str(" ASC, a.rel_path COLLATE NOCASE ASC");
            }
            Self::TrianglesAsc => {
                sql.push_str("(a.triangle_count IS NULL), a.triangle_count ASC, ");
                sql.push_str(NAME);
                sql.push_str(" ASC, a.rel_path COLLATE NOCASE ASC");
            }
            Self::Format => {
                sql.push_str("CASE a.format WHEN 'threemf' THEN 0 WHEN 'obj' THEN 1 WHEN 'stl' THEN 2 ELSE 3 END, ");
                sql.push_str(NAME);
                sql.push_str(" ASC, a.rel_path COLLATE NOCASE ASC");
            }
        }
    }
}

/// How [`AssetQuery::tags`] combines several tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TagMode {
    /// Asset has every tag.
    #[default]
    All,
    /// Asset has at least one of the tags.
    Any,
}

#[derive(Clone, Debug, Default)]
pub struct AssetQuery {
    pub library_id: Option<i64>,
    /// Folder inside `library_id` (`/`-separated, relative to its root);
    /// matches assets in it and its subfolders.
    pub dir: Option<String>,
    pub tags: Vec<String>,
    pub tag_mode: TagMode,
    /// Empty means every format.
    pub formats: Vec<AssetFormat>,
    /// Build volume X, Y, Z in mm: keep assets whose bbox fits, turned 90° on
    /// the bed if need be. Assets without a bbox yet are left out.
    pub fits_bed: Option<[f32; 3]>,
    /// File size range in bytes: at least `.0`, below `.1`.
    pub file_size: Option<(u64, Option<u64>)>,
    pub modified_since_ns: Option<i64>,
    pub untagged: bool,
    pub print_status: Option<StatusFilter>,
    /// Only assets whose content hash is shared with another asset.
    pub duplicates_only: bool,
    pub search: Option<String>,
    /// 3MF files whose slicer settings use any of these filament types.
    pub materials: Vec<String>,
    /// Estimated print time range in seconds: at least `.0`, below `.1`.
    /// Files without an estimate are left out.
    pub print_time: Option<(u64, Option<u64>)>,
    pub thumb_state: Option<ThumbState>,
    /// Skip libraries whose root is missing (e.g. NAS offline).
    pub online_only: bool,
    pub limit: Option<usize>,
    pub sort: AssetSort,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedSearch {
    pub id: i64,
    pub name: String,
    /// Opaque to the catalog; see the `saved_searches` table.
    pub query: String,
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    pub skipped: u32,
    /// Files renamed or moved within the library; counted in no other field.
    pub moved: u32,
}

pub struct Catalog {
    conn: Mutex<Connection>,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "DELETE FROM detached_meta WHERE detached_at < ?1",
            params![now_secs() - DETACHED_KEEP_DAYS * 86_400],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn add_library(&self, root: &Path) -> Result<Library> {
        let root = dunce_canonicalize(root)?;
        let added_at = now_secs();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO libraries (root_path, added_at) VALUES (?1, ?2)",
            params![root.to_string_lossy(), added_at],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM libraries WHERE root_path = ?1",
            params![root.to_string_lossy()],
            |r| r.get(0),
        )?;
        Ok(Library {
            id,
            root_path: root,
        })
    }

    /// Forget a library and its assets. Files on disk are untouched. Their
    /// tags are kept aside, so adding the folder back (or a folder holding
    /// the same files) restores them.
    pub fn remove_library(&self, id: i64) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        stash_user_data(&tx, "a.library_id = ?1", id)?;
        // assets_fts is contentless and not covered by the FK cascade.
        tx.execute(
            "DELETE FROM assets_fts WHERE rowid IN (SELECT id FROM assets WHERE library_id = ?1)",
            params![id],
        )?;
        tx.execute("DELETE FROM libraries WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// Drop one asset from the index (after its file was trashed).
    pub fn remove_asset(&self, id: i64) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM assets_fts WHERE rowid = ?1", params![id])?;
        tx.execute("DELETE FROM assets WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn libraries(&self) -> Result<Vec<Library>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, root_path FROM libraries ORDER BY root_path")?;
        let rows = stmt.query_map([], |r| {
            Ok(Library {
                id: r.get(0)?,
                root_path: PathBuf::from(r.get::<_, String>(1)?),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn scan_library(&self, library: &Library) -> Result<ScanStats> {
        self.scan_library_progress(library, |_, _| {})
    }

    /// If `root_path` is missing (NAS offline), leave the index untouched.
    pub fn scan_library_progress(
        &self,
        library: &Library,
        mut on_progress: impl FnMut(u32, u32),
    ) -> Result<ScanStats> {
        let mut stats = ScanStats::default();
        if !library.root_path.exists() {
            on_progress(0, 0);
            return Ok(stats);
        }

        let files = collect_mesh_files(&library.root_path);
        let total = files.len() as u32;
        on_progress(0, total);

        let mut seen = HashSet::new();
        let mut added = Vec::new();
        let mut done = 0u32;
        // Stat outside the lock, then write each chunk in one transaction so
        // queries from the UI thread can interleave between chunks.
        for chunk in files.chunks(SCAN_CHUNK) {
            let mut entries = Vec::with_capacity(chunk.len());
            for (path, format) in chunk {
                let rel = match path.strip_prefix(&library.root_path) {
                    Ok(r) => r.to_string_lossy().replace('\\', "/"),
                    Err(_) => continue,
                };
                seen.insert(rel.clone());
                match std::fs::metadata(path) {
                    Ok(meta) => entries.push((rel, *format, meta.len() as i64, mtime_ns(&meta))),
                    Err(_) => stats.skipped += 1,
                }
            }
            {
                let mut conn = self.conn.lock().unwrap();
                let tx = conn.transaction()?;
                for (rel, format, size, mtime) in entries {
                    if let Some(id) =
                        upsert_asset(&tx, library.id, &rel, format, size, mtime, &mut stats)?
                    {
                        added.push(ScannedFile {
                            id,
                            rel,
                            format: format.as_str().to_string(),
                            size,
                            mtime,
                        });
                    }
                }
                tx.commit()?;
            }
            done += chunk.len() as u32;
            on_progress(done, total);
        }

        let gone: Vec<GoneFile> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT id, rel_path, format, size_bytes, mtime_ns, content_sha256
                 FROM assets WHERE library_id = ?1",
            )?;
            let rows = stmt.query_map(params![library.id], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    GoneFile {
                        id: r.get(0)?,
                        format: r.get(2)?,
                        size: r.get(3)?,
                        mtime: r.get(4)?,
                        sha: r.get(5)?,
                    },
                ))
            })?;
            rows.filter_map(|r| r.ok())
                .filter(|(rel, _)| !seen.contains(rel))
                .map(|(_, gone)| gone)
                .collect()
        };
        // Hashing reads files, so match moves before taking the lock.
        let moves = plan_moves(&gone, &added, |file| {
            file_sha256(&library.root_path.join(&file.rel)).ok()
        });

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut moved = HashSet::new();
        for &(old_id, new_id) in &moves {
            if relink_moved(&tx, old_id, new_id)? {
                moved.insert(old_id);
                stats.added -= 1;
                stats.moved += 1;
            }
        }
        for file in gone.iter().filter(|g| !moved.contains(&g.id)) {
            stash_user_data(&tx, "a.id = ?1", file.id)?;
            let _ = tx.execute("DELETE FROM assets_fts WHERE rowid = ?1", params![file.id]);
            tx.execute("DELETE FROM assets WHERE id = ?1", params![file.id])?;
            stats.removed += 1;
        }
        tx.commit()?;
        Ok(stats)
    }

    pub fn assets(&self, query: &AssetQuery) -> Result<Vec<Asset>> {
        let online = if query.online_only {
            Some(self.online_library_ids()?)
        } else {
            None
        };
        let conn = self.conn.lock().unwrap();
        let mut sql = format!("{SELECT_ASSETS} WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        push_query_filters(query, online, &mut sql, &mut args);
        query.sort.push_order_by(&mut sql, query.duplicates_only);
        if let Some(limit) = query.limit {
            sql.push_str(" LIMIT ?");
            args.push((limit as i64).into());
        }

        let mut stmt = conn.prepare(&sql)?;
        let params_ref: Vec<&dyn rusqlite::ToSql> =
            args.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt.query(params_ref.as_slice())?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(asset_from_row(r)?);
        }
        drop(rows);
        drop(stmt);

        if !out.is_empty() {
            let index: HashMap<i64, usize> =
                out.iter().enumerate().map(|(i, a)| (a.id, i)).collect();
            let ids = format!(
                "[{}]",
                out.iter()
                    .map(|a| a.id.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let mut tag_stmt = conn.prepare(
                "SELECT at.asset_id, t.name FROM asset_tags at
                 JOIN tags t ON t.id = at.tag_id
                 WHERE at.asset_id IN (SELECT value FROM json_each(?1))
                 ORDER BY t.name",
            )?;
            let rows = tag_stmt.query_map(params![ids], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for (asset_id, name) in rows.filter_map(|r| r.ok()) {
                if let Some(&i) = index.get(&asset_id) {
                    out[i].tags.push(name);
                }
            }
        }
        Ok(out)
    }

    /// How many assets [`Self::assets`] would return, ignoring `limit`.
    pub fn count(&self, query: &AssetQuery) -> Result<u32> {
        let online = if query.online_only {
            Some(self.online_library_ids()?)
        } else {
            None
        };
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from("SELECT COUNT(*) FROM assets a WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        push_query_filters(query, online, &mut sql, &mut args);
        let n: i64 = conn.query_row(&sql, rusqlite::params_from_iter(args), |r| r.get(0))?;
        Ok(n as u32)
    }

    /// Libraries whose root is currently reachable.
    fn online_library_ids(&self) -> Result<Vec<i64>> {
        Ok(self
            .libraries()?
            .into_iter()
            .filter(|l| l.root_path.is_dir())
            .map(|l| l.id)
            .collect())
    }

    /// Pending thumbnails in online libraries only: an offline file can't be
    /// read, and marking it `failed` would stick after it comes back.
    pub fn pending_thumbs(&self, limit: usize) -> Result<Vec<Asset>> {
        self.assets(&AssetQuery {
            thumb_state: Some(ThumbState::Pending),
            online_only: true,
            limit: Some(limit),
            ..Default::default()
        })
    }

    /// Counts what [`Self::pending_thumbs`] would process.
    pub fn pending_count(&self) -> Result<u32> {
        let online = self.online_library_ids()?;
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT COUNT(*) FROM assets WHERE thumb_state = 'pending' AND library_id IN ({})",
            placeholders(online.len())
        );
        let n: i64 = conn.query_row(&sql, rusqlite::params_from_iter(online), |r| r.get(0))?;
        Ok(n as u32)
    }

    /// Up to `limit` assets without a content hash, in online libraries, in
    /// id order after `after_id`. Paging by id lets the hashing pass walk the
    /// table once, so a file it can't read is not fetched again and again.
    pub fn unhashed_assets(&self, after_id: i64, limit: usize) -> Result<Vec<Asset>> {
        let online = self.online_library_ids()?;
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "{SELECT_ASSETS} WHERE a.content_sha256 IS NULL AND a.id > ?
               AND a.library_id IN ({}) ORDER BY a.id LIMIT ?",
            placeholders(online.len())
        );
        let mut args: Vec<rusqlite::types::Value> = vec![after_id.into()];
        args.extend(online.into_iter().map(Into::into));
        args.push((limit as i64).into());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), asset_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Up to `limit` 3MF assets in online libraries whose slicer metadata
    /// hasn't been read (or was read by an older version), in id order after
    /// `after_id`, like [`Self::unhashed_assets`].
    pub fn assets_without_print_info(&self, after_id: i64, limit: usize) -> Result<Vec<Asset>> {
        let online = self.online_library_ids()?;
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "{SELECT_ASSETS} WHERE a.format = 'threemf' AND a.id > ?
               AND (p.asset_id IS NULL OR p.version < ?)
               AND a.library_id IN ({}) ORDER BY a.id LIMIT ?",
            placeholders(online.len())
        );
        let mut args: Vec<rusqlite::types::Value> =
            vec![after_id.into(), PRINT_INFO_VERSION.into()];
        args.extend(online.into_iter().map(Into::into));
        args.push((limit as i64).into());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), asset_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Store what was read from `asset`'s file, unless a scan has seen the
    /// file change since `asset` was read (as in [`Self::hash_asset`]).
    pub fn set_print_info(&self, asset: &Asset, info: &PrintInfo) -> Result<()> {
        let materials = serde_json::to_string(&info.materials).unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO print_info (asset_id, version, title, designer, application,
                 printer, materials, print_time_s, filament_g, layer_height)
             SELECT id, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12 FROM assets
             WHERE id = ?1 AND size_bytes = ?2 AND mtime_ns = ?3",
            params![
                asset.id,
                asset.size_bytes as i64,
                asset.mtime_ns,
                PRINT_INFO_VERSION,
                info.title,
                info.designer,
                info.application,
                info.printer,
                materials,
                info.print_time_s.map(|s| s as i64),
                info.filament_g,
                info.layer_height_mm,
            ],
        )?;
        Ok(())
    }

    /// Filament types found in 3MF files, with how many files use each,
    /// most used first.
    pub fn all_materials(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.value, COUNT(*) FROM print_info p, json_each(p.materials) m
             GROUP BY m.value ORDER BY COUNT(*) DESC, m.value",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Queue every app-rendered (`ready`) thumbnail again and retry `failed`
    /// ones, e.g. after the renderer or loader changed. Slicer-embedded
    /// thumbnails are left alone. Returns how many were requeued.
    pub fn requeue_rendered_thumbs(&self) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE assets SET thumb_state='pending', error=NULL
             WHERE thumb_state IN ('ready', 'failed')",
            [],
        )?;
        Ok(n as u32)
    }

    /// Reset `ready` / `embedded` assets to `pending` when `thumb_exists` says
    /// their cached PNG is gone (the OS or the user may clear the cache dir).
    /// Returns how many were requeued.
    pub fn requeue_missing_thumbs(&self, thumb_exists: impl Fn(&str) -> bool) -> Result<u32> {
        let rows: Vec<(i64, Option<Vec<u8>>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT id, content_sha256 FROM assets WHERE thumb_state IN ('ready', 'embedded')",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // Stat files without holding the DB lock.
        let missing: Vec<i64> = rows
            .into_iter()
            .filter(|(_, sha)| {
                sha.as_deref()
                    .is_none_or(|b| !thumb_exists(&crate::paths::hex_sha256(b)))
            })
            .map(|(id, _)| id)
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("UPDATE assets SET thumb_state='pending', error=NULL WHERE id=?1")?;
            for id in &missing {
                stmt.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(missing.len() as u32)
    }

    pub fn set_mesh_meta(&self, id: i64, sha: &[u8], triangles: i64, bbox: &BBox) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE assets SET content_sha256=?1, triangle_count=?2,
                bbox_min_x=?3, bbox_min_y=?4, bbox_min_z=?5,
                bbox_max_x=?6, bbox_max_y=?7, bbox_max_z=?8
             WHERE id=?9",
            params![
                sha,
                triangles,
                bbox.min[0],
                bbox.min[1],
                bbox.min[2],
                bbox.max[0],
                bbox.max[1],
                bbox.max[2],
                id
            ],
        )?;
        Ok(())
    }

    pub fn set_thumb_state(&self, id: i64, state: ThumbState, error: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE assets SET thumb_state=?1, error=?2 WHERE id=?3",
            params![state.as_str(), error, id],
        )?;
        Ok(())
    }

    /// Hash `asset`'s file and store the hash, unless a scan has seen the file
    /// change since `asset` was read: the row then has a new size or mtime,
    /// and a hash of the old contents must not land on it.
    ///
    /// A file that left the index with tags (moved out of another library)
    /// gets them back here, if this asset has none of its own yet.
    pub fn hash_asset(&self, asset: &Asset) -> Result<Vec<u8>> {
        let sha = file_sha256(&asset.abs_path())?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let stored = tx.execute(
            "UPDATE assets SET content_sha256=?1 WHERE id=?2 AND size_bytes=?3 AND mtime_ns=?4",
            params![&sha[..], asset.id, asset.size_bytes as i64, asset.mtime_ns],
        )?;
        if stored > 0 {
            reattach_user_data(&tx, asset.id, &sha)?;
        }
        tx.commit()?;
        Ok(sha.to_vec())
    }

    pub fn all_tags(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT t.name, COUNT(at.asset_id) FROM tags t
             LEFT JOIN asset_tags at ON at.tag_id = t.id
             GROUP BY t.id ORDER BY t.name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn set_tags(&self, asset_id: i64, tags: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM asset_tags WHERE asset_id = ?1",
            params![asset_id],
        )?;
        for raw in tags {
            insert_asset_tag(&tx, asset_id, raw)?;
        }
        refresh_fts(&tx, asset_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Add `tags` to every asset in `ids`, keeping the tags they already have.
    pub fn add_tags(&self, ids: &[i64], tags: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for &id in ids {
            for raw in tags {
                insert_asset_tag(&tx, id, raw)?;
            }
            refresh_fts(&tx, id)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Take `tag` off every asset in `ids`.
    pub fn remove_tag(&self, ids: &[i64], tag: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for &id in ids {
            tx.execute(
                "DELETE FROM asset_tags WHERE asset_id = ?1
                   AND tag_id = (SELECT id FROM tags WHERE name = ?2)",
                params![id, tag],
            )?;
            refresh_fts(&tx, id)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Set (or with `None` clear) the print status of every asset in `ids`.
    pub fn set_status(&self, ids: &[i64], status: Option<PrintStatus>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let now = now_secs();
        for &id in ids {
            tx.execute(
                "INSERT INTO asset_notes (asset_id, status, status_at)
                 SELECT id, ?2, ?3 FROM assets WHERE id = ?1
                 ON CONFLICT (asset_id) DO UPDATE
                   SET status = excluded.status, status_at = excluded.status_at",
                params![id, status.map(PrintStatus::as_str), status.map(|_| now)],
            )?;
            drop_empty_notes(&tx, id)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace an asset's note. A blank note clears it.
    pub fn set_note(&self, id: i64, note: &str) -> Result<()> {
        let note = if note.trim().is_empty() { "" } else { note };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO asset_notes (asset_id, note) SELECT id, ?2 FROM assets WHERE id = ?1
             ON CONFLICT (asset_id) DO UPDATE SET note = excluded.note",
            params![id, note],
        )?;
        drop_empty_notes(&conn, id)?;
        Ok(())
    }

    /// Saved searches by name.
    pub fn saved_searches(&self) -> Result<Vec<SavedSearch>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, query FROM saved_searches ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SavedSearch {
                id: r.get(0)?,
                name: r.get(1)?,
                query: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_saved_search(&self, name: &str, query: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO saved_searches (name, query, created_at) VALUES (?1, ?2, ?3)",
            params![name.trim(), query, now_secs()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn rename_saved_search(&self, id: i64, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE saved_searches SET name = ?2 WHERE id = ?1",
            params![id, name.trim()],
        )?;
        Ok(())
    }

    pub fn set_saved_search_query(&self, id: i64, query: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE saved_searches SET query = ?2 WHERE id = ?1",
            params![id, query],
        )?;
        Ok(())
    }

    pub fn remove_saved_search(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM saved_searches WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Relative paths of every asset in a library, for building its folder tree.
    pub fn rel_paths(&self, library_id: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT rel_path FROM assets WHERE library_id = ?1")?;
        let rows = stmt.query_map(params![library_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// How many assets have this content hash (the asset itself included).
    pub fn copies(&self, sha: &[u8]) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM assets WHERE content_sha256 = ?1",
            params![sha],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// How many assets share their content hash with at least one other asset.
    pub fn duplicate_count(&self) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COALESCE(SUM(n), 0) FROM (
               SELECT COUNT(*) AS n FROM assets WHERE content_sha256 IS NOT NULL
               GROUP BY content_sha256 HAVING COUNT(*) > 1)",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }
}

/// Append `query`'s conditions (all on `assets a`) to `sql`, a statement
/// ending in a `WHERE` clause, and their values to `args`. `online` lists
/// the reachable libraries when the query asks for online ones only.
fn push_query_filters(
    query: &AssetQuery,
    online: Option<Vec<i64>>,
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
) {
    if let Some(id) = query.library_id {
        sql.push_str(" AND a.library_id = ?");
        args.push(id.into());
    }
    if let Some(dir) = query.dir.as_deref().map(|d| d.trim_matches('/')) {
        if !dir.is_empty() {
            sql.push_str(" AND a.rel_path LIKE ? ESCAPE '\\'");
            args.push(format!("{}/%", escape_like(dir)).into());
        }
    }
    if !query.tags.is_empty() {
        sql.push_str(&format!(
            " AND a.id IN (SELECT at.asset_id FROM asset_tags at JOIN tags t ON t.id = at.tag_id
               WHERE t.name IN ({}) GROUP BY at.asset_id",
            placeholders(query.tags.len())
        ));
        args.extend(query.tags.iter().map(|t| t.clone().into()));
        if query.tag_mode == TagMode::All {
            sql.push_str(" HAVING COUNT(DISTINCT t.id) = ?");
            let distinct: HashSet<&String> = query.tags.iter().collect();
            args.push((distinct.len() as i64).into());
        }
        sql.push(')');
    }
    if query.untagged {
        sql.push_str(" AND NOT EXISTS (SELECT 1 FROM asset_tags at WHERE at.asset_id = a.id)");
    }
    match query.print_status {
        Some(StatusFilter::Is(status)) => {
            sql.push_str(" AND a.id IN (SELECT asset_id FROM asset_notes WHERE status = ?)");
            args.push(status.as_str().to_string().into());
        }
        Some(StatusFilter::Unmarked) => sql.push_str(
            " AND a.id NOT IN (SELECT asset_id FROM asset_notes WHERE status IS NOT NULL)",
        ),
        None => {}
    }
    if !query.formats.is_empty() {
        sql.push_str(&format!(
            " AND a.format IN ({})",
            placeholders(query.formats.len())
        ));
        args.extend(query.formats.iter().map(|f| f.as_str().to_string().into()));
    }
    if let Some([x, y, z]) = query.fits_bed {
        sql.push_str(
            " AND a.bbox_min_x IS NOT NULL
              AND (a.bbox_max_z - a.bbox_min_z) <= ?
              AND (((a.bbox_max_x - a.bbox_min_x) <= ? AND (a.bbox_max_y - a.bbox_min_y) <= ?)
                OR ((a.bbox_max_x - a.bbox_min_x) <= ? AND (a.bbox_max_y - a.bbox_min_y) <= ?))",
        );
        for v in [z, x, y, y, x] {
            args.push(f64::from(v).into());
        }
    }
    if let Some((min, max)) = query.file_size {
        sql.push_str(" AND a.size_bytes >= ?");
        args.push((min as i64).into());
        if let Some(max) = max {
            sql.push_str(" AND a.size_bytes < ?");
            args.push((max as i64).into());
        }
    }
    if let Some(since) = query.modified_since_ns {
        sql.push_str(" AND a.mtime_ns >= ?");
        args.push(since.into());
    }
    if query.duplicates_only {
        sql.push_str(
            " AND a.content_sha256 IN (
                SELECT content_sha256 FROM assets WHERE content_sha256 IS NOT NULL
                GROUP BY content_sha256 HAVING COUNT(*) > 1)",
        );
    }
    if let Some(search) = query.search.as_ref().map(|s| s.trim().to_string()) {
        if !search.is_empty() {
            let like = format!("%{}%", escape_like(&search));
            sql.push_str(
                " AND (a.rel_path LIKE ? ESCAPE '\\' OR a.id IN (
                    SELECT at.asset_id FROM asset_tags at
                    JOIN tags t ON t.id = at.tag_id
                    WHERE t.name LIKE ? ESCAPE '\\'
                  ) OR a.id IN (
                    SELECT asset_id FROM asset_notes WHERE note LIKE ? ESCAPE '\\'
                  ) OR a.id IN (
                    SELECT asset_id FROM print_info
                    WHERE COALESCE(title, '') || ' ' || COALESCE(designer, '') || ' '
                          || COALESCE(printer, '') || ' ' || materials LIKE ? ESCAPE '\\'
                  ) OR a.id IN (
                    SELECT rowid FROM assets_fts WHERE assets_fts MATCH ?
                  ))",
            );
            args.push(like.clone().into());
            args.push(like.clone().into());
            args.push(like.clone().into());
            args.push(like.into());
            args.push(fts_query(&search).into());
        }
    }
    if !query.materials.is_empty() {
        sql.push_str(&format!(
            " AND a.id IN (SELECT p.asset_id FROM print_info p, json_each(p.materials) m
               WHERE m.value IN ({}))",
            placeholders(query.materials.len())
        ));
        args.extend(query.materials.iter().map(|m| m.clone().into()));
    }
    if let Some((min, max)) = query.print_time {
        sql.push_str(" AND a.id IN (SELECT asset_id FROM print_info WHERE print_time_s >= ?");
        args.push((min as i64).into());
        if let Some(max) = max {
            sql.push_str(" AND print_time_s < ?");
            args.push((max as i64).into());
        }
        sql.push(')');
    }
    if let Some(state) = query.thumb_state {
        sql.push_str(" AND a.thumb_state = ?");
        args.push(state.as_str().to_string().into());
    }
    if let Some(ids) = online {
        sql.push_str(&format!(
            " AND a.library_id IN ({})",
            placeholders(ids.len())
        ));
        args.extend(ids.into_iter().map(Into::into));
    }
}

/// The columns [`asset_from_row`] reads; callers append the `WHERE` clause.
const SELECT_ASSETS: &str = "SELECT a.id, a.library_id, l.root_path, a.rel_path, a.format,
        a.size_bytes, a.mtime_ns, a.content_sha256, a.triangle_count,
        a.bbox_min_x, a.bbox_min_y, a.bbox_min_z,
        a.bbox_max_x, a.bbox_max_y, a.bbox_max_z,
        a.thumb_state, a.error, n.status, n.status_at, COALESCE(n.note, ''),
        p.asset_id, p.title, p.designer, p.application, p.printer, p.materials,
        p.print_time_s, p.filament_g, p.layer_height
 FROM assets a
 JOIN libraries l ON l.id = a.library_id
 LEFT JOIN asset_notes n ON n.asset_id = a.id
 LEFT JOIN print_info p ON p.asset_id = a.id";

/// An [`Asset`] from a [`SELECT_ASSETS`] row, without its tags.
fn asset_from_row(r: &rusqlite::Row) -> rusqlite::Result<Asset> {
    let bbox = match (
        r.get::<_, Option<f32>>(9)?,
        r.get::<_, Option<f32>>(10)?,
        r.get::<_, Option<f32>>(11)?,
        r.get::<_, Option<f32>>(12)?,
        r.get::<_, Option<f32>>(13)?,
        r.get::<_, Option<f32>>(14)?,
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f)) => Some(BBox {
            min: [a, b, c],
            max: [d, e, f],
        }),
        _ => None,
    };
    let format = AssetFormat::parse(&r.get::<_, String>(4)?).unwrap_or(AssetFormat::Stl);
    Ok(Asset {
        id: r.get(0)?,
        library_id: r.get(1)?,
        root_path: PathBuf::from(r.get::<_, String>(2)?),
        rel_path: r.get(3)?,
        format,
        size_bytes: r.get::<_, i64>(5)? as u64,
        mtime_ns: r.get(6)?,
        content_sha256: r.get(7)?,
        triangle_count: r.get(8)?,
        bbox,
        thumb_state: ThumbState::parse(&r.get::<_, String>(15)?),
        error: r.get(16)?,
        tags: Vec::new(),
        status: r
            .get::<_, Option<String>>(17)?
            .as_deref()
            .and_then(PrintStatus::parse),
        status_at: r.get(18)?,
        note: r.get(19)?,
        print: match r.get::<_, Option<i64>>(20)? {
            None => None,
            Some(_) => Some(PrintInfo {
                title: r.get(21)?,
                designer: r.get(22)?,
                application: r.get(23)?,
                printer: r.get(24)?,
                materials: r
                    .get::<_, Option<String>>(25)?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                print_time_s: r.get::<_, Option<i64>>(26)?.map(|s| s.max(0) as u64),
                filament_g: r.get(27)?,
                layer_height_mm: r.get(28)?,
            }),
        },
    })
}

/// Drop an asset's `asset_notes` row once it holds neither status nor note.
fn drop_empty_notes(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM asset_notes WHERE asset_id = ?1 AND status IS NULL AND note = ''",
        params![id],
    )?;
    Ok(())
}

/// Attach one tag (created if new) to an asset. Blank names are ignored.
fn insert_asset_tag(conn: &Connection, asset_id: i64, raw: &str) -> Result<()> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO tags (name) VALUES (?1)",
        params![name],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO asset_tags (asset_id, tag_id)
         SELECT ?1, id FROM tags WHERE name = ?2",
        params![asset_id, name],
    )?;
    Ok(())
}

/// Rewrite an asset's `assets_fts` row from its current path and tags. The
/// table is contentless and has no triggers, so every tag write calls this.
fn refresh_fts(conn: &Connection, asset_id: i64) -> Result<()> {
    let rel: String = conn.query_row(
        "SELECT rel_path FROM assets WHERE id=?1",
        params![asset_id],
        |r| r.get(0),
    )?;
    let tags: String = conn.query_row(
        "SELECT COALESCE(group_concat(t.name, ' '), '') FROM asset_tags at
         JOIN tags t ON t.id = at.tag_id WHERE at.asset_id = ?1",
        params![asset_id],
        |r| r.get(0),
    )?;
    let file_name = Path::new(&rel)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&rel)
        .to_string();
    let _ = conn.execute("DELETE FROM assets_fts WHERE rowid=?1", params![asset_id]);
    let _ = conn.execute(
        "INSERT INTO assets_fts (rowid, name, rel_path, tags) VALUES (?1, ?2, ?3, ?4)",
        params![asset_id, file_name, rel, tags],
    );
    Ok(())
}

fn upsert_asset(
    conn: &Connection,
    library_id: i64,
    rel: &str,
    format: AssetFormat,
    size: i64,
    mtime: i64,
    stats: &mut ScanStats,
) -> Result<Option<i64>> {
    let existing: Option<(i64, i64, i64)> = conn
        .query_row(
            "SELECT id, size_bytes, mtime_ns FROM assets WHERE library_id = ?1 AND rel_path = ?2",
            params![library_id, rel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    match existing {
        Some((_id, old_size, old_mtime)) if old_size == size && old_mtime == mtime => {
            stats.skipped += 1;
        }
        Some((id, _, _)) => {
            conn.execute(
                "UPDATE assets SET format=?1, size_bytes=?2, mtime_ns=?3, thumb_state='pending', error=NULL, content_sha256=NULL
                 WHERE id=?4",
                params![format.as_str(), size, mtime, id],
            )?;
            conn.execute("DELETE FROM print_info WHERE asset_id = ?1", params![id])?;
            stats.updated += 1;
        }
        None => {
            conn.execute(
                "INSERT INTO assets (library_id, rel_path, format, size_bytes, mtime_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![library_id, rel, format.as_str(), size, mtime],
            )?;
            let id = conn.last_insert_rowid();
            let name = Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(rel);
            let _ = conn.execute(
                "INSERT INTO assets_fts (rowid, name, rel_path, tags) VALUES (?1, ?2, ?3, '')",
                params![id, name, rel],
            );
            stats.added += 1;
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// A file a scan added to the index.
struct ScannedFile {
    id: i64,
    rel: String,
    format: String,
    size: i64,
    mtime: i64,
}

/// An indexed file a scan no longer found.
struct GoneFile {
    id: i64,
    format: String,
    size: i64,
    mtime: i64,
    sha: Option<Vec<u8>>,
}

/// Pair files that vanished with files that appeared in the same scan, as
/// (gone id, added id), when they are the same file renamed or moved:
///
/// - the one gone file and the one added file with a given size, mtime and
///   format (the test a scan already uses to call a file unchanged), or
/// - an added file with the gone file's size and format whose content hash
///   (from `hash`) matches the gone file's stored hash, e.g. moved by a copy
///   that reset its mtime.
fn plan_moves(
    gone: &[GoneFile],
    added: &[ScannedFile],
    mut hash: impl FnMut(&ScannedFile) -> Option<[u8; 32]>,
) -> Vec<(i64, i64)> {
    type Key<'a> = (i64, i64, &'a str);
    let mut gone_by_key: HashMap<Key, Vec<&GoneFile>> = HashMap::new();
    for g in gone {
        gone_by_key
            .entry((g.size, g.mtime, &g.format))
            .or_default()
            .push(g);
    }
    let mut added_by_key: HashMap<Key, Vec<&ScannedFile>> = HashMap::new();
    for a in added {
        added_by_key
            .entry((a.size, a.mtime, &a.format))
            .or_default()
            .push(a);
    }

    let mut moves = Vec::new();
    let mut matched_gone = HashSet::new();
    let mut claimed = HashSet::new();
    for (key, gs) in &gone_by_key {
        if let (&[g], Some(&[a])) = (gs.as_slice(), added_by_key.get(key).map(Vec::as_slice)) {
            moves.push((g.id, a.id));
            matched_gone.insert(g.id);
            claimed.insert(a.id);
        }
    }

    let mut hashes: HashMap<i64, Option<[u8; 32]>> = HashMap::new();
    for g in gone.iter().filter(|g| !matched_gone.contains(&g.id)) {
        let Some(sha) = g.sha.as_deref() else {
            continue;
        };
        let mut candidates: Vec<&ScannedFile> = added
            .iter()
            .filter(|a| !claimed.contains(&a.id) && a.size == g.size && a.format == g.format)
            .collect();
        // Same mtime first: likeliest to be this file.
        candidates.sort_by_key(|a| a.mtime != g.mtime);
        for a in candidates.into_iter().take(MOVE_HASH_CANDIDATES) {
            let found = *hashes.entry(a.id).or_insert_with(|| hash(a));
            if found.is_some_and(|h| h[..] == *sha) {
                moves.push((g.id, a.id));
                claimed.insert(a.id);
                break;
            }
        }
    }
    moves
}

/// Point the row of a vanished file (`old_id`) at the file it was moved to,
/// and drop the row the scan had just added for that file (`new_id`). The
/// old row keeps its id, so its tags, thumbnail and everything else keyed
/// by asset id stay with the file. Returns false if either row is gone.
fn relink_moved(conn: &Connection, old_id: i64, new_id: i64) -> Result<bool> {
    let new: Option<(String, String, i64, i64)> = conn
        .query_row(
            "SELECT rel_path, format, size_bytes, mtime_ns FROM assets WHERE id = ?1",
            params![new_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let old_exists = conn
        .query_row(
            "SELECT 1 FROM assets WHERE id = ?1",
            params![old_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let (Some((rel, format, size, mtime)), true) = (new, old_exists) else {
        return Ok(false);
    };
    // Tags someone added to the new row in the meantime come along.
    conn.execute(
        "INSERT OR IGNORE INTO asset_tags (asset_id, tag_id)
         SELECT ?1, tag_id FROM asset_tags WHERE asset_id = ?2",
        params![old_id, new_id],
    )?;
    let _ = conn.execute("DELETE FROM assets_fts WHERE rowid = ?1", params![new_id]);
    conn.execute("DELETE FROM assets WHERE id = ?1", params![new_id])?;
    conn.execute(
        "UPDATE assets SET rel_path = ?1, format = ?2, size_bytes = ?3, mtime_ns = ?4
         WHERE id = ?5",
        params![rel, format, size, mtime, old_id],
    )?;
    refresh_fts(conn, old_id)?;
    Ok(true)
}

/// Keep aside the user data of the assets matching `condition` (SQL on
/// `assets a`, with `?1` bound to `arg`) before they leave the index, keyed
/// by content hash. Assets without a hash or without user data are skipped.
fn stash_user_data(conn: &Connection, condition: &str, arg: i64) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO detached_meta (content_sha256, data, detached_at)
             SELECT a.content_sha256, json_object(
                 'tags', json((SELECT json_group_array(t.name) FROM asset_tags at
                               JOIN tags t ON t.id = at.tag_id WHERE at.asset_id = a.id)),
                 'status', n.status, 'status_at', n.status_at, 'note', n.note
               ), ?2
             FROM assets a
             LEFT JOIN asset_notes n ON n.asset_id = a.id
             WHERE ({condition}) AND a.content_sha256 IS NOT NULL
               AND (n.asset_id IS NOT NULL
                    OR EXISTS (SELECT 1 FROM asset_tags at WHERE at.asset_id = a.id))"
        ),
        params![arg, now_secs()],
    )?;
    Ok(())
}

/// Give asset `id` the user data stashed for content hash `sha`, if any and
/// if the asset has none of its own. The stash is used up either way, so
/// only one copy of the file gets it.
fn reattach_user_data(conn: &Connection, id: i64, sha: &[u8]) -> Result<()> {
    let Some(data) = conn
        .query_row(
            "SELECT data FROM detached_meta WHERE content_sha256 = ?1",
            params![sha],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(());
    };
    let has_own: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM asset_tags WHERE asset_id = ?1)
             OR EXISTS (SELECT 1 FROM asset_notes WHERE asset_id = ?1)",
        params![id],
        |r| r.get(0),
    )?;
    if !has_own {
        let mut stmt = conn.prepare("SELECT value FROM json_each(?1, '$.tags')")?;
        let tags: Vec<String> = stmt
            .query_map(params![data], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for tag in &tags {
            insert_asset_tag(conn, id, tag)?;
        }
        refresh_fts(conn, id)?;
        conn.execute(
            "INSERT INTO asset_notes (asset_id, status, status_at, note)
             SELECT ?1, json_extract(?2, '$.status'), json_extract(?2, '$.status_at'),
                    COALESCE(json_extract(?2, '$.note'), '')",
            params![id, data],
        )?;
        drop_empty_notes(conn, id)?;
    }
    conn.execute(
        "DELETE FROM detached_meta WHERE content_sha256 = ?1",
        params![sha],
    )?;
    Ok(())
}

fn collect_mesh_files(root: &Path) -> Vec<(PathBuf, AssetFormat)> {
    WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0 || e.file_name().to_str().is_none_or(|s| !s.starts_with('.'))
        })
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().is_file() {
                return None;
            }
            let path = entry.path().to_path_buf();
            let format = AssetFormat::from_path(&path)?;
            Some((path, format))
        })
        .collect()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn dunce_canonicalize(path: &Path) -> Result<PathBuf> {
    Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `?, ?, ?` for an `IN (...)` list; `NULL` when empty so the SQL stays valid
/// and matches nothing.
fn placeholders(n: usize) -> String {
    if n == 0 {
        "NULL".into()
    } else {
        vec!["?"; n].join(", ")
    }
}

fn fts_query(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "\"\"".into()
    } else {
        format!("\"{cleaned}\"*")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_add_update_remove() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(root).unwrap();
        let stats = cat.scan_library(&lib).unwrap();
        assert_eq!(stats.added, 1);
        let assets = cat.assets(&AssetQuery::default()).unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].name(), "a.stl");

        fs::write(root.join("b.obj"), b"v 0 0 0\n").unwrap();
        let stats = cat.scan_library(&lib).unwrap();
        assert_eq!(stats.added, 1);

        fs::remove_file(root.join("a.stl")).unwrap();
        let stats = cat.scan_library(&lib).unwrap();
        assert_eq!(stats.removed, 1);
        let assets = cat.assets(&AssetQuery::default()).unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].name(), "b.obj");
    }

    #[test]
    fn tags_and_search() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("benchy.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let assets = cat.assets(&AssetQuery::default()).unwrap();
        cat.set_tags(assets[0].id, &["校準".into(), "boat".into()])
            .unwrap();

        let found = cat
            .assets(&AssetQuery {
                search: Some("benchy".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(found.len(), 1);

        let tagged = cat
            .assets(&AssetQuery {
                tags: vec!["校準".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(tagged.len(), 1);
        assert!(tagged[0].tags.contains(&"校準".into()));
    }

    #[test]
    fn assets_attach_sorted_tags_to_each_asset() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.stl", "b.stl", "c.stl"] {
            fs::write(dir.path().join(name), b"solid x\nendsolid x\n").unwrap();
        }
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let assets = cat.assets(&AssetQuery::default()).unwrap();
        let id = |n: &str| assets.iter().find(|a| a.name() == n).unwrap().id;
        cat.set_tags(id("a.stl"), &["zeta".into(), "alpha".into()])
            .unwrap();
        cat.set_tags(id("b.stl"), &["mid".into()]).unwrap();

        let assets = cat.assets(&AssetQuery::default()).unwrap();
        let tags = |n: &str| assets.iter().find(|a| a.name() == n).unwrap().tags.clone();
        assert_eq!(tags("a.stl"), vec!["alpha".to_string(), "zeta".to_string()]);
        assert_eq!(tags("b.stl"), vec!["mid".to_string()]);
        assert!(tags("c.stl").is_empty());

        let by_tag = cat
            .assets(&AssetQuery {
                search: Some("zet".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_tag.len(), 1);
        assert_eq!(by_tag[0].name(), "a.stl");
    }

    #[test]
    fn offline_library_keeps_index() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("keep.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        assert_eq!(cat.assets(&AssetQuery::default()).unwrap().len(), 1);

        let missing = Library {
            id: lib.id,
            root_path: dir.path().join("gone"),
        };
        let stats = cat.scan_library(&missing).unwrap();
        assert_eq!(stats.removed, 0);
        assert_eq!(cat.assets(&AssetQuery::default()).unwrap().len(), 1);
    }

    #[test]
    fn pending_thumbs_limited() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        fs::write(dir.path().join("b.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        assert_eq!(cat.pending_count().unwrap(), 2);
        assert_eq!(cat.pending_thumbs(1).unwrap().len(), 1);
    }

    #[test]
    fn open_file_catalog_roundtrip_and_remove_library() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        let db = dir.path().join("catalog.sqlite");
        let cat = Catalog::open(&db).unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        assert_eq!(cat.libraries().unwrap().len(), 1);
        cat.remove_library(lib.id).unwrap();
        assert!(cat.libraries().unwrap().is_empty());
        assert!(cat.assets(&AssetQuery::default()).unwrap().is_empty());
    }

    fn fts_rows(cat: &Catalog) -> i64 {
        cat.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM assets_fts", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn pending_thumbs_skip_offline_libraries() {
        let online = tempfile::tempdir().unwrap();
        let offline = tempfile::tempdir().unwrap();
        fs::write(online.path().join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        fs::write(offline.path().join("b.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        for dir in [&online, &offline] {
            let lib = cat.add_library(dir.path()).unwrap();
            cat.scan_library(&lib).unwrap();
        }
        assert_eq!(cat.pending_count().unwrap(), 2);

        let offline_root = offline.path().to_path_buf();
        offline.close().unwrap();
        let pending = cat.pending_thumbs(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].rel_path, "a.stl");
        assert_eq!(cat.pending_count().unwrap(), 1);
        // Still indexed, just not processed.
        assert_eq!(cat.assets(&AssetQuery::default()).unwrap().len(), 2);

        fs::create_dir(&offline_root).unwrap();
        fs::write(offline_root.join("b.stl"), b"solid x\nendsolid x\n").unwrap();
        assert_eq!(cat.pending_count().unwrap(), 2);
        fs::remove_dir_all(&offline_root).unwrap();
    }

    #[test]
    fn remove_library_clears_search_index() {
        let keep = tempfile::tempdir().unwrap();
        let gone = tempfile::tempdir().unwrap();
        fs::write(keep.path().join("keep.stl"), b"solid x\nendsolid x\n").unwrap();
        fs::write(gone.path().join("gone.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let keep_lib = cat.add_library(keep.path()).unwrap();
        let gone_lib = cat.add_library(gone.path()).unwrap();
        cat.scan_library(&keep_lib).unwrap();
        cat.scan_library(&gone_lib).unwrap();
        assert_eq!(fts_rows(&cat), 2);

        cat.remove_library(gone_lib.id).unwrap();
        assert_eq!(fts_rows(&cat), 1);
        let search = |q: &str| {
            cat.assets(&AssetQuery {
                search: Some(q.into()),
                ..Default::default()
            })
            .unwrap()
            .len()
        };
        assert_eq!(search("keep"), 1);
        assert_eq!(search("gone"), 0);
    }

    #[test]
    fn remove_asset_drops_row_tags_and_search_entry() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        fs::write(dir.path().join("b.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let a = cat
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .find(|x| x.rel_path == "a.stl")
            .unwrap();
        cat.set_tags(a.id, &["benchy".into()]).unwrap();

        cat.remove_asset(a.id).unwrap();
        let left = cat.assets(&AssetQuery::default()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].rel_path, "b.stl");
        assert_eq!(fts_rows(&cat), 1);
        assert_eq!(cat.all_tags().unwrap(), vec![("benchy".to_string(), 0)]);
    }

    #[test]
    fn scan_skips_hidden_dirs_and_non_mesh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".cache")).unwrap();
        fs::write(
            root.join(".cache").join("hidden.stl"),
            b"solid x\nendsolid x\n",
        )
        .unwrap();
        fs::write(root.join("notes.txt"), b"hello").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(
            root.join("sub").join("ok.obj"),
            b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n",
        )
        .unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(root).unwrap();
        let stats = cat.scan_library(&lib).unwrap();
        assert_eq!(stats.added, 1);
        let assets = cat.assets(&AssetQuery::default()).unwrap();
        assert_eq!(assets[0].rel_path, "sub/ok.obj");
        assert!(assets[0].abs_path().ends_with("sub/ok.obj"));
    }

    #[test]
    fn mesh_meta_hash_and_thumb_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let asset = cat.assets(&AssetQuery::default()).unwrap().pop().unwrap();
        let sha = cat.hash_asset(&asset).unwrap();
        assert_eq!(sha.len(), 32);
        let bbox = BBox {
            min: [0.0, 0.0, 0.0],
            max: [10.0, 20.0, 30.0],
        };
        cat.set_mesh_meta(asset.id, &sha, 12, &bbox).unwrap();
        cat.set_thumb_state(asset.id, ThumbState::Ready, None)
            .unwrap();
        let updated = cat.assets(&AssetQuery::default()).unwrap().pop().unwrap();
        assert_eq!(updated.triangle_count, Some(12));
        assert_eq!(updated.thumb_state, ThumbState::Ready);
        assert_eq!(updated.bbox.unwrap().format_mm(), "10×20×30 mm");
        assert_eq!(updated.sha256_hex().unwrap().len(), 64);
        assert_eq!(cat.pending_count().unwrap(), 0);
    }

    #[test]
    fn requeue_rendered_thumbs_keeps_embedded() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["r.stl", "e.3mf", "f.stl"] {
            fs::write(dir.path().join(name), name).unwrap();
        }
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let state_of = |n: &str| {
            cat.assets(&AssetQuery::default())
                .unwrap()
                .into_iter()
                .find(|a| a.rel_path == n)
                .unwrap()
        };
        cat.set_thumb_state(state_of("r.stl").id, ThumbState::Ready, None)
            .unwrap();
        cat.set_thumb_state(state_of("e.3mf").id, ThumbState::Embedded, None)
            .unwrap();
        cat.set_thumb_state(state_of("f.stl").id, ThumbState::Failed, Some("x"))
            .unwrap();

        assert_eq!(cat.requeue_rendered_thumbs().unwrap(), 2);
        assert_eq!(state_of("r.stl").thumb_state, ThumbState::Pending);
        let failed = state_of("f.stl");
        assert_eq!(failed.thumb_state, ThumbState::Pending);
        assert!(failed.error.is_none());
        assert_eq!(state_of("e.3mf").thumb_state, ThumbState::Embedded);
    }

    #[test]
    fn requeue_missing_thumbs_only_touches_finished_assets_without_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["kept.stl", "lost.stl", "failed.stl", "nohash.stl"] {
            fs::write(dir.path().join(name), name).unwrap();
        }
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let by_name = |n: &str| {
            cat.assets(&AssetQuery::default())
                .unwrap()
                .into_iter()
                .find(|a| a.rel_path == n)
                .unwrap()
        };
        let kept = by_name("kept.stl");
        let kept_hex = crate::paths::hex_sha256(&cat.hash_asset(&kept).unwrap());
        cat.hash_asset(&by_name("lost.stl")).unwrap();
        cat.set_thumb_state(kept.id, ThumbState::Ready, None)
            .unwrap();
        cat.set_thumb_state(by_name("lost.stl").id, ThumbState::Embedded, None)
            .unwrap();
        cat.set_thumb_state(by_name("failed.stl").id, ThumbState::Failed, Some("x"))
            .unwrap();
        cat.set_thumb_state(by_name("nohash.stl").id, ThumbState::Ready, None)
            .unwrap();

        let requeued = cat.requeue_missing_thumbs(|hex| hex == kept_hex).unwrap();
        assert_eq!(requeued, 2);
        assert_eq!(by_name("kept.stl").thumb_state, ThumbState::Ready);
        assert_eq!(by_name("lost.stl").thumb_state, ThumbState::Pending);
        assert_eq!(by_name("nohash.stl").thumb_state, ThumbState::Pending);
        assert_eq!(by_name("failed.stl").thumb_state, ThumbState::Failed);
        assert_eq!(cat.requeue_missing_thumbs(|_| true).unwrap(), 0);
    }

    #[test]
    fn search_escapes_like_wildcards_and_filters_by_library() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("100%.stl"), b"solid x\nendsolid x\n").unwrap();
        fs::write(dir.path().join("other.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let found = cat
            .assets(&AssetQuery {
                search: Some("100%".into()),
                library_id: Some(lib.id),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name(), "100%.stl");
        let limited = cat
            .assets(&AssetQuery {
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(limited.len(), 1);
    }

    fn names_of(assets: &[Asset]) -> Vec<&str> {
        assets.iter().map(|a| a.name()).collect()
    }

    fn write_mesh(path: &std::path::Path) {
        fs::write(path, b"solid x\nendsolid x\n").unwrap();
    }

    fn set_mtime(path: &std::path::Path, secs: u64) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn assets_sort_by_name_filename_not_rel_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("a")).unwrap();
        write_mesh(&root.join("b.stl"));
        write_mesh(&root.join("a/z.stl"));
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(root).unwrap();
        cat.scan_library(&lib).unwrap();

        let asc = cat
            .assets(&AssetQuery {
                sort: AssetSort::NameAsc,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(names_of(&asc), ["b.stl", "z.stl"]);

        let desc = cat
            .assets(&AssetQuery {
                sort: AssetSort::NameDesc,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(names_of(&desc), ["z.stl", "b.stl"]);
    }

    #[test]
    fn assets_sort_by_mtime_size_triangles_and_format() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_mesh(&root.join("old.stl"));
        write_mesh(&root.join("new.stl"));
        fs::write(root.join("mid.obj"), b"v 0 0 0\n").unwrap();
        fs::write(root.join("plate.3mf"), b"pk").unwrap();
        set_mtime(&root.join("old.stl"), 100);
        set_mtime(&root.join("new.stl"), 300);
        set_mtime(&root.join("mid.obj"), 200);
        set_mtime(&root.join("plate.3mf"), 250);
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(root).unwrap();
        cat.scan_library(&lib).unwrap();

        let by_id = |name: &str| {
            cat.assets(&AssetQuery::default())
                .unwrap()
                .into_iter()
                .find(|a| a.name() == name)
                .unwrap()
                .id
        };
        let small = BBox {
            min: [0.0, 0.0, 0.0],
            max: [10.0, 10.0, 10.0],
        };
        let large = BBox {
            min: [0.0, 0.0, 0.0],
            max: [50.0, 10.0, 10.0],
        };
        cat.set_mesh_meta(by_id("old.stl"), &[0u8; 32], 10, &small)
            .unwrap();
        cat.set_mesh_meta(by_id("new.stl"), &[1u8; 32], 100, &large)
            .unwrap();
        cat.set_mesh_meta(by_id("mid.obj"), &[2u8; 32], 50, &small)
            .unwrap();

        let newest = cat
            .assets(&AssetQuery {
                sort: AssetSort::ModifiedDesc,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            names_of(&newest),
            ["new.stl", "plate.3mf", "mid.obj", "old.stl"]
        );

        let oldest = cat
            .assets(&AssetQuery {
                sort: AssetSort::ModifiedAsc,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            names_of(&oldest),
            ["old.stl", "mid.obj", "plate.3mf", "new.stl"]
        );

        let largest = cat
            .assets(&AssetQuery {
                sort: AssetSort::SizeDesc,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(largest[0].name(), "new.stl");
        assert_eq!(largest.last().unwrap().name(), "plate.3mf");

        let smallest = cat
            .assets(&AssetQuery {
                sort: AssetSort::SizeAsc,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(smallest.last().unwrap().name(), "plate.3mf");
        assert!(smallest[0].name() == "old.stl" || smallest[0].name() == "mid.obj");

        let most = cat
            .assets(&AssetQuery {
                sort: AssetSort::TrianglesDesc,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(names_of(&most)[..3], ["new.stl", "mid.obj", "old.stl"]);
        assert_eq!(most.last().unwrap().name(), "plate.3mf");

        let fewest = cat
            .assets(&AssetQuery {
                sort: AssetSort::TrianglesAsc,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(fewest[0].name(), "old.stl");
        assert_eq!(fewest.last().unwrap().name(), "plate.3mf");

        let format = cat
            .assets(&AssetQuery {
                sort: AssetSort::Format,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            format.iter().map(|a| a.format).collect::<Vec<_>>(),
            vec![
                AssetFormat::ThreeMf,
                AssetFormat::Obj,
                AssetFormat::Stl,
                AssetFormat::Stl
            ]
        );
    }

    #[test]
    fn asset_sort_parse_roundtrip() {
        for sort in AssetSort::ALL {
            assert_eq!(AssetSort::parse(sort.as_str()), sort);
        }
        assert_eq!(AssetSort::parse("modified-desc"), AssetSort::ModifiedDesc);
        assert_eq!(AssetSort::parse("nope"), AssetSort::NameAsc);
        assert_eq!(AssetSort::parse("  SIZE  "), AssetSort::SizeDesc);
    }

    #[test]
    fn set_tags_ignores_blanks_and_updates_counts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.stl"), b"solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let id = cat.assets(&AssetQuery::default()).unwrap()[0].id;
        cat.set_tags(id, &["  ".into(), "boat".into(), "boat".into()])
            .unwrap();
        let tags = cat.all_tags().unwrap();
        assert_eq!(tags, vec![("boat".into(), 1)]);
        cat.set_tags(id, &[]).unwrap();
        let tags = cat.all_tags().unwrap();
        assert_eq!(tags, vec![("boat".into(), 0)]);
    }

    #[test]
    fn thumb_state_parse_roundtrip() {
        for state in [
            ThumbState::Pending,
            ThumbState::Ready,
            ThumbState::Failed,
            ThumbState::Embedded,
        ] {
            assert_eq!(ThumbState::parse(state.as_str()), state);
        }
        assert_eq!(ThumbState::parse("unknown"), ThumbState::Pending);
    }

    /// Library with the given files (relative paths); returns catalog and
    /// a name → id lookup of its assets.
    fn catalog_with(root: &Path, files: &[&str]) -> (Catalog, Library, HashMap<String, i64>) {
        for rel in files {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_mesh(&path);
        }
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(root).unwrap();
        cat.scan_library(&lib).unwrap();
        let ids = cat
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .map(|a| (a.rel_path, a.id))
            .collect();
        (cat, lib, ids)
    }

    fn rel_paths_of(cat: &Catalog, query: AssetQuery) -> Vec<String> {
        let mut out: Vec<String> = cat
            .assets(&query)
            .unwrap()
            .into_iter()
            .map(|a| a.rel_path)
            .collect();
        out.sort();
        out
    }

    #[test]
    fn dir_filter_matches_folder_and_subfolders_only() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, lib, _) = catalog_with(
            dir.path(),
            &[
                "top.stl",
                "a/b/one.stl",
                "a/b/deep/two.stl",
                "a/bc/three.stl",
                "a/b_x/four.stl",
            ],
        );
        let in_dir = |d: &str| {
            rel_paths_of(
                &cat,
                AssetQuery {
                    library_id: Some(lib.id),
                    dir: Some(d.into()),
                    ..Default::default()
                },
            )
        };
        assert_eq!(in_dir("a/b"), ["a/b/deep/two.stl", "a/b/one.stl"]);
        assert_eq!(in_dir("a").len(), 4);
        // `_` is a LIKE wildcard; it must match literally.
        assert_eq!(in_dir("a/b_x"), ["a/b_x/four.stl"]);
        assert_eq!(in_dir("").len(), 5);
    }

    #[test]
    fn tags_filter_all_any_and_untagged() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, _, ids) = catalog_with(dir.path(), &["a.stl", "b.stl", "c.stl"]);
        cat.set_tags(ids["a.stl"], &["red".into(), "big".into()])
            .unwrap();
        cat.set_tags(ids["b.stl"], &["red".into()]).unwrap();
        let tagged = |tags: &[&str], tag_mode| {
            rel_paths_of(
                &cat,
                AssetQuery {
                    tags: tags.iter().map(|t| t.to_string()).collect(),
                    tag_mode,
                    ..Default::default()
                },
            )
        };
        assert_eq!(tagged(&["red", "big"], TagMode::All), ["a.stl"]);
        assert_eq!(tagged(&["red", "big"], TagMode::Any), ["a.stl", "b.stl"]);
        assert_eq!(tagged(&["big", "big"], TagMode::All), ["a.stl"]);
        let untagged = rel_paths_of(
            &cat,
            AssetQuery {
                untagged: true,
                ..Default::default()
            },
        );
        assert_eq!(untagged, ["c.stl"]);
    }

    #[test]
    fn format_size_and_date_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_mesh(&root.join("a.stl"));
        fs::write(root.join("b.obj"), vec![b'v'; 4096]).unwrap();
        fs::write(root.join("c.3mf"), b"pk").unwrap();
        set_mtime(&root.join("a.stl"), 100);
        set_mtime(&root.join("b.obj"), 300);
        set_mtime(&root.join("c.3mf"), 500);
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(root).unwrap();
        cat.scan_library(&lib).unwrap();

        let formats = rel_paths_of(
            &cat,
            AssetQuery {
                formats: vec![AssetFormat::Stl, AssetFormat::ThreeMf],
                ..Default::default()
            },
        );
        assert_eq!(formats, ["a.stl", "c.3mf"]);

        let small = rel_paths_of(
            &cat,
            AssetQuery {
                file_size: Some((0, Some(1024))),
                ..Default::default()
            },
        );
        assert_eq!(small, ["a.stl", "c.3mf"]);
        let large = rel_paths_of(
            &cat,
            AssetQuery {
                file_size: Some((1024, None)),
                ..Default::default()
            },
        );
        assert_eq!(large, ["b.obj"]);

        let recent = rel_paths_of(
            &cat,
            AssetQuery {
                modified_since_ns: Some(300 * 1_000_000_000),
                ..Default::default()
            },
        );
        assert_eq!(recent, ["b.obj", "c.3mf"]);
    }

    #[test]
    fn fits_bed_allows_rotation_and_skips_unmeasured() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, _, ids) = catalog_with(
            dir.path(),
            &["small.stl", "long.stl", "tall.stl", "wide.stl", "new.stl"],
        );
        let size = |x: f32, y: f32, z: f32| BBox {
            min: [-1.0, -1.0, 0.0],
            max: [x - 1.0, y - 1.0, z],
        };
        cat.set_mesh_meta(ids["small.stl"], &[1], 12, &size(100.0, 100.0, 100.0))
            .unwrap();
        // 250 × 150 only fits a 180 × 256 bed turned 90°.
        cat.set_mesh_meta(ids["long.stl"], &[2], 12, &size(250.0, 150.0, 50.0))
            .unwrap();
        cat.set_mesh_meta(ids["tall.stl"], &[3], 12, &size(50.0, 50.0, 300.0))
            .unwrap();
        cat.set_mesh_meta(ids["wide.stl"], &[4], 12, &size(260.0, 260.0, 10.0))
            .unwrap();
        let fits = rel_paths_of(
            &cat,
            AssetQuery {
                fits_bed: Some([180.0, 256.0, 256.0]),
                ..Default::default()
            },
        );
        assert_eq!(fits, ["long.stl", "small.stl"]);
    }

    #[test]
    fn duplicates_group_by_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (cat, _, _) = catalog_with(root, &["x/a.stl", "y/b.stl", "z/c.stl", "d.stl", "e.stl"]);
        fs::write(root.join("d.stl"), b"solid other\nendsolid other\n").unwrap();
        fs::write(root.join("e.stl"), b"solid other\nendsolid other\n").unwrap();
        fs::write(root.join("z/c.stl"), b"solid unique\nendsolid unique\n").unwrap();
        let lib = cat.libraries().unwrap().remove(0);
        cat.scan_library(&lib).unwrap();
        assert_eq!(cat.duplicate_count().unwrap(), 0, "nothing hashed yet");
        for asset in cat.assets(&AssetQuery::default()).unwrap() {
            cat.hash_asset(&asset).unwrap();
        }
        assert_eq!(cat.duplicate_count().unwrap(), 4);
        let a = cat
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .find(|a| a.rel_path == "x/a.stl")
            .unwrap();
        assert_eq!(cat.copies(a.content_sha256.as_deref().unwrap()).unwrap(), 2);
        assert_eq!(cat.copies(&[0; 32]).unwrap(), 0);

        let dups = cat
            .assets(&AssetQuery {
                duplicates_only: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(dups.len(), 4);
        assert!(dups.iter().all(|a| a.rel_path != "z/c.stl"));
        // Each group is contiguous.
        let hashes: Vec<_> = dups.iter().map(|a| a.content_sha256.clone()).collect();
        assert_eq!(hashes[0], hashes[1]);
        assert_eq!(hashes[2], hashes[3]);
        assert_ne!(hashes[1], hashes[2]);
    }

    #[test]
    fn unhashed_assets_pages_through_online_files_without_a_hash() {
        let online = tempfile::tempdir().unwrap();
        let offline = tempfile::tempdir().unwrap();
        let (cat, _, _) = catalog_with(online.path(), &["a.stl", "b.stl", "c.stl"]);
        write_mesh(&offline.path().join("d.stl"));
        let gone = cat.add_library(offline.path()).unwrap();
        cat.scan_library(&gone).unwrap();
        offline.close().unwrap();
        let b = cat
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .find(|a| a.rel_path == "b.stl")
            .unwrap();
        cat.hash_asset(&b).unwrap();

        let mut seen = Vec::new();
        let mut after = 0;
        while let Some(asset) = cat.unhashed_assets(after, 1).unwrap().pop() {
            assert!(asset.id > after);
            after = asset.id;
            seen.push(asset.rel_path);
        }
        seen.sort();
        assert_eq!(seen, ["a.stl", "c.stl"]);
    }

    #[test]
    fn hash_asset_skips_storing_once_a_scan_saw_the_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, lib, _) = catalog_with(dir.path(), &["a.stl"]);
        let first = |cat: &Catalog| cat.assets(&AssetQuery::default()).unwrap().remove(0);
        let before = first(&cat);
        assert!(before.unchanged_on_disk());

        fs::write(
            dir.path().join("a.stl"),
            b"solid changed\nendsolid changed\n",
        )
        .unwrap();
        assert!(!before.unchanged_on_disk());
        cat.scan_library(&lib).unwrap();
        // `before` describes the old file, so its hash can't go on the new row.
        cat.hash_asset(&before).unwrap();
        let after = first(&cat);
        assert_eq!(after.content_sha256, None);

        cat.hash_asset(&after).unwrap();
        assert!(first(&cat).content_sha256.is_some());
    }

    #[test]
    fn add_and_remove_tags_in_bulk_keep_search_in_sync() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, _, ids) = catalog_with(dir.path(), &["a.stl", "b.stl", "c.stl"]);
        cat.set_tags(ids["a.stl"], &["keep".into()]).unwrap();
        let both = [ids["a.stl"], ids["b.stl"]];
        cat.add_tags(&both, &["gift".into(), " ".into()]).unwrap();

        let search = |q: &str| {
            rel_paths_of(
                &cat,
                AssetQuery {
                    search: Some(q.into()),
                    ..Default::default()
                },
            )
        };
        assert_eq!(search("gift"), ["a.stl", "b.stl"]);
        let tags = cat.all_tags().unwrap();
        assert_eq!(tags, [("gift".to_string(), 2), ("keep".to_string(), 1)]);

        cat.remove_tag(&both, "gift").unwrap();
        assert!(search("gift").is_empty());
        // Other tags survive, and FTS still has them.
        assert_eq!(search("keep"), ["a.stl"]);
        assert_eq!(fts_rows(&cat), 3);
    }

    fn tags_of(cat: &Catalog, rel: &str) -> Vec<String> {
        cat.assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .find(|a| a.rel_path == rel)
            .map(|a| a.tags)
            .unwrap_or_default()
    }

    #[test]
    fn rename_and_move_keep_the_row_tags_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, lib, ids) = catalog_with(dir.path(), &["a.stl", "b.stl"]);
        let a = cat
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .find(|x| x.rel_path == "a.stl")
            .unwrap();
        let sha = cat.hash_asset(&a).unwrap();
        cat.set_thumb_state(a.id, ThumbState::Ready, None).unwrap();
        cat.set_tags(a.id, &["boat".into()]).unwrap();

        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::rename(dir.path().join("a.stl"), dir.path().join("sub/renamed.stl")).unwrap();
        let stats = cat.scan_library(&lib).unwrap();
        assert_eq!((stats.moved, stats.added, stats.removed), (1, 0, 0));

        let moved = cat
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .find(|x| x.rel_path == "sub/renamed.stl")
            .unwrap();
        assert_eq!(moved.id, ids["a.stl"]);
        assert_eq!(moved.tags, ["boat"]);
        assert_eq!(moved.content_sha256.as_deref(), Some(&sha[..]));
        assert_eq!(moved.thumb_state, ThumbState::Ready);
        assert_eq!(fts_rows(&cat), 2);
        let search = |q: &str| {
            rel_paths_of(
                &cat,
                AssetQuery {
                    search: Some(q.into()),
                    ..Default::default()
                },
            )
        };
        assert_eq!(search("renamed"), ["sub/renamed.stl"]);
        assert_eq!(search("boat"), ["sub/renamed.stl"]);
        assert!(cat
            .assets(&AssetQuery::default())
            .unwrap()
            .iter()
            .all(|x| x.rel_path != "a.stl"));
    }

    #[test]
    fn move_that_changed_mtime_is_matched_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.stl"), "solid a\nendsolid a\n").unwrap();
        fs::write(root.join("unhashed.stl"), "solid u\nendsolid u\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib = cat.add_library(root).unwrap();
        cat.scan_library(&lib).unwrap();
        let by_rel = |rel: &str| {
            cat.assets(&AssetQuery::default())
                .unwrap()
                .into_iter()
                .find(|x| x.rel_path == rel)
        };
        let a = by_rel("a.stl").unwrap();
        cat.hash_asset(&a).unwrap();
        cat.set_tags(a.id, &["kept".into()]).unwrap();
        let u = by_rel("unhashed.stl").unwrap();
        cat.set_tags(u.id, &["lost".into()]).unwrap();

        // Copy-then-delete moves give the new file a new mtime.
        for (from, to, secs) in [("a.stl", "b.stl", 1_000), ("unhashed.stl", "v.stl", 2_000)] {
            fs::copy(root.join(from), root.join(to)).unwrap();
            set_mtime(&root.join(to), secs);
            fs::remove_file(root.join(from)).unwrap();
        }
        let stats = cat.scan_library(&lib).unwrap();
        assert_eq!((stats.moved, stats.added, stats.removed), (1, 1, 1));
        assert_eq!(by_rel("b.stl").unwrap().id, a.id);
        assert_eq!(tags_of(&cat, "b.stl"), ["kept"]);
        // No hash to confirm it by, and the mtime changed: a new asset.
        assert!(tags_of(&cat, "v.stl").is_empty());
    }

    #[test]
    fn identical_files_moved_together_keep_their_tags() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (cat, lib, _) = catalog_with(root, &["one.stl", "two.stl"]);
        for (rel, secs) in [("one.stl", 5_000), ("two.stl", 5_000)] {
            set_mtime(&root.join(rel), secs);
        }
        cat.scan_library(&lib).unwrap();
        for asset in cat.assets(&AssetQuery::default()).unwrap() {
            cat.hash_asset(&asset).unwrap();
            cat.set_tags(asset.id, &[asset.rel_path.replace(".stl", "")])
                .unwrap();
        }
        fs::create_dir(root.join("moved")).unwrap();
        for rel in ["one.stl", "two.stl"] {
            fs::rename(root.join(rel), root.join("moved").join(rel)).unwrap();
        }
        let stats = cat.scan_library(&lib).unwrap();
        // Same size and mtime: ambiguous, so the hash pairs them up.
        assert_eq!((stats.moved, stats.added, stats.removed), (2, 0, 0));
        let mut tags: Vec<String> = ["moved/one.stl", "moved/two.stl"]
            .into_iter()
            .flat_map(|rel| tags_of(&cat, rel))
            .collect();
        tags.sort();
        assert_eq!(tags, ["one", "two"]);
    }

    #[test]
    fn tags_follow_a_file_into_another_library_once() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fs::write(a.path().join("boat.stl"), "solid boat\nendsolid boat\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib_a = cat.add_library(a.path()).unwrap();
        let lib_b = cat.add_library(b.path()).unwrap();
        cat.scan_library(&lib_a).unwrap();
        let boat = cat.assets(&AssetQuery::default()).unwrap().pop().unwrap();
        cat.hash_asset(&boat).unwrap();
        cat.set_tags(boat.id, &["boat".into(), "校準".into()])
            .unwrap();

        fs::copy(a.path().join("boat.stl"), b.path().join("boat.stl")).unwrap();
        fs::copy(a.path().join("boat.stl"), b.path().join("copy.stl")).unwrap();
        fs::remove_file(a.path().join("boat.stl")).unwrap();
        assert_eq!(cat.scan_library(&lib_a).unwrap().removed, 1);
        cat.scan_library(&lib_b).unwrap();
        assert!(tags_of(&cat, "boat.stl").is_empty(), "not hashed yet");

        for asset in cat.assets(&AssetQuery::default()).unwrap() {
            cat.hash_asset(&asset).unwrap();
        }
        let tagged: Vec<Vec<String>> = ["boat.stl", "copy.stl"]
            .into_iter()
            .map(|rel| tags_of(&cat, rel))
            .collect();
        // Whichever copy was hashed first got the tags; the other did not.
        assert_eq!(tagged.iter().filter(|t| !t.is_empty()).count(), 1);
        assert!(tagged.contains(&vec!["boat".to_string(), "校準".to_string()]));
        assert_eq!(
            rel_paths_of(
                &cat,
                AssetQuery {
                    search: Some("校準".into()),
                    ..Default::default()
                }
            )
            .len(),
            1
        );
    }

    #[test]
    fn removed_library_gets_its_tags_back_when_added_again() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, lib, ids) = catalog_with(dir.path(), &["a.stl"]);
        let a = cat.assets(&AssetQuery::default()).unwrap().pop().unwrap();
        cat.hash_asset(&a).unwrap();
        cat.set_tags(ids["a.stl"], &["keep".into()]).unwrap();
        cat.remove_library(lib.id).unwrap();

        let lib = cat.add_library(dir.path()).unwrap();
        cat.scan_library(&lib).unwrap();
        let a = cat.assets(&AssetQuery::default()).unwrap().pop().unwrap();
        assert!(a.tags.is_empty());
        cat.hash_asset(&a).unwrap();
        assert_eq!(tags_of(&cat, "a.stl"), ["keep"]);
    }

    #[test]
    fn stashed_tags_never_overwrite_an_assets_own_tags() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fs::write(a.path().join("x.stl"), "solid x\nendsolid x\n").unwrap();
        fs::write(b.path().join("x.stl"), "solid x\nendsolid x\n").unwrap();
        let cat = Catalog::open_memory().unwrap();
        let lib_a = cat.add_library(a.path()).unwrap();
        let lib_b = cat.add_library(b.path()).unwrap();
        cat.scan_library(&lib_a).unwrap();
        cat.scan_library(&lib_b).unwrap();
        for asset in cat.assets(&AssetQuery::default()).unwrap() {
            let tag = if asset.library_id == lib_a.id {
                "old"
            } else {
                "mine"
            };
            cat.set_tags(asset.id, &[tag.into()]).unwrap();
            if asset.library_id == lib_a.id {
                cat.hash_asset(&asset).unwrap();
            }
        }
        fs::remove_file(a.path().join("x.stl")).unwrap();
        cat.scan_library(&lib_a).unwrap();
        let mine = cat.assets(&AssetQuery::default()).unwrap().pop().unwrap();
        cat.hash_asset(&mine).unwrap();
        assert_eq!(tags_of(&cat, "x.stl"), ["mine"]);
    }

    #[test]
    fn print_status_and_notes_filter_search_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, _lib, ids) = catalog_with(dir.path(), &["a.stl", "b.stl", "c.stl"]);
        let with_status = |filter| {
            rel_paths_of(
                &cat,
                AssetQuery {
                    print_status: Some(filter),
                    ..Default::default()
                },
            )
        };
        cat.set_status(&[ids["a.stl"], ids["b.stl"]], Some(PrintStatus::Todo))
            .unwrap();
        cat.set_status(&[ids["b.stl"], 9_999], Some(PrintStatus::Printed))
            .unwrap();
        cat.set_note(ids["c.stl"], "0.12mm 層高，支撐要開").unwrap();
        assert_eq!(with_status(StatusFilter::Is(PrintStatus::Todo)), ["a.stl"]);
        assert_eq!(
            with_status(StatusFilter::Is(PrintStatus::Printed)),
            ["b.stl"]
        );
        assert_eq!(with_status(StatusFilter::Unmarked), ["c.stl"]);

        let assets = cat.assets(&AssetQuery::default()).unwrap();
        let b = assets.iter().find(|a| a.rel_path == "b.stl").unwrap();
        assert_eq!(b.status, Some(PrintStatus::Printed));
        assert!(b.status_at.unwrap() > 0);
        let c = assets.iter().find(|a| a.rel_path == "c.stl").unwrap();
        assert_eq!((c.status, c.note.as_str()), (None, "0.12mm 層高，支撐要開"));

        let found = rel_paths_of(
            &cat,
            AssetQuery {
                search: Some("支撐".into()),
                ..Default::default()
            },
        );
        assert_eq!(found, ["c.stl"]);

        let rows = |cat: &Catalog| -> i64 {
            cat.conn
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM asset_notes", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(rows(&cat), 3);
        cat.set_status(&[ids["a.stl"]], None).unwrap();
        cat.set_note(ids["c.stl"], "  \n").unwrap();
        assert_eq!(rows(&cat), 1, "cleared rows are dropped");
        assert_eq!(with_status(StatusFilter::Unmarked), ["a.stl", "c.stl"]);
    }

    #[test]
    fn print_status_and_note_follow_moves() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (cat, lib_a, ids) = catalog_with(a.path(), &["x.stl", "y.stl"]);
        let lib_b = cat.add_library(b.path()).unwrap();
        cat.set_status(&[ids["x.stl"]], Some(PrintStatus::Failed))
            .unwrap();
        cat.set_note(ids["x.stl"], "warped").unwrap();
        let y = cat
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .find(|x| x.rel_path == "y.stl")
            .unwrap();
        cat.set_note(y.id, "elsewhere").unwrap();
        cat.hash_asset(&y).unwrap();

        // Renamed in place: same row.
        fs::rename(a.path().join("x.stl"), a.path().join("x2.stl")).unwrap();
        // Moved to another library: through the stash.
        fs::rename(a.path().join("y.stl"), b.path().join("y.stl")).unwrap();
        cat.scan_library(&lib_a).unwrap();
        cat.scan_library(&lib_b).unwrap();
        let find = |rel: &str| {
            cat.assets(&AssetQuery::default())
                .unwrap()
                .into_iter()
                .find(|x| x.rel_path == rel)
                .unwrap()
        };
        let x2 = find("x2.stl");
        assert_eq!(
            (x2.status, x2.note.as_str()),
            (Some(PrintStatus::Failed), "warped")
        );
        cat.hash_asset(&find("y.stl")).unwrap();
        assert_eq!(find("y.stl").note, "elsewhere");
    }

    #[test]
    fn print_info_is_read_once_per_3mf_and_again_after_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, lib, _) = catalog_with(dir.path(), &["a.3mf", "b.3mf", "c.stl"]);
        let pending = |cat: &Catalog| -> Vec<String> {
            let mut out: Vec<String> = cat
                .assets_without_print_info(0, 10)
                .unwrap()
                .into_iter()
                .map(|a| a.rel_path)
                .collect();
            out.sort();
            out
        };
        assert_eq!(pending(&cat), ["a.3mf", "b.3mf"]);
        let first = cat.assets_without_print_info(0, 1).unwrap().pop().unwrap();
        assert_eq!(
            cat.assets_without_print_info(first.id, 10).unwrap().len(),
            1
        );
        let a = cat
            .assets_without_print_info(0, 10)
            .unwrap()
            .into_iter()
            .find(|x| x.rel_path == "a.3mf")
            .unwrap();
        let info = PrintInfo {
            title: Some("Benchy".into()),
            materials: vec!["PLA".into(), "PETG".into()],
            print_time_s: Some(3_000),
            ..Default::default()
        };
        cat.set_print_info(&a, &info).unwrap();
        let b = cat.assets_without_print_info(0, 10).unwrap().pop().unwrap();
        assert_eq!(b.rel_path, "b.3mf");
        cat.set_print_info(&b, &PrintInfo::default()).unwrap();
        assert!(pending(&cat).is_empty(), "empty results count as read");
        let stored = cat
            .assets(&AssetQuery::default())
            .unwrap()
            .into_iter()
            .map(|x| (x.rel_path, x.print))
            .collect::<HashMap<_, _>>();
        assert_eq!(stored["a.3mf"].as_ref(), Some(&info));
        assert_eq!(stored["b.3mf"], Some(PrintInfo::default()));
        assert_eq!(stored["c.stl"], None);

        // A changed file is read again, and a result for the old contents
        // is not stored.
        fs::write(dir.path().join("a.3mf"), "changed").unwrap();
        set_mtime(&dir.path().join("a.3mf"), 7_000);
        cat.scan_library(&lib).unwrap();
        assert_eq!(pending(&cat), ["a.3mf"]);
        cat.set_print_info(&a, &info).unwrap();
        assert_eq!(pending(&cat), ["a.3mf"]);

        // Rows from an older reader version are read again.
        cat.conn
            .lock()
            .unwrap()
            .execute("UPDATE print_info SET version = 0", [])
            .unwrap();
        assert_eq!(pending(&cat), ["a.3mf", "b.3mf"]);
    }

    #[test]
    fn print_info_filters_by_material_and_time_and_is_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, _lib, _) = catalog_with(dir.path(), &["quick.3mf", "long.3mf", "plain.3mf"]);
        for asset in cat.assets_without_print_info(0, 10).unwrap() {
            let info = match asset.rel_path.as_str() {
                "quick.3mf" => PrintInfo {
                    printer: Some("Bambu Lab A1 mini 0.4 nozzle".into()),
                    materials: vec!["PLA".into()],
                    print_time_s: Some(1_800),
                    ..Default::default()
                },
                "long.3mf" => PrintInfo {
                    designer: Some("Ada".into()),
                    materials: vec!["PETG".into(), "PLA".into()],
                    print_time_s: Some(5 * 3_600),
                    ..Default::default()
                },
                _ => PrintInfo::default(),
            };
            cat.set_print_info(&asset, &info).unwrap();
        }
        let query = |q: AssetQuery| rel_paths_of(&cat, q);
        assert_eq!(
            query(AssetQuery {
                materials: vec!["PETG".into()],
                ..Default::default()
            }),
            ["long.3mf"]
        );
        assert_eq!(
            query(AssetQuery {
                materials: vec!["PLA".into(), "ABS".into()],
                ..Default::default()
            }),
            ["long.3mf", "quick.3mf"]
        );
        assert_eq!(
            query(AssetQuery {
                print_time: Some((0, Some(3_600))),
                ..Default::default()
            }),
            ["quick.3mf"]
        );
        assert_eq!(
            query(AssetQuery {
                print_time: Some((4 * 3_600, None)),
                ..Default::default()
            }),
            ["long.3mf"]
        );
        let search = |q: &str| {
            query(AssetQuery {
                search: Some(q.into()),
                ..Default::default()
            })
        };
        assert_eq!(search("a1 mini"), ["quick.3mf"]);
        assert_eq!(search("ada"), ["long.3mf"]);
        assert_eq!(search("petg"), ["long.3mf"]);
        assert_eq!(
            cat.all_materials().unwrap(),
            [("PLA".to_string(), 2), ("PETG".to_string(), 1)]
        );
    }

    #[test]
    fn saved_searches_roundtrip_sorted_by_name() {
        let cat = Catalog::open_memory().unwrap();
        let b = cat.add_saved_search(" b ", "{}").unwrap();
        let a = cat.add_saved_search("A", r#"{"search":"x"}"#).unwrap();
        let names = |cat: &Catalog| -> Vec<String> {
            cat.saved_searches()
                .unwrap()
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(names(&cat), ["A", "b"]);
        cat.rename_saved_search(b, "0 first").unwrap();
        cat.set_saved_search_query(a, "{}").unwrap();
        assert_eq!(names(&cat), ["0 first", "A"]);
        assert_eq!(cat.saved_searches().unwrap()[1].query, "{}");
        cat.remove_saved_search(b).unwrap();
        assert_eq!(names(&cat), ["A"]);
    }

    #[test]
    fn count_matches_assets_without_loading_them() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, _lib, ids) = catalog_with(dir.path(), &["a.stl", "b.stl", "sub/c.obj"]);
        cat.set_status(&[ids["a.stl"]], Some(PrintStatus::Todo))
            .unwrap();
        for query in [
            AssetQuery::default(),
            AssetQuery {
                formats: vec![AssetFormat::Stl],
                ..Default::default()
            },
            AssetQuery {
                print_status: Some(StatusFilter::Unmarked),
                search: Some("c".into()),
                ..Default::default()
            },
            AssetQuery {
                dir: Some("sub".into()),
                online_only: true,
                limit: Some(1),
                ..Default::default()
            },
        ] {
            let unlimited = AssetQuery {
                limit: None,
                ..query.clone()
            };
            assert_eq!(
                cat.count(&query).unwrap() as usize,
                cat.assets(&unlimited).unwrap().len(),
                "{query:?}"
            );
        }
    }

    #[test]
    fn plan_moves_pairs_unique_matches_and_confirms_the_rest_by_hash() {
        let gone = |id, size, mtime, sha: Option<u8>| GoneFile {
            id,
            format: "stl".into(),
            size,
            mtime,
            sha: sha.map(|b| vec![b; 32]),
        };
        let added = |id, size, mtime| ScannedFile {
            id,
            rel: format!("{id}.stl"),
            format: "stl".into(),
            size,
            mtime,
        };
        let mut hashed = Vec::new();
        let moves = plan_moves(
            &[
                gone(1, 10, 100, None),
                gone(2, 20, 200, Some(7)),
                gone(3, 30, 300, None),
            ],
            &[
                added(11, 10, 100),
                added(12, 20, 999),
                added(13, 30, 301),
                added(14, 20, 998),
            ],
            |a| {
                hashed.push(a.id);
                Some([if a.id == 12 { 7 } else { 0 }; 32])
            },
        );
        let mut moves = moves;
        moves.sort();
        assert_eq!(moves, [(1, 11), (2, 12)]);
        // Only same-size candidates of a gone file that had a hash.
        hashed.sort();
        assert!(hashed.iter().all(|id| [12, 14].contains(id)));

        // A format change is never a move.
        let mut other = added(21, 10, 100);
        other.format = "obj".into();
        assert!(plan_moves(&[gone(1, 10, 100, Some(1))], &[other], |_| Some([1; 32])).is_empty());
    }

    #[test]
    fn rel_paths_lists_one_library() {
        let dir = tempfile::tempdir().unwrap();
        let (cat, lib, _) = catalog_with(dir.path(), &["a.stl", "sub/b.stl"]);
        let mut paths = cat.rel_paths(lib.id).unwrap();
        paths.sort();
        assert_eq!(paths, ["a.stl", "sub/b.stl"]);
        assert!(cat.rel_paths(lib.id + 1).unwrap().is_empty());
    }
}
