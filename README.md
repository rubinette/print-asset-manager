# 列印素材庫

用 Rust + GPUI 寫的原生 3D 打印素材管理工具。掃描本機資料夾（檔案不搬動），索引 **3MF / STL / OBJ**，產生縮圖，搜尋與標籤，並在視窗內旋轉預覽。

支援 **macOS** 與 **Linux**（Wayland / X11）。

## 需求

- Rust 1.85+
- macOS：Xcode Command Line Tools（Metal）
- Linux：`libxkbcommon`、Vulkan 或 OpenGL 驅動、以及 Wayland 或 X11 開發套件

## 執行

```bash
cargo run -p pam-app --release
```

啟動後點「加入資料夾」，選一個含 `.stl` / `.3mf` / `.obj` 的目錄。原檔留在原處；索引存在：

- macOS：`~/Library/Application Support/dev.print-asset-manager.print-asset-manager/catalog.sqlite`
- Linux：`~/.local/share/print-asset-manager/catalog.sqlite`

縮圖快取：

- macOS：`~/Library/Caches/dev.print-asset-manager.print-asset-manager/thumbs/`
- Linux：`~/.cache/print-asset-manager/thumbs/`

## 操作

| 動作 | 快捷鍵 |
|------|--------|
| 加入資料夾 | ⌘O / Ctrl+O |
| 搜尋 | ⌘F / Ctrl+F |
| 打開（系統預設切片軟體） | Enter |
| 多選：加入／取消單一個 | ⌘-點選 / Ctrl-點選 |
| 多選：選取範圍 | Shift-點選 |
| 全選目前列表 | ⌘A / Ctrl+A |
| 將選取的檔案移到垃圾桶（會先確認） | ⌘⌫ / Delete |
| 結束 | ⌘Q / Ctrl+Q |

預覽區：拖曳旋轉、滾輪縮放、雙擊重設視角。卡片雙擊等同打開。可把資料夾拖進視窗加入庫。

- **子資料夾**：點側欄資料夾左邊的箭頭展開，點子資料夾只看它（含更深層）底下的檔案。
- **篩選**：工具列「篩選」打開篩選列，可依格式、是否放得進列印平台（在「尺寸」選單選機型，會記住）、檔案大小、修改時間、多個標籤（全部符合／任一符合）、未加標籤、縮圖失敗來篩。側欄標籤按 ⌘／Ctrl-點選可以一次選多個。
- **重複檔案**：內容完全相同（SHA-256 一樣）的檔案會出現在側欄「重複檔案」，同一組排在一起。雜湊在產生縮圖時計算，所以要等縮圖跑完才完整。
- **批次操作**：多選後，右側面板可以一次加標籤、移除某個標籤，或把全部移到垃圾桶；右鍵選單也會對整個選取動作。
- **移除資料夾**：在側欄資料夾上按右鍵 →「移除資料夾…」。只從素材庫移除，不會動到磁碟上的檔案。
- **移除檔案**：在素材上按右鍵 →「移到垃圾桶…」，或用詳細資訊面板的垃圾桶按鈕。原始檔會移到系統垃圾桶（可復原），這是 App 唯一會動到原始檔的操作。

## 架構

```
crates/pam-core      掃描、SQLite、STL/OBJ/3MF 解析
crates/pam-preview   CPU 光柵化縮圖與互動預覽
crates/pam-app       GPUI Kit 桌面介面
```

3MF 若含切片軟體嵌入的 PNG（例如 `Metadata/plate_1.png`）會直接當縮圖，不必先 tessellate。

## 測試

```bash
cargo test --workspace
```

v1 不做：STEP/IGES 預覽、切片、派送印表機、Windows、把檔案複製進統一庫。
