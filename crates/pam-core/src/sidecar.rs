//! Images and documents next to a model file: photos of the print, README,
//! license or assembly notes. Downloads from model sites often unpack to
//! `<thing>/files/*.stl` beside `<thing>/images/` and `<thing>/README.txt`,
//! so a model in a `files` folder also gets what sits one level up.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::mesh::AssetFormat;

/// At most this many images and documents are returned.
pub const MAX_IMAGES: usize = 10;
pub const MAX_DOCS: usize = 8;

/// A folder with more models than this is a shared folder: of its loose
/// images and documents, only those named after the model belong to it.
const SHARED_FOLDER_MODELS: usize = 4;

/// Subfolders that hold a model's pictures.
const IMAGE_DIRS: [&str; 9] = [
    "images", "image", "imgs", "img", "photos", "photo", "pictures", "pics", "renders",
];
/// Folder names that model sites put the printable files in.
const MODEL_DIRS: [&str; 5] = ["files", "models", "stl", "stls", "3mf"];

const IMAGE_EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
const TEXT_EXTS: [&str; 3] = ["txt", "md", "markdown"];
const DOC_EXTS: [&str; 1] = ["pdf"];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sidecars {
    /// Images, those named after the model first.
    pub images: Vec<PathBuf>,
    /// Documents, those named after the model first, then READMEs, licenses last.
    pub docs: Vec<PathBuf>,
    /// The document worth previewing: the first text one that isn't a license.
    pub readme: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Text,
    Doc,
}

fn kind_of(path: &Path) -> Option<Kind> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if name.starts_with('.') {
        return None;
    }
    let ext = match name.rsplit_once('.') {
        Some((_, ext)) => ext,
        // "README", "LICENSE" without an extension.
        None if is_readme(&name) || is_license(&name) => return Some(Kind::Text),
        None => return None,
    };
    if IMAGE_EXTS.contains(&ext) {
        Some(Kind::Image)
    } else if TEXT_EXTS.contains(&ext) {
        Some(Kind::Text)
    } else if DOC_EXTS.contains(&ext) {
        Some(Kind::Doc)
    } else {
        None
    }
}

fn is_readme(name: &str) -> bool {
    name.starts_with("readme")
}

fn is_license(name: &str) -> bool {
    name.starts_with("license") || name.starts_with("licence") || name.starts_with("copying")
}

fn lower_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

fn lower_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

/// Whether a file is named after the model: "benchy.stl" ↔ "benchy-photo.jpg"
/// or "benchy_v2.stl" ↔ "benchy.jpg". Very short stems match nothing.
fn named_after(model_stem: &str, other_stem: &str) -> bool {
    let shorter = model_stem.chars().count().min(other_stem.chars().count());
    shorter >= 3 && (other_stem.starts_with(model_stem) || model_stem.starts_with(other_stem))
}

/// Files directly in `dir` (not hidden), sorted by name; empty if unreadable.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .collect();
    files.sort_by_key(|p| lower_name(p));
    files
}

/// `dir`'s subfolders that hold images, by name.
fn image_dirs_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| IMAGE_DIRS.contains(&lower_name(p).as_str()))
        .collect();
    dirs.sort();
    dirs
}

/// Images and documents that belong with `model`. Reads a few directories;
/// call it off the UI thread.
pub fn find_sidecars(model: &Path) -> Sidecars {
    let Some(dir) = model.parent() else {
        return Sidecars::default();
    };
    let stem = lower_stem(model);
    // (file, kind, named after the model)
    let mut found: Vec<(PathBuf, Kind, bool)> = Vec::new();
    let mut add = |path: PathBuf, only_named: bool, images_only: bool| {
        let Some(kind) = kind_of(&path) else {
            return;
        };
        if images_only && kind != Kind::Image {
            return;
        }
        let named = named_after(&stem, &lower_stem(&path));
        if (named || !only_named) && !found.iter().any(|(p, ..)| *p == path) {
            found.push((path, kind, named));
        }
    };

    let files = files_in(dir);
    let models = files
        .iter()
        .filter(|p| AssetFormat::from_path(p).is_some())
        .count();
    let shared = models > SHARED_FOLDER_MODELS;
    for file in files {
        add(file, shared, false);
    }
    for sub in image_dirs_in(dir) {
        for file in files_in(&sub) {
            add(file, shared, true);
        }
    }
    let in_model_dir = MODEL_DIRS.contains(&lower_name(dir).as_str());
    if let Some(thing) = dir.parent().filter(|_| in_model_dir) {
        for file in files_in(thing) {
            add(file, false, false);
        }
        for sub in image_dirs_in(thing) {
            for file in files_in(&sub) {
                add(file, false, true);
            }
        }
    }

    // Stable sort keeps folder order within each rank.
    found.sort_by_key(|(path, _, named)| {
        let name = lower_name(path);
        if *named {
            0
        } else if is_readme(&name) {
            1
        } else if is_license(&name) {
            3
        } else {
            2
        }
    });
    let mut out = Sidecars::default();
    for (path, kind, _) in found {
        match kind {
            Kind::Image if out.images.len() < MAX_IMAGES => out.images.push(path),
            Kind::Text | Kind::Doc if out.docs.len() < MAX_DOCS => {
                if kind == Kind::Text && out.readme.is_none() && !is_license(&lower_name(&path)) {
                    out.readme = Some(path.clone());
                }
                out.docs.push(path);
            }
            _ => {}
        }
    }
    out
}

/// The start of a text file, at most `max_bytes`, cut at a line break when
/// there is one. `None` if it can't be read or is empty.
pub fn read_text_preview(path: &Path, max_bytes: usize) -> Option<String> {
    let mut bytes = Vec::with_capacity(max_bytes);
    fs::File::open(path)
        .ok()?
        .take(max_bytes as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = bytes.len() == max_bytes;
    let mut text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    if truncated {
        if let Some(end) = text.rfind('\n') {
            text.truncate(end);
        }
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"x").unwrap();
        path
    }

    fn names(paths: &[PathBuf], root: &Path) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn model_site_download_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let model = touch(root, "Benchy - 763622/files/hull.stl");
        touch(root, "Benchy - 763622/files/mast.stl");
        touch(root, "Benchy - 763622/images/b_photo.jpg");
        touch(root, "Benchy - 763622/images/a_render.png");
        touch(root, "Benchy - 763622/images/notes.txt");
        touch(root, "Benchy - 763622/LICENSE.txt");
        touch(root, "Benchy - 763622/README.txt");
        touch(root, "Benchy - 763622/.DS_Store");
        touch(root, "Benchy - 763622/files/._hull.png");

        let found = find_sidecars(&model);
        let base = root.join("Benchy - 763622");
        assert_eq!(
            names(&found.images, &base),
            ["images/a_render.png", "images/b_photo.jpg"]
        );
        // Image folders contribute only images; README before LICENSE.
        assert_eq!(names(&found.docs, &base), ["README.txt", "LICENSE.txt"]);
        assert_eq!(found.readme, Some(base.join("README.txt")));
    }

    #[test]
    fn shared_folder_keeps_only_files_named_after_the_model() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for name in ["a.stl", "b.stl", "c.stl", "d.stl", "e.3mf"] {
            touch(root, name);
        }
        let model = touch(root, "benchy_v2.stl");
        touch(root, "benchy.jpg");
        touch(root, "benchy_v2 print.png");
        touch(root, "cat.png");
        touch(root, "photos/benchy_v2-top.webp");
        touch(root, "photos/dog.webp");
        touch(root, "benchy.md");
        touch(root, "README.md");

        let found = find_sidecars(&model);
        assert_eq!(
            names(&found.images, root),
            [
                "benchy.jpg",
                "benchy_v2 print.png",
                "photos/benchy_v2-top.webp"
            ]
        );
        assert_eq!(names(&found.docs, root), ["benchy.md"]);
        assert_eq!(found.readme, Some(root.join("benchy.md")));
    }

    #[test]
    fn small_folder_shows_everything_named_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let model = touch(root, "clip.3mf");
        touch(root, "clip-base.stl");
        touch(root, "aaa.png");
        touch(root, "clip.jpg");
        touch(root, "Assembly.pdf");
        touch(root, "LICENSE");
        touch(root, "thumbs/x.png");

        let found = find_sidecars(&model);
        assert_eq!(names(&found.images, root), ["clip.jpg", "aaa.png"]);
        assert_eq!(names(&found.docs, root), ["Assembly.pdf", "LICENSE"]);
        // A license isn't worth previewing; a PDF can't be.
        assert_eq!(found.readme, None);
    }

    #[test]
    fn caps_and_missing_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let model = touch(root, "m.stl");
        for i in 0..MAX_IMAGES + 5 {
            touch(root, &format!("img{i:02}.png"));
        }
        assert_eq!(find_sidecars(&model).images.len(), MAX_IMAGES);
        assert_eq!(find_sidecars(&root.join("gone/m.stl")), Sidecars::default());
    }

    #[test]
    fn text_preview_cuts_at_a_line_break() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("README.md");
        fs::write(&path, "# 小船\r\n\r\n先印船身。\n再印桅杆。\n").unwrap();
        assert_eq!(
            read_text_preview(&path, 1024).as_deref(),
            Some("# 小船\n\n先印船身。\n再印桅杆。")
        );
        // Cut mid-line (and mid-character): keep whole lines only.
        let short = read_text_preview(&path, 29).unwrap();
        assert_eq!(short, "# 小船\n\n先印船身。");
        fs::write(&path, "  \n").unwrap();
        assert_eq!(read_text_preview(&path, 1024), None);
        assert_eq!(read_text_preview(&tmp.path().join("nope.txt"), 1024), None);
    }
}
