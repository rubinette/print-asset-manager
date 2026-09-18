use std::path::PathBuf;

use directories::ProjectDirs;

const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "print-asset-manager";
const APPLICATION: &str = "print-asset-manager";

fn dirs() -> ProjectDirs {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).expect("home directory is required")
}

pub fn data_dir() -> PathBuf {
    let dir = dirs().data_dir().to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn cache_dir() -> PathBuf {
    let dir = dirs().cache_dir().to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn catalog_db_path() -> PathBuf {
    data_dir().join("catalog.sqlite")
}

pub fn thumbs_dir() -> PathBuf {
    let dir = cache_dir().join("thumbs");
    let _ = std::fs::create_dir_all(&dir);
    dir
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
