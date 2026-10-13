//! Saved searches: a folder, search text and filters kept under a name in
//! the sidebar, with a live count.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{v_flex, ActiveTheme, Icon, WindowExt as _};
use gpui_kit::*;
use pam_core::catalog::{AssetQuery, SavedSearch};
use serde_json::{json, Value};

use crate::i18n::{self, Key};

use super::filters::Filters;
use super::removal::confirm_destructive;
use super::sidebar::{library_label, nav_item, Location};
use super::Workspace;

/// What a saved search restores.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct SavedQuery {
    pub(super) location: Location,
    pub(super) search: String,
    pub(super) filters: Filters,
}

impl SavedQuery {
    fn to_json(&self) -> String {
        let location = match &self.location {
            Location::All => json!({"kind": "all"}),
            Location::Duplicates => json!({"kind": "duplicates"}),
            Location::Favorites => json!({"kind": "favorites"}),
            Location::Library { id, dir } => json!({"kind": "library", "id": id, "dir": dir}),
        };
        json!({
            "location": location,
            "search": self.search.trim(),
            "filters": self.filters.to_json(),
        })
        .to_string()
    }

    /// Inverse of [`Self::to_json`]. Unreadable parts fall back to "all assets,
    /// no search, no filters" rather than failing.
    fn parse(raw: &str) -> Self {
        let value: Value = serde_json::from_str(raw).unwrap_or_default();
        let location = &value["location"];
        Self {
            location: match location["kind"].as_str() {
                Some("duplicates") => Location::Duplicates,
                Some("favorites") => Location::Favorites,
                Some("library") => match location["id"].as_i64() {
                    Some(id) => Location::Library {
                        id,
                        dir: location["dir"].as_str().map(String::from),
                    },
                    None => Location::All,
                },
                _ => Location::All,
            },
            search: value["search"].as_str().unwrap_or_default().to_string(),
            filters: Filters::from_json(&value["filters"]),
        }
    }
}

pub(super) struct SavedEntry {
    id: i64,
    name: String,
    query: SavedQuery,
    /// Assets it matches right now.
    count: u32,
}

/// What the name dialog will do when confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NameTarget {
    New,
    Rename(i64),
}

impl Workspace {
    fn current_saved_query(&self) -> SavedQuery {
        SavedQuery {
            location: self.location.clone(),
            search: self.search_text.trim().to_string(),
            filters: self.filters.clone(),
        }
    }

    /// Whether there is anything to save: a folder, search or filter.
    pub(super) fn can_save_search(&self) -> bool {
        self.current_saved_query() != SavedQuery::default()
    }

    /// Reload saved searches and their counts. Part of `reload`, so the
    /// counts follow tagging, scans and thumbnails.
    pub(super) fn reload_saved(&mut self) {
        let Ok(saved) = self.catalog.saved_searches() else {
            return;
        };
        self.saved = saved
            .into_iter()
            .map(|SavedSearch { id, name, query }| {
                let query = SavedQuery::parse(&query);
                let count = self
                    .catalog
                    .count(&self.saved_asset_query(&query))
                    .unwrap_or(0);
                SavedEntry {
                    id,
                    name,
                    query,
                    count,
                }
            })
            .collect();
    }

    fn saved_asset_query(&self, saved: &SavedQuery) -> AssetQuery {
        self.query_for(&saved.location, &saved.filters, &saved.search)
    }

    fn apply_saved(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.saved.iter().find(|s| s.id == id) else {
            return;
        };
        let query = entry.query.clone();
        self.location = match query.location {
            Location::Library { id, .. } if !self.libraries.iter().any(|l| l.id == id) => {
                Location::All
            }
            location => location,
        };
        self.show_filters |= query.filters != Filters::default();
        self.filters = query.filters;
        self.search_text = query.search.clone();
        self.search
            .update(cx, |input, cx| input.set_value(query.search, window, cx));
        // A folder that's gone falls back to its library.
        self.reload_folders();
        self.reload(cx);
    }

    /// Ask for a name, then save the current search (or rename one).
    pub(super) fn open_name_dialog(
        &mut self,
        target: NameTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = match target {
            NameTarget::New => self.suggested_name(),
            NameTarget::Rename(id) => match self.saved.iter().find(|s| s.id == id) {
                Some(entry) => entry.name.clone(),
                None => return,
            },
        };
        self.naming = Some(target);
        self.saved_name_input
            .update(cx, |input, cx| input.set_value(name, window, cx));
        let input = self.saved_name_input.clone();
        let this = cx.entity().downgrade();
        let title = i18n::t(match target {
            NameTarget::New => Key::SaveSearch,
            NameTarget::Rename(_) => Key::RenameSavedSearch,
        });
        window.open_dialog(cx, move |dialog, _, _| {
            let (on_cancel, on_save) = (this.clone(), this.clone());
            dialog
                .title(title)
                .w(px(380.))
                .child(v_flex().gap_2().child(Input::new(&input)))
                .on_ok({
                    let this = this.clone();
                    move |_, window, cx| {
                        let _ = this.update(cx, |this, cx| this.commit_name(window, cx));
                        true
                    }
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("name-cancel")
                                .outline()
                                .label(i18n::t(Key::Cancel))
                                .on_click(move |_, window, cx| {
                                    let _ = on_cancel.update(cx, |this, _| this.naming = None);
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("name-save")
                                .primary()
                                .label(i18n::t(Key::Save))
                                .on_click(move |_, window, cx| {
                                    let _ =
                                        on_save.update(cx, |this, cx| this.commit_name(window, cx));
                                }),
                        ),
                )
        });
        // Opening the dialog focuses the dialog itself; take it back so the
        // name can be typed (and Enter saves) right away.
        self.saved_name_input
            .update(cx, |input, cx| input.focus(window, cx));
    }

    /// Save or rename with the dialog's name, and close it. A blank name
    /// keeps the dialog open. Runs at most once per dialog.
    pub(super) fn commit_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.saved_name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        let Some(target) = self.naming.take() else {
            return;
        };
        let result = match target {
            NameTarget::New => self
                .catalog
                .add_saved_search(&name, &self.current_saved_query().to_json())
                .map(|_| ()),
            NameTarget::Rename(id) => self.catalog.rename_saved_search(id, &name),
        };
        if let Err(err) = result {
            eprintln!("save search failed: {err}");
        }
        window.close_dialog(cx);
        self.reload(cx);
    }

    fn update_saved(&mut self, id: i64, cx: &mut Context<Self>) {
        let query = self.current_saved_query().to_json();
        if let Err(err) = self.catalog.set_saved_search_query(id, &query) {
            eprintln!("update saved search failed: {err}");
        }
        self.reload(cx);
    }

    fn confirm_remove_saved(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.saved.iter().find(|s| s.id == id) else {
            return;
        };
        let this = cx.entity().downgrade();
        confirm_destructive(
            window,
            cx,
            i18n::remove_saved_search_confirm(&entry.name),
            i18n::t(Key::RemoveSavedSearchDetail),
            i18n::t(Key::Delete),
            move |_, cx| {
                let _ = this.update(cx, |this, cx| {
                    if let Err(err) = this.catalog.remove_saved_search(id) {
                        eprintln!("remove saved search failed: {err}");
                    }
                    this.reload(cx);
                });
            },
        );
    }

    /// A starting name for the dialog: the search text, else the filters,
    /// else the folder.
    fn suggested_name(&self) -> String {
        let query = self.current_saved_query();
        let mut parts = Vec::new();
        if !query.search.is_empty() {
            parts.push(query.search.clone());
        }
        parts.extend(query.filters.labels(self.bed));
        if parts.is_empty() {
            parts.push(match &query.location {
                Location::All => i18n::t(Key::AllAssets).to_string(),
                Location::Duplicates => i18n::t(Key::Duplicates).to_string(),
                Location::Favorites => i18n::t(Key::Favorites).to_string(),
                Location::Library { id, dir } => match dir {
                    Some(dir) => dir.rsplit('/').next().unwrap_or(dir).to_string(),
                    None => self
                        .libraries
                        .iter()
                        .find(|l| l.id == *id)
                        .map(|l| library_label(l).to_string())
                        .unwrap_or_default(),
                },
            });
        }
        parts.join(" · ")
    }
}

/// Sidebar rows, one per saved search. The one matching what is on screen
/// is highlighted.
pub(super) fn saved_rows(this: &Workspace, cx: &mut Context<Workspace>) -> Vec<AnyElement> {
    let theme = cx.theme().clone();
    let current = this.current_saved_query();
    let workspace = cx.entity().downgrade();
    this.saved
        .iter()
        .map(|entry| {
            let id = entry.id;
            let workspace = workspace.clone();
            nav_item(
                format!("nav-saved-{id}"),
                Icon::new(Lucide::Bookmark),
                entry.name.clone(),
                Some(entry.count.to_string()),
                entry.query == current,
                false,
                cx.listener(move |this, _, window, cx| this.apply_saved(id, window, cx)),
                &theme,
            )
            .context_menu(move |menu, _, _| {
                let (rename, update, remove) =
                    (workspace.clone(), workspace.clone(), workspace.clone());
                menu.item(
                    PopupMenuItem::new(i18n::t(Key::RenameEllipsis))
                        .icon(Icon::new(Lucide::Pencil))
                        .on_click(move |_, window, cx| {
                            let _ = rename.update(cx, |this, cx| {
                                this.open_name_dialog(NameTarget::Rename(id), window, cx)
                            });
                        }),
                )
                .item(
                    PopupMenuItem::new(i18n::t(Key::UpdateSavedSearch))
                        .icon(Icon::new(Lucide::Save))
                        .on_click(move |_, _, cx| {
                            let _ = update.update(cx, |this, cx| this.update_saved(id, cx));
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(i18n::t(Key::DeleteEllipsis))
                        .icon(Icon::new(Lucide::Trash))
                        .on_click(move |_, window, cx| {
                            let _ = remove
                                .update(cx, |this, cx| this.confirm_remove_saved(id, window, cx));
                        }),
                )
            })
            .into_any_element()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Filters, Location, SavedQuery};
    use pam_core::catalog::{PrintStatus, StatusFilter};

    // gpui-kit's glob re-export shadows Rust's #[test]; use the prelude path.
    #[::core::prelude::v1::test]
    fn saved_query_roundtrips_and_tolerates_junk() {
        let queries = [
            SavedQuery::default(),
            SavedQuery {
                location: Location::Duplicates,
                search: "benchy".into(),
                ..Default::default()
            },
            SavedQuery {
                location: Location::Library {
                    id: 3,
                    dir: Some("零件/螺絲".into()),
                },
                search: String::new(),
                filters: Filters {
                    status: Some(StatusFilter::Is(PrintStatus::Todo)),
                    fits_bed: true,
                    ..Default::default()
                },
            },
            SavedQuery {
                location: Location::Library { id: 1, dir: None },
                ..Default::default()
            },
            SavedQuery {
                location: Location::Favorites,
                ..Default::default()
            },
        ];
        for query in queries {
            assert_eq!(SavedQuery::parse(&query.to_json()), query);
        }
        assert_eq!(SavedQuery::parse("not json"), SavedQuery::default());
        assert_eq!(
            SavedQuery::parse(r#"{"location":{"kind":"library"},"search":7}"#),
            SavedQuery::default()
        );
    }
}
