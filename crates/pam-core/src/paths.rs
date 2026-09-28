use std::path::PathBuf;
use std::sync::OnceLock;

use directories::ProjectDirs;

const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "print-asset-manager";
const APPLICATION: &str = "print-asset-manager";

fn dirs() -> ProjectDirs {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).expect("home directory is required")
}

/// Resolve `dir` once and create it; later calls reuse the cached path.
fn ensured(cell: &'static OnceLock<PathBuf>, dir: impl FnOnce() -> PathBuf) -> PathBuf {
    cell.get_or_init(|| {
        let dir = dir();
        let _ = std::fs::create_dir_all(&dir);
        dir
    })
    .clone()
}

pub fn data_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    ensured(&DIR, || dirs().data_dir().to_path_buf())
}

pub fn cache_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    ensured(&DIR, || dirs().cache_dir().to_path_buf())
}

pub fn catalog_db_path() -> PathBuf {
    data_dir().join("catalog.sqlite")
}

pub fn thumbs_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    ensured(&DIR, || cache_dir().join("thumbs"))
}

pub fn thumb_path(sha256_hex: &str) -> PathBuf {
    thumbs_dir().join(format!("{sha256_hex}.png"))
}

pub fn preview_frame_path() -> PathBuf {
    cache_dir().join("preview.png")
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_sha256_encodes_bytes() {
        assert_eq!(hex_sha256(&[0x0a, 0xff]), "0aff");
        assert_eq!(hex_sha256(&[]), "");
    }

    #[test]
    fn thumb_and_preview_paths_live_under_cache() {
        let thumb = thumb_path("abc123");
        assert_eq!(thumb.file_name().unwrap(), "abc123.png");
        assert!(thumb.starts_with(thumbs_dir()));
        assert_eq!(preview_frame_path().file_name().unwrap(), "preview.png");
        assert!(catalog_db_path().ends_with("catalog.sqlite"));
    }
}
