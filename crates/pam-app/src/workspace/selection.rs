//! Which assets are selected, and how clicks and arrow keys change that.

use std::collections::HashSet;

use gpui_kit::*;
use pam_core::catalog::Asset;
use pam_preview::Camera;

use super::asset_view::ViewMode;
use super::preview::load_thumb_render_image;
use super::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Left,
    Right,
    Up,
    Down,
}

/// Index to select after an arrow key, or `None` if the key does nothing.
fn step_index(
    current: Option<usize>,
    len: usize,
    cols: usize,
    step: Step,
    mode: ViewMode,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(current) = current else {
        return Some(0);
    };
    let cols = cols.max(1) as isize;
    let delta = match (mode, step) {
        (ViewMode::List, Step::Left | Step::Right) => return None,
        (ViewMode::List, Step::Up) => -1,
        (ViewMode::List, Step::Down) => 1,
        (ViewMode::Grid, Step::Left) => -1,
        (ViewMode::Grid, Step::Right) => 1,
        (ViewMode::Grid, Step::Up) => -cols,
        (ViewMode::Grid, Step::Down) => cols,
    };
    let next = (current as isize + delta).clamp(0, len as isize - 1) as usize;
    (next != current).then_some(next)
}

/// Ids from `anchor` to `target` inclusive, in list order. Without a usable
/// anchor, just `target`.
fn range_ids(assets: &[Asset], anchor: Option<i64>, target: i64) -> Vec<i64> {
    let find = |id: i64| assets.iter().position(|a| a.id == id);
    let Some(to) = find(target) else {
        return Vec::new();
    };
    let from = anchor.and_then(find).unwrap_or(to);
    let (lo, hi) = (from.min(to), from.max(to));
    assets[lo..=hi].iter().map(|a| a.id).collect()
}

impl Workspace {
    pub(super) fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.assets.iter().position(|a| a.id == id)
    }

    pub(super) fn move_selection(
        &mut self,
        step: Step,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(next) = step_index(
            self.selected_index(),
            self.assets.len(),
            self.grid_cols,
            step,
            self.view_mode,
        ) else {
            return;
        };
        let id = self.assets[next].id;
        self.select(id, window, cx);
        self.scroll_to_selected();
    }

    pub(super) fn scroll_to_selected(&self) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let row = match self.view_mode {
            ViewMode::Grid => index / self.grid_cols.max(1),
            ViewMode::List => index,
        };
        self.asset_scroll
            .scroll_to_item(row, ScrollStrategy::Nearest);
    }

    /// What a menu or shortcut acts on for `id`: the whole selection if `id`
    /// is part of it, otherwise just `id`.
    pub(super) fn targets_for(&self, id: i64) -> Vec<i64> {
        if self.selection.contains(&id) {
            self.selection.clone()
        } else {
            vec![id]
        }
    }

    /// Whether every asset in `ids` is starred, so toggling them unstars.
    pub(super) fn all_favorite(&self, ids: &[i64]) -> bool {
        if ids.is_empty() {
            return false;
        }
        let starred: HashSet<i64> = self
            .assets
            .iter()
            .filter(|a| a.favorite)
            .map(|a| a.id)
            .collect();
        ids.iter().all(|id| starred.contains(id))
    }

    /// Unstar `ids` if they are all starred, otherwise star them all.
    pub(super) fn toggle_favorite(
        &mut self,
        ids: &[i64],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ids.is_empty() {
            return;
        }
        let favorite = !self.all_favorite(ids);
        let index = self.selected_index().unwrap_or(0);
        if let Err(err) = self.catalog.set_favorite(ids, favorite) {
            eprintln!("set favorite failed: {err}");
        }
        // Unstarring in the favorites view drops the rows, possibly the
        // selected one, so refresh the preview and tag input like a removal.
        self.reload_after_removal(index, window, cx);
    }

    /// Select only `id`.
    pub(super) fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = vec![id];
        self.anchor = Some(id);
        self.focus_asset(id, window, cx);
    }

    /// ⌘/Ctrl-click: add `id` to the selection, or take it out.
    pub(super) fn toggle_select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.anchor = Some(id);
        if let Some(i) = self.selection.iter().position(|s| *s == id) {
            self.selection.remove(i);
            if self.selected == Some(id) {
                match self.selection.last().copied() {
                    Some(next) => self.focus_asset(next, window, cx),
                    None => self.clear_selection(cx),
                }
            } else {
                cx.notify();
            }
        } else {
            self.selection.push(id);
            self.focus_asset(id, window, cx);
        }
    }

    /// Shift-click: select everything from the anchor to `id`.
    pub(super) fn extend_select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = range_ids(&self.assets, self.anchor, id);
        if self.anchor.is_none() {
            self.anchor = Some(id);
        }
        self.focus_asset(id, window, cx);
    }

    pub(super) fn select_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = self.assets.iter().map(|a| a.id).collect();
        match self.selected.or_else(|| self.assets.first().map(|a| a.id)) {
            Some(id) if self.selected != Some(id) => self.focus_asset(id, window, cx),
            _ => cx.notify(),
        }
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.selection.clear();
        self.preview_mesh = None;
        if let Some(old) = self.preview_image.take() {
            cx.drop_image(old, None);
        }
        cx.notify();
    }

    /// Make `id` the primary selection: preview it and load its tags into the
    /// inspector. Leaves `selection` alone.
    fn focus_asset(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.refresh_copies();
        if self.preview_mesh.as_ref().is_some_and(|m| m.asset_id != id) {
            self.preview_mesh = None;
        }
        self.camera = Camera::default();
        self.last_preview_pos = None;
        self.preview_busy = false;
        self.preview_dirty = false;
        if let Some(old) = self.preview_image.take() {
            cx.drop_image(old, None);
        }
        if let Some(asset) = self.assets.iter().find(|a| a.id == id) {
            let tags = asset.tags.join(", ");
            self.tag_input.update(cx, |input, cx| {
                input.set_value(tags, window, cx);
            });
            if let Some(thumb) = load_thumb_render_image(asset) {
                self.preview_image = Some(thumb);
            }
        }
        self.render_preview(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // gpui-kit's glob re-export shadows Rust's #[test]; use the prelude path.
    #[::core::prelude::v1::test]
    fn arrow_keys_move_within_grid_and_list() {
        use super::{step_index, Step, ViewMode};
        let grid = |cur, step| step_index(cur, 10, 4, step, ViewMode::Grid);
        assert_eq!(grid(None, Step::Down), Some(0));
        assert_eq!(grid(Some(1), Step::Right), Some(2));
        assert_eq!(grid(Some(1), Step::Down), Some(5));
        assert_eq!(grid(Some(5), Step::Up), Some(1));
        // Partial last row clamps to the last asset; edges are no-ops.
        assert_eq!(grid(Some(7), Step::Down), Some(9));
        assert_eq!(grid(Some(0), Step::Left), None);
        assert_eq!(grid(Some(9), Step::Right), None);

        let list = |cur, step| step_index(cur, 3, 4, step, ViewMode::List);
        assert_eq!(list(Some(0), Step::Down), Some(1));
        assert_eq!(list(Some(1), Step::Up), Some(0));
        assert_eq!(list(Some(1), Step::Right), None);
        assert_eq!(step_index(None, 0, 4, Step::Down, ViewMode::Grid), None);
    }

    fn asset(id: i64) -> pam_core::catalog::Asset {
        pam_core::catalog::Asset {
            id,
            library_id: 1,
            root_path: "/lib".into(),
            rel_path: format!("{id}.stl"),
            format: pam_core::mesh::AssetFormat::Stl,
            size_bytes: 0,
            mtime_ns: 0,
            content_sha256: None,
            triangle_count: None,
            bbox: None,
            thumb_state: pam_core::catalog::ThumbState::Pending,
            error: None,
            tags: Vec::new(),
            favorite: false,
        }
    }

    #[::core::prelude::v1::test]
    fn shift_click_selects_range_in_list_order() {
        use super::range_ids;
        let assets: Vec<_> = [10, 20, 30, 40].into_iter().map(asset).collect();
        assert_eq!(range_ids(&assets, Some(20), 40), [20, 30, 40]);
        assert_eq!(range_ids(&assets, Some(40), 20), [20, 30, 40]);
        assert_eq!(range_ids(&assets, None, 30), [30]);
        // Anchor filtered out of the list: fall back to the clicked asset.
        assert_eq!(range_ids(&assets, Some(99), 30), [30]);
        assert!(range_ids(&assets, Some(10), 99).is_empty());
    }
}
