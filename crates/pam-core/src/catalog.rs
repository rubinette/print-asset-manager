use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use walkdir::WalkDir;

use crate::error::Result;
use crate::hash::file_sha256;
use crate::mesh::{AssetFormat, BBox};

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;

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

#[derive(Clone, Debug, Default)]
pub struct AssetQuery {
    pub library_id: Option<i64>,
    pub tag: Option<String>,
    pub search: Option<String>,
    pub thumb_state: Option<ThumbState>,
    pub limit: Option<usize>,
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

    pub fn remove_library(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM libraries WHERE id = ?1", params![id])?;
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
        for (i, (path, format)) in files.into_iter().enumerate() {
            let rel = match path.strip_prefix(&library.root_path) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            seen.insert(rel.clone());
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    stats.skipped += 1;
                    on_progress(i as u32 + 1, total);
                    continue;
                }
            };
            let size = meta.len() as i64;
            let mtime = mtime_ns(&meta);
            self.upsert_asset(library.id, &rel, format, size, mtime, &mut stats)?;
            on_progress(i as u32 + 1, total);
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, rel_path FROM assets WHERE library_id = ?1")?;
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
            let _ = conn.execute("DELETE FROM assets_fts WHERE rowid = ?1", params![id]);
            conn.execute("DELETE FROM assets WHERE id = ?1", params![id])?;
            stats.removed += 1;
        }
        Ok(stats)
    }

    fn upsert_asset(
        &self,
        library_id: i64,
        rel: &str,
        format: AssetFormat,
        size: i64,
        mtime: i64,
        stats: &mut ScanStats,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
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

    pub fn assets(&self, query: &AssetQuery) -> Result<Vec<Asset>> {
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
        if let Some(tag) = &query.tag {
            sql.push_str(
                " AND a.id IN (SELECT asset_id FROM asset_tags at JOIN tags t ON t.id = at.tag_id WHERE t.name = ?)",
            );
            args.push(tag.clone().into());
        }
        if let Some(search) = query.search.as_ref().map(|s| s.trim().to_string()) {
            if !search.is_empty() {
                let like = format!("%{}%", escape_like(&search));
                sql.push_str(
                    " AND (a.rel_path LIKE ? ESCAPE '\\' OR a.id IN (
                        SELECT at.asset_id FROM asset_tags at
                        JOIN tags t ON t.id = at.tag_id
                        WHERE t.name LIKE ? ESCAPE '\\'
                      ) OR CAST(a.id AS TEXT) IN (
                        SELECT CAST(rowid AS TEXT) FROM assets_fts WHERE assets_fts MATCH ?
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
        sql.push_str(" ORDER BY a.rel_path COLLATE NOCASE");
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

        for asset in &mut out {
            let mut tag_stmt =
                conn.prepare("SELECT t.name FROM tags t JOIN asset_tags at ON at.tag_id = t.id WHERE at.asset_id = ?1 ORDER BY t.name")?;
            asset.tags = tag_stmt
                .query_map(params![asset.id], |r| r.get(0))?
                .filter_map(|r| r.ok())
                .collect();
        }
        Ok(out)
    }

    pub fn pending_thumbs(&self, limit: usize) -> Result<Vec<Asset>> {
        self.assets(&AssetQuery {
            thumb_state: Some(ThumbState::Pending),
            limit: Some(limit),
            ..Default::default()
        })
    }

    pub fn pending_count(&self) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM assets WHERE thumb_state = 'pending'",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u32)
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM asset_tags WHERE asset_id = ?1",
            params![asset_id],
        )?;
        for raw in tags {
            let name = raw.trim();
            if name.is_empty() {
                continue;
            }
            conn.execute(
                "INSERT OR IGNORE INTO tags (name) VALUES (?1)",
                params![name],
            )?;
            let tag_id: i64 =
                conn.query_row("SELECT id FROM tags WHERE name = ?1", params![name], |r| {
                    r.get(0)
                })?;
            conn.execute(
                "INSERT OR IGNORE INTO asset_tags (asset_id, tag_id) VALUES (?1, ?2)",
                params![asset_id, tag_id],
            )?;
        }
        let joined = tags
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let name: String = conn.query_row(
            "SELECT rel_path FROM assets WHERE id=?1",
            params![asset_id],
            |r| r.get(0),
        )?;
        let file_name = Path::new(&name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&name)
            .to_string();
        let _ = conn.execute("DELETE FROM assets_fts WHERE rowid=?1", params![asset_id]);
        let _ = conn.execute(
            "INSERT INTO assets_fts (rowid, name, rel_path, tags) VALUES (?1, ?2, ?3, ?4)",
            params![asset_id, file_name, name, joined],
        );
        Ok(())
    }
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
                tag: Some("校準".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(tagged.len(), 1);
        assert!(tagged[0].tags.contains(&"校準".into()));
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
}
