use std::sync::RwLock;

use pam_core::catalog::AssetSort;
use pam_core::paths::data_dir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    Chinese,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguagePref {
    System,
    English,
    Chinese,
}

impl LanguagePref {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::English => "en",
            Self::Chinese => "zh",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "en" | "en-us" | "en_us" | "english" => Self::English,
            "zh" | "zh-tw" | "zh_tw" | "zh-cn" | "zh_cn" | "zh-hk" | "zh_hk" | "chinese" => {
                Self::Chinese
            }
            _ => Self::System,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    AppTitle,
    Search,
    AddFolder,
    AddFolderEllipsis,
    Folders,
    FolderFallback,
    Tags,
    AllAssets,
    Open,
    EmptyTitle,
    EmptyBody,
    Scanning,
    Triangles,
    Size,
    Format,
    TagsHint,
    DragHint,
    Loading,
    Failed,
    Offline,
    NoSelection,
    EmptyFolder,
    Language,
    LanguageSystem,
    LanguageChinese,
    LanguageEnglish,
    Quit,
    Sort,
    SortNameAsc,
    SortNameDesc,
    SortModifiedDesc,
    SortModifiedAsc,
    SortSizeDesc,
    SortSizeAsc,
    SortTrianglesDesc,
    SortTrianglesAsc,
    SortFormat,
    ViewGrid,
    ViewList,
    ThumbSize,
    RevealInFileManager,
    CopyPath,
    RegenerateThumb,
    Name,
    Modified,
    MoveToTrashEllipsis,
    MoveToTrash,
    TrashDetail,
    TrashFailed,
    RemoveFolderEllipsis,
    RemoveFolder,
    RemoveFolderDetail,
    Cancel,
    Ok,
}

static PREF: RwLock<LanguagePref> = RwLock::new(LanguagePref::System);

pub fn init() {
    if let Ok(raw) = std::fs::read_to_string(pref_path()) {
        *PREF.write().expect("language pref lock") = LanguagePref::parse(&raw);
    }
    apply_kit_locale();
}

pub fn pref() -> LanguagePref {
    *PREF.read().expect("language pref lock")
}

pub fn current() -> Language {
    match pref() {
        LanguagePref::English => Language::English,
        LanguagePref::Chinese => Language::Chinese,
        LanguagePref::System => system_language(),
    }
}

pub fn set_pref(next: LanguagePref, persist: bool) {
    *PREF.write().expect("language pref lock") = next;
    if persist {
        let _ = std::fs::write(pref_path(), next.as_str());
    }
    apply_kit_locale();
}

pub fn t(key: Key) -> &'static str {
    lookup(current(), key)
}

pub fn scan_progress(done: u32, total: u32) -> String {
    scan_progress_for(current(), done, total)
}

pub fn thumb_progress(done: u32, total: u32) -> String {
    thumb_progress_for(current(), done, total)
}

pub fn asset_count(n: usize) -> String {
    asset_count_for(current(), n)
}

pub fn trash_confirm(name: &str) -> String {
    trash_confirm_for(current(), name)
}

pub fn remove_folder_confirm(name: &str) -> String {
    remove_folder_confirm_for(current(), name)
}

fn trash_confirm_for(language: Language, name: &str) -> String {
    match language {
        Language::Chinese => format!("要將「{name}」移到垃圾桶嗎？"),
        Language::English => format!("Move “{name}” to the Trash?"),
    }
}

fn remove_folder_confirm_for(language: Language, name: &str) -> String {
    match language {
        Language::Chinese => format!("要從素材庫移除「{name}」資料夾嗎？"),
        Language::English => format!("Remove the folder “{name}” from the library?"),
    }
}

fn scan_progress_for(language: Language, done: u32, total: u32) -> String {
    match language {
        Language::Chinese => format!("掃描 {done} / {total}"),
        Language::English => format!("Scanning {done} / {total}"),
    }
}

fn thumb_progress_for(language: Language, done: u32, total: u32) -> String {
    match language {
        Language::Chinese => format!("縮圖 {done} / {total}"),
        Language::English => format!("Thumbnails {done} / {total}"),
    }
}

fn asset_count_for(language: Language, n: usize) -> String {
    match language {
        Language::Chinese => format!("{n} 個素材"),
        Language::English if n == 1 => "1 asset".into(),
        Language::English => format!("{n} assets"),
    }
}

pub fn system_language() -> Language {
    language_from_locale(&sys_locale::get_locale().unwrap_or_default())
}

pub fn language_from_locale(raw: &str) -> Language {
    let normalized = raw.trim().replace('_', "-").to_ascii_lowercase();
    let primary = normalized.split(['-', '.']).next().unwrap_or("");
    if primary == "zh" {
        Language::Chinese
    } else {
        Language::English
    }
}

fn pref_path() -> std::path::PathBuf {
    data_dir().join("locale")
}

fn apply_kit_locale() {
    match current() {
        Language::English => gpui_kit::component::set_locale("en"),
        Language::Chinese => gpui_kit::component::set_locale("zh-TW"),
    }
}

fn lookup(language: Language, key: Key) -> &'static str {
    match (language, key) {
        (Language::Chinese, Key::AppTitle) => "列印素材庫",
        (Language::English, Key::AppTitle) => "Print Asset Library",
        (Language::Chinese, Key::Search) => "搜尋素材…",
        (Language::English, Key::Search) => "Search assets…",
        (Language::Chinese, Key::AddFolder) => "加入資料夾",
        (Language::English, Key::AddFolder) => "Add Folder",
        (Language::Chinese, Key::AddFolderEllipsis) => "加入資料夾…",
        (Language::English, Key::AddFolderEllipsis) => "Add Folder…",
        (Language::Chinese, Key::Folders) => "資料夾",
        (Language::English, Key::Folders) => "Folders",
        (Language::Chinese, Key::FolderFallback) => "資料夾",
        (Language::English, Key::FolderFallback) => "Folder",
        (Language::Chinese, Key::Tags) => "標籤",
        (Language::English, Key::Tags) => "Tags",
        (Language::Chinese, Key::AllAssets) => "全部素材",
        (Language::English, Key::AllAssets) => "All Assets",
        (Language::Chinese, Key::Open) => "打開",
        (Language::English, Key::Open) => "Open",
        (Language::Chinese, Key::EmptyTitle) => "尚未加入素材資料夾",
        (Language::English, Key::EmptyTitle) => "No asset folders yet",
        (Language::Chinese, Key::EmptyBody) => {
            "掃描現有的 3MF / STL / OBJ，檔案留在原處，只建立索引與縮圖。"
        }
        (Language::English, Key::EmptyBody) => {
            "Index existing 3MF / STL / OBJ files in place. Originals stay put; only the catalog and thumbnails are created."
        }
        (Language::Chinese, Key::Scanning) => "掃描中…",
        (Language::English, Key::Scanning) => "Scanning…",
        (Language::Chinese, Key::Triangles) => "三角面",
        (Language::English, Key::Triangles) => "Triangles",
        (Language::Chinese, Key::Size) => "尺寸",
        (Language::English, Key::Size) => "Size",
        (Language::Chinese, Key::Format) => "格式",
        (Language::English, Key::Format) => "Format",
        (Language::Chinese, Key::TagsHint) => "標籤（逗號分隔，Enter 套用）",
        (Language::English, Key::TagsHint) => "Tags (comma-separated, press Enter)",
        (Language::Chinese, Key::DragHint) => "拖曳旋轉 · 滾輪縮放 · 雙擊重設",
        (Language::English, Key::DragHint) => "Drag to orbit · scroll to zoom · double-click to reset",
        (Language::Chinese, Key::Loading) => "載入中…",
        (Language::English, Key::Loading) => "Loading…",
        (Language::Chinese, Key::Failed) => "無法預覽",
        (Language::English, Key::Failed) => "Preview failed",
        (Language::Chinese, Key::Offline) => "離線",
        (Language::English, Key::Offline) => "Offline",
        (Language::Chinese, Key::NoSelection) => "選擇一個模型以預覽",
        (Language::English, Key::NoSelection) => "Select a model to preview",
        (Language::Chinese, Key::EmptyFolder) => "此資料夾沒有 3MF / STL / OBJ",
        (Language::English, Key::EmptyFolder) => "This folder has no 3MF / STL / OBJ files",
        (Language::Chinese, Key::Language) => "語言",
        (Language::English, Key::Language) => "Language",
        (Language::Chinese, Key::LanguageSystem) => "跟隨系統",
        (Language::English, Key::LanguageSystem) => "System",
        (Language::Chinese, Key::LanguageChinese) => "中文",
        (Language::English, Key::LanguageChinese) => "中文",
        (Language::Chinese, Key::LanguageEnglish) => "English",
        (Language::English, Key::LanguageEnglish) => "English",
        (Language::Chinese, Key::Quit) => "結束",
        (Language::English, Key::Quit) => "Quit",
        (Language::Chinese, Key::Sort) => "排序",
        (Language::English, Key::Sort) => "Sort",
        (Language::Chinese, Key::SortNameAsc) => "名稱 A → Z",
        (Language::English, Key::SortNameAsc) => "Name A → Z",
        (Language::Chinese, Key::SortNameDesc) => "名稱 Z → A",
        (Language::English, Key::SortNameDesc) => "Name Z → A",
        (Language::Chinese, Key::SortModifiedDesc) => "修改時間（新到舊）",
        (Language::English, Key::SortModifiedDesc) => "Date modified (newest)",
        (Language::Chinese, Key::SortModifiedAsc) => "修改時間（舊到新）",
        (Language::English, Key::SortModifiedAsc) => "Date modified (oldest)",
        (Language::Chinese, Key::SortSizeDesc) => "尺寸（大到小）",
        (Language::English, Key::SortSizeDesc) => "Size (largest)",
        (Language::Chinese, Key::SortSizeAsc) => "尺寸（小到大）",
        (Language::English, Key::SortSizeAsc) => "Size (smallest)",
        (Language::Chinese, Key::SortTrianglesDesc) => "三角面（多到少）",
        (Language::English, Key::SortTrianglesDesc) => "Triangles (most)",
        (Language::Chinese, Key::SortTrianglesAsc) => "三角面（少到多）",
        (Language::English, Key::SortTrianglesAsc) => "Triangles (fewest)",
        (Language::Chinese, Key::SortFormat) => "格式",
        (Language::English, Key::SortFormat) => "Format",
        (Language::Chinese, Key::ViewGrid) => "網格",
        (Language::English, Key::ViewGrid) => "Grid",
        (Language::Chinese, Key::ViewList) => "清單",
        (Language::English, Key::ViewList) => "List",
        (Language::Chinese, Key::ThumbSize) => "縮圖大小",
        (Language::English, Key::ThumbSize) => "Thumbnail size",
        (Language::Chinese, Key::RevealInFileManager) if cfg!(target_os = "macos") => {
            "在 Finder 中顯示"
        }
        (Language::English, Key::RevealInFileManager) if cfg!(target_os = "macos") => {
            "Show in Finder"
        }
        (Language::Chinese, Key::RevealInFileManager) => "在檔案管理員中顯示",
        (Language::English, Key::RevealInFileManager) => "Show in File Manager",
        (Language::Chinese, Key::CopyPath) => "複製路徑",
        (Language::English, Key::CopyPath) => "Copy Path",
        (Language::Chinese, Key::RegenerateThumb) => "重新產生縮圖",
        (Language::English, Key::RegenerateThumb) => "Regenerate Thumbnail",
        (Language::Chinese, Key::Name) => "名稱",
        (Language::English, Key::Name) => "Name",
        (Language::Chinese, Key::Modified) => "修改時間",
        (Language::English, Key::Modified) => "Modified",
        (Language::Chinese, Key::MoveToTrashEllipsis) => "移到垃圾桶…",
        (Language::English, Key::MoveToTrashEllipsis) => "Move to Trash…",
        (Language::Chinese, Key::MoveToTrash) => "移到垃圾桶",
        (Language::English, Key::MoveToTrash) => "Move to Trash",
        (Language::Chinese, Key::TrashDetail) => "原始檔案會移到系統垃圾桶，可從垃圾桶復原。",
        (Language::English, Key::TrashDetail) => {
            "The original file will be moved to the system Trash, where you can restore it."
        }
        (Language::Chinese, Key::TrashFailed) => "無法移到垃圾桶",
        (Language::English, Key::TrashFailed) => "Couldn't Move to Trash",
        (Language::Chinese, Key::RemoveFolderEllipsis) => "移除資料夾…",
        (Language::English, Key::RemoveFolderEllipsis) => "Remove Folder…",
        (Language::Chinese, Key::RemoveFolder) => "移除",
        (Language::English, Key::RemoveFolder) => "Remove",
        (Language::Chinese, Key::RemoveFolderDetail) => {
            "只會從素材庫移除，磁碟上的檔案不受影響。之後可以再加回來。"
        }
        (Language::English, Key::RemoveFolderDetail) => {
            "It is only removed from the library; files on disk are not touched. You can add it again later."
        }
        (Language::Chinese, Key::Cancel) => "取消",
        (Language::English, Key::Cancel) => "Cancel",
        (Language::Chinese, Key::Ok) => "好",
        (Language::English, Key::Ok) => "OK",
    }
}

pub fn sort_key(sort: AssetSort) -> Key {
    match sort {
        AssetSort::NameAsc => Key::SortNameAsc,
        AssetSort::NameDesc => Key::SortNameDesc,
        AssetSort::ModifiedDesc => Key::SortModifiedDesc,
        AssetSort::ModifiedAsc => Key::SortModifiedAsc,
        AssetSort::SizeDesc => Key::SortSizeDesc,
        AssetSort::SizeAsc => Key::SortSizeAsc,
        AssetSort::TrianglesDesc => Key::SortTrianglesDesc,
        AssetSort::TrianglesAsc => Key::SortTrianglesAsc,
        AssetSort::Format => Key::SortFormat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_codes_map_to_chinese_or_english() {
        assert_eq!(language_from_locale("zh_TW.UTF-8"), Language::Chinese);
        assert_eq!(language_from_locale("zh-Hans-CN"), Language::Chinese);
        assert_eq!(language_from_locale("zh"), Language::Chinese);
        assert_eq!(language_from_locale("en_US"), Language::English);
        assert_eq!(language_from_locale("en-GB"), Language::English);
        assert_eq!(language_from_locale("C"), Language::English);
        assert_eq!(language_from_locale(""), Language::English);
    }

    #[test]
    fn pref_roundtrip() {
        assert_eq!(LanguagePref::parse("en"), LanguagePref::English);
        assert_eq!(LanguagePref::parse("zh-TW"), LanguagePref::Chinese);
        assert_eq!(LanguagePref::parse("system"), LanguagePref::System);
        assert_eq!(LanguagePref::English.as_str(), "en");
        assert_eq!(LanguagePref::Chinese.as_str(), "zh");
    }

    #[test]
    fn translations_cover_chinese_and_english() {
        assert_eq!(lookup(Language::Chinese, Key::Open), "打開");
        assert_eq!(lookup(Language::English, Key::Open), "Open");
        assert_eq!(lookup(Language::Chinese, Key::AppTitle), "列印素材庫");
        assert_eq!(
            lookup(Language::English, Key::AppTitle),
            "Print Asset Library"
        );
        assert_eq!(asset_count_for(Language::Chinese, 3), "3 個素材");
        assert_eq!(asset_count_for(Language::English, 1), "1 asset");
        assert_eq!(asset_count_for(Language::English, 3), "3 assets");
        assert_eq!(
            scan_progress_for(Language::English, 2, 10),
            "Scanning 2 / 10"
        );
        assert_eq!(thumb_progress_for(Language::Chinese, 1, 4), "縮圖 1 / 4");
        assert_eq!(
            thumb_progress_for(Language::English, 1, 4),
            "Thumbnails 1 / 4"
        );
        assert_eq!(
            trash_confirm_for(Language::Chinese, "a.stl"),
            "要將「a.stl」移到垃圾桶嗎？"
        );
        assert_eq!(
            remove_folder_confirm_for(Language::English, "Models"),
            "Remove the folder “Models” from the library?"
        );
    }

    #[test]
    fn every_key_has_chinese_and_english_copy() {
        const KEYS: &[Key] = &[
            Key::AppTitle,
            Key::Search,
            Key::AddFolder,
            Key::AddFolderEllipsis,
            Key::Folders,
            Key::FolderFallback,
            Key::Tags,
            Key::AllAssets,
            Key::Open,
            Key::EmptyTitle,
            Key::EmptyBody,
            Key::Scanning,
            Key::Triangles,
            Key::Size,
            Key::Format,
            Key::TagsHint,
            Key::DragHint,
            Key::Loading,
            Key::Failed,
            Key::Offline,
            Key::NoSelection,
            Key::EmptyFolder,
            Key::Language,
            Key::LanguageSystem,
            Key::LanguageChinese,
            Key::LanguageEnglish,
            Key::Quit,
            Key::Sort,
            Key::SortNameAsc,
            Key::SortNameDesc,
            Key::SortModifiedDesc,
            Key::SortModifiedAsc,
            Key::SortSizeDesc,
            Key::SortSizeAsc,
            Key::SortTrianglesDesc,
            Key::SortTrianglesAsc,
            Key::SortFormat,
            Key::ViewGrid,
            Key::ViewList,
            Key::ThumbSize,
            Key::RevealInFileManager,
            Key::CopyPath,
            Key::RegenerateThumb,
            Key::Name,
            Key::Modified,
            Key::MoveToTrashEllipsis,
            Key::MoveToTrash,
            Key::TrashDetail,
            Key::TrashFailed,
            Key::RemoveFolderEllipsis,
            Key::RemoveFolder,
            Key::RemoveFolderDetail,
            Key::Cancel,
            Key::Ok,
        ];
        for key in KEYS {
            assert!(
                !lookup(Language::Chinese, *key).is_empty(),
                "missing zh for {key:?}"
            );
            assert!(
                !lookup(Language::English, *key).is_empty(),
                "missing en for {key:?}"
            );
        }
    }

    #[test]
    fn language_pref_as_str_matches_parse() {
        for pref in [
            LanguagePref::System,
            LanguagePref::English,
            LanguagePref::Chinese,
        ] {
            assert_eq!(LanguagePref::parse(pref.as_str()), pref);
        }
        assert_eq!(LanguagePref::parse("  EN-US "), LanguagePref::English);
        assert_eq!(LanguagePref::parse("zh_HK"), LanguagePref::Chinese);
    }

    #[test]
    fn sort_keys_cover_every_mode() {
        for sort in AssetSort::ALL {
            let key = sort_key(sort);
            assert!(!lookup(Language::Chinese, key).is_empty());
            assert!(!lookup(Language::English, key).is_empty());
        }
        assert_eq!(lookup(Language::Chinese, Key::SortNameAsc), "名稱 A → Z");
        assert_eq!(
            lookup(Language::English, Key::SortModifiedDesc),
            "Date modified (newest)"
        );
    }
}
