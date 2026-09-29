use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use walkdir::WalkDir;

use crate::error::Result;
use crate::hash::file_sha256;
use crate::mesh::{AssetFormat, BBox};

/// Files written per scan transaction.
const SCAN_CHUNK: usize = 500;

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
    /// Only assets whose content hash is shared with another asset.
    pub duplicates_only: bool,
    pub search: Option<String>,
    pub thumb_state: Option<ThumbState>,
    /// Skip libraries whose root is missing (e.g. NAS offline).
    pub online_only: bool,
    pub limit: Option<usize>,
    pub sort: AssetSort,
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    pub skipped: u32,
}

pub struct Catalog {
    conn: Mutex<Connection>,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
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

    /// Forget a library and its assets. Files on disk are untouched.
    pub fn remove_library(&self, id: i64) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
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
                for (rel, format, size, mtime) in &entries {
                    upsert_asset(&tx, library.id, rel, *format, *size, *mtime, &mut stats)?;
                }
                tx.commit()?;
            }
            done += chunk.len() as u32;
            on_progress(done, total);
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare("SELECT id, rel_path FROM assets WHERE library_id = ?1")?;
        let stale: Vec<i64> = stmt
            .query_map(params![library.id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .filter(|(_, rel)| !seen.contains(rel))
            .map(|(id, _)| id)
            .collect();
        drop(stmt);
        for id in stale {
            let _ = tx.execute("DELETE FROM assets_fts WHERE rowid = ?1", params![id]);
            tx.execute("DELETE FROM assets WHERE id = ?1", params![id])?;
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
        let mut sql = String::from(
            "SELECT a.id, a.library_id, l.root_path, a.rel_path, a.format, a.size_bytes, a.mtime_ns,
                    a.content_sha256, a.triangle_count,
                    a.bbox_min_x, a.bbox_min_y, a.bbox_min_z,
                    a.bbox_max_x, a.bbox_max_y, a.bbox_max_z,
                    a.thumb_state, a.error
             FROM assets a
             JOIN libraries l ON l.id = a.library_id
             WHERE 1=1",
        );
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
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
                        SELECT rowid FROM assets_fts WHERE assets_fts MATCH ?
                      ))",
                );
                args.push(like.clone().into());
                args.push(like.into());
                args.push(fts_query(&search).into());
            }
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
            let id: i64 = r.get(0)?;
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
            let format = AssetFormat::from_str(&r.get::<_, String>(4)?).unwrap_or(AssetFormat::Stl);
            out.push(Asset {
                id,
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
            });
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

    pub fn hash_asset(&self, asset: &Asset) -> Result<Vec<u8>> {
        let sha = file_sha256(&asset.abs_path())?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE assets SET content_sha256=?1 WHERE id=?2",
            params![&sha[..], asset.id],
        )?;
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
) -> Result<()> {
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
        }
    }
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
