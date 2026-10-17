# 列印素材庫

用 Rust + GPUI 寫的原生 3D 打印素材管理工具。掃描本機資料夾（檔案不搬動），索引 **3MF / STL / OBJ**，產生縮圖，搜尋與標籤，並在視窗內旋轉預覽。

支援 **macOS** 與 **Linux**（Wayland / X11）。

## 需求

- Rust 1.95+
- macOS：Xcode Command Line Tools（Metal）
- Linux：`libxkbcommon`、`fontconfig`、Vulkan 或 OpenGL 驅動、以及 Wayland 或 X11 開發套件

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
| 打開（系統預設切片軟體） | Enter（在素材列表上） |
| 多選：加入／取消單一個 | ⌘-點選 / Ctrl-點選 |
| 多選：選取範圍 | Shift-點選 |
| 全選目前列表 | ⌘A / Ctrl+A |
| 將選取的檔案移到垃圾桶（會先確認） | ⌘⌫ / Delete |
| 加入／移出我的最愛 | F |
| 結束 | ⌘Q / Ctrl+Q |

預覽區：拖曳旋轉、滾輪縮放、雙擊重設視角。卡片雙擊等同打開。可把資料夾拖進視窗加入庫。

- **子資料夾**：點側欄資料夾左邊的箭頭展開，點子資料夾只看它（含更深層）底下的檔案。
- **篩選**：工具列「篩選」打開篩選列，可依格式、是否放得進列印平台（在「尺寸」選單選機型，會記住）、檔案大小、修改時間、多個標籤（全部符合／任一符合）、未加標籤、縮圖失敗來篩。側欄標籤按 ⌘／Ctrl-點選可以一次選多個。
- **我的最愛**：在素材上按右鍵 →「加入我的最愛」、按詳細資訊面板的星號，或按 F。加了星號的檔案會出現在側欄「我的最愛」，卡片右上角也會顯示星號。多選時會一次加入或移出（全部都已加入時才會移出）。檔案修改、改名或搬移後星號會保留，檔案移到垃圾桶後一併移除。
- **已存搜尋**：選好資料夾、搜尋字和篩選後，按工具列搜尋框右邊的書籤按鈕，取個名字存起來（例如「待印的 PETG」）。它會出現在側欄「已存搜尋」，旁邊的數字是目前符合的檔案數，點一下就回到那組條件。右鍵可以重新命名、改成目前的條件或刪除。
- **重複檔案**：內容完全相同（SHA-256 一樣）的檔案會出現在側欄「重複檔案」，同一組排在一起。雜湊在掃描後就和縮圖一起計算，而且比算縮圖快得多，所以不用等縮圖跑完，清單就會先補齊。
- **批次操作**：多選後，右側面板可以一次加標籤、移除某個標籤，或把全部移到垃圾桶；右鍵選單也會對整個選取動作。
- **切片資訊**：Bambu Studio、OrcaSlicer、PrusaSlicer 存的 3MF 專案檔，會讀出標題、設計者、印表機、材料、層高；切過片的檔案還有預估列印時間和耗材重量。這些顯示在詳細資訊面板，篩選列可依「材料」和「列印時間」篩，搜尋也找得到標題、設計者、印表機和材料。
- **同資料夾的圖片與說明**：詳細資訊面板最下面會列出模型旁邊的照片（PNG / JPEG / WebP）、README、授權和 PDF 說明，點一下用系統預設程式打開，README 會直接預覽前幾行。從模型網站下載、解壓後是 `files/` 加 `images/` 的資料夾也認得。同一個資料夾裡模型很多時，只列出檔名和模型對得上的檔案。
- **列印狀態與備註**：在詳細資訊面板把檔案標成「待印／已印／失敗」（再按一次取消），並寫一段備註（參數、心得）。多選或右鍵選單可以一次標多個；篩選列的「列印狀態」可以只看某一種，搜尋也會找備註內容。
- **改名與搬移**：在素材庫裡改檔名或搬到別的子資料夾，標籤、縮圖和其他資料都會跟著檔案走。搬到另一個已加入的資料夾時，標籤、列印狀態、備註和星號會在新位置算完雜湊後接回去（90 天內有效）。
- **移除資料夾**：在側欄資料夾上按右鍵 →「移除資料夾…」。只從素材庫移除，不會動到磁碟上的檔案；之後再加回來，標籤、列印狀態和備註也會接回去。
- **移除檔案**：在素材上按右鍵 →「移到垃圾桶…」，或用詳細資訊面板的垃圾桶按鈕。原始檔會移到系統垃圾桶（可復原），這是 App 唯一會動到原始檔的操作。

## 發布

先把 `Cargo.toml` 的 `version` 改成新版本並合併，再推一個對應的 tag：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

GitHub Actions 的 Release 工作會編出下列檔案，放進一個**草稿** Release，確認後在 GitHub 上按發布：

- `print-asset-manager-<版本>-macos-universal.dmg`：Intel 與 Apple Silicon 通用。尚未簽章與公證，第一次打開要在 Finder 對 App 按右鍵 →「打開」。
- `print-asset-manager-<版本>-linux-x86_64.tar.gz`、`…-linux-aarch64.tar.gz`：解開後執行 `./install.sh` 安裝到 `~/.local`（含選單捷徑和圖示）。

打包用的檔案在 `packaging/`；`packaging/icon.png` 是暫用圖示。

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
