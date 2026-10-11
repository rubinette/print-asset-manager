//! Sidebar: all assets, duplicates, library folder trees, saved searches,
//! and tags.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Icon, IconName, Sizable as _};
use gpui_kit::{prelude::FluentBuilder as _, *};
use pam_core::catalog::Library;

use crate::i18n::{self, Key};

use super::saved::saved_rows;
use super::Workspace;

pub(super) const SIDEBAR_WIDTH: f32 = 208.0;

/// What the sidebar has picked; filters narrow it further.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(super) enum Location {
    #[default]
    All,
    /// A library, or a folder inside it (`dir` relative to its root).
    Library {
        id: i64,
        dir: Option<String>,
    },
    Duplicates,
}

/// A folder inside a library; the root node has an empty `path`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct FolderNode {
    name: String,
    /// `/`-separated, relative to the library root.
    path: String,
    /// Assets in this folder and all its subfolders.
    count: usize,
    /// Sorted by name, case-insensitively.
    children: Vec<FolderNode>,
}

/// Folder tree of a library from its assets' relative paths.
fn build_folder_tree<'a>(rel_paths: impl IntoIterator<Item = &'a str>) -> FolderNode {
    #[derive(Default)]
    struct Building {
        count: usize,
        children: BTreeMap<String, Building>,
    }
    fn finish(name: String, path: String, node: Building) -> FolderNode {
        let mut children: Vec<FolderNode> = node
            .children
            .into_iter()
            .map(|(name, child)| {
                let path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                };
                finish(name, path, child)
            })
            .collect();
        children.sort_by_key(|c| c.name.to_lowercase());
        FolderNode {
            name,
            path,
            count: node.count,
            children,
        }
    }

    let mut root = Building::default();
    for rel in rel_paths {
        root.count += 1;
        let mut node = &mut root;
        let mut parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
        parts.pop(); // the file itself
        for part in parts {
            node = node.children.entry(part.to_string()).or_default();
            node.count += 1;
        }
    }
    finish(String::new(), String::new(), root)
}

fn folder_exists(root: &FolderNode, path: &str) -> bool {
    path.split('/')
        .try_fold(root, |node, part| {
            node.children.iter().find(|c| c.name == part)
        })
        .is_some()
}

/// Subfolder rows to show under `root`, depth-first, descending only into
/// folders whose path is in `expanded`. Depth starts at 1.
fn visible_folders(
    root: &FolderNode,
    expanded: impl Fn(&str) -> bool,
) -> Vec<(usize, &FolderNode)> {
    fn walk<'a>(
        node: &'a FolderNode,
        depth: usize,
        expanded: &dyn Fn(&str) -> bool,
        out: &mut Vec<(usize, &'a FolderNode)>,
    ) {
        for child in &node.children {
            out.push((depth, child));
            if expanded(&child.path) {
                walk(child, depth + 1, expanded, out);
            }
        }
    }
    let mut out = Vec::new();
    if expanded(&root.path) {
        walk(root, 1, &expanded, &mut out);
    }
    out
}

impl Workspace {
    /// Rebuild the sidebar folder trees. Separate from `reload` because that
    /// runs every half second while thumbnails are generated.
    pub(super) fn reload_folders(&mut self) {
        self.folder_trees = self
            .libraries
            .iter()
            .filter_map(|lib| {
                let paths = self.catalog.rel_paths(lib.id).ok()?;
                Some((lib.id, build_folder_tree(paths.iter().map(String::as_str))))
            })
            .collect();
        // Drop expansions of folders that no longer exist.
        let trees = &self.folder_trees;
        self.expanded.retain(|(lib, path)| {
            trees
                .get(lib)
                .is_some_and(|tree| path.is_empty() || folder_exists(tree, path))
        });
        if let Location::Library { id, dir: Some(dir) } = &self.location {
            if !trees.get(id).is_some_and(|tree| folder_exists(tree, dir)) {
                self.location = Location::Library { id: *id, dir: None };
            }
        }
    }

    fn set_location(&mut self, location: Location, cx: &mut Context<Self>) {
        self.location = location;
        self.reload(cx);
    }

    fn toggle_expanded(&mut self, lib: i64, path: String, cx: &mut Context<Self>) {
        let key = (lib, path);
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        cx.notify();
    }

    /// Sidebar tag click: plain click shows just that tag; ⌘/Ctrl-click adds
    /// it to (or drops it from) the tag filter.
    fn click_tag(&mut self, name: String, additive: bool, cx: &mut Context<Self>) {
        if additive {
            if let Some(i) = self.filters.tags.iter().position(|t| *t == name) {
                self.filters.tags.remove(i);
            } else {
                self.filters.tags.push(name);
            }
        } else {
            self.filters.tags = vec![name];
            self.location = Location::All;
        }
        self.reload(cx);
    }
}

pub(super) fn sidebar(this: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let theme = cx.theme().clone();
    v_flex()
        .id("sidebar")
        .w(px(SIDEBAR_WIDTH))
        .flex_shrink_0()
        .h_full()
        .min_h_0()
        .pb_3()
        .overflow_y_scroll()
        .border_r_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .child(sidebar_section(i18n::t(Key::Folders), &theme))
        .child(nav_item(
            "nav-all",
            Lucide::Boxes.into(),
            i18n::t(Key::AllAssets).to_string(),
            None,
            this.location == Location::All && this.filters.tags.is_empty(),
            false,
            cx.listener(|this, _, _, cx| {
                this.location = Location::All;
                this.filters.tags.clear();
                this.reload(cx);
            }),
            &theme,
        ))
        .when(this.duplicate_count > 0, |el| {
            el.child(nav_item(
                "nav-duplicates",
                Lucide::Files.into(),
                i18n::t(Key::Duplicates).to_string(),
                Some(this.duplicate_count.to_string()),
                this.location == Location::Duplicates,
                false,
                cx.listener(|this, _, _, cx| this.set_location(Location::Duplicates, cx)),
                &theme,
            ))
        })
        .children(
            this.libraries
                .iter()
                .flat_map(|lib| library_rows(this, lib, cx))
                .collect::<Vec<_>>(),
        )
        .when(!this.saved.is_empty(), |el| {
            el.child(sidebar_section(i18n::t(Key::SavedSearches), &theme))
        })
        .children(saved_rows(this, cx))
        .when(!this.tags.is_empty(), |el| {
            el.child(sidebar_section(i18n::t(Key::Tags), &theme))
        })
        .children(this.tags.iter().cloned().map(|(name, count)| {
            let selected = this.filters.tags.contains(&name);
            let tag_name = name.clone();
            nav_item(
                format!("nav-tag-{name}"),
                Lucide::Hash.into(),
                name,
                Some(count.to_string()),
                selected,
                false,
                cx.listener(move |this, event: &ClickEvent, _, cx| {
                    this.click_tag(tag_name.clone(), event.modifiers().secondary(), cx);
                }),
                &theme,
            )
        }))
}

/// A library row and, when expanded, its subfolder rows.
fn library_rows(this: &Workspace, lib: &Library, cx: &mut Context<Workspace>) -> Vec<AnyElement> {
    let theme = cx.theme().clone();
    let workspace = cx.entity().downgrade();
    let id = lib.id;
    let online = lib.root_path.exists();
    let tree = this.folder_trees.get(&id);
    let is_expanded = |path: &str| this.expanded.contains(&(id, path.to_string()));
    let selected_dir = match &this.location {
        Location::Library { id: lib_id, dir } if *lib_id == id => Some(dir.as_deref()),
        _ => None,
    };

    let root_selected = selected_dir == Some(None);
    let root_item = nav_item(
        format!("nav-lib-{id}"),
        if root_selected {
            IconName::FolderOpen.into()
        } else {
            IconName::Folder.into()
        },
        library_label(lib).to_string(),
        if online {
            tree.map(|t| t.count.to_string())
        } else {
            Some(i18n::t(Key::Offline).to_string())
        },
        root_selected,
        !online,
        cx.listener(move |this, _, _, cx| {
            this.set_location(Location::Library { id, dir: None }, cx)
        }),
        &theme,
    )
    .ml_0p5()
    .context_menu(move |menu, _, _| {
        let workspace = workspace.clone();
        menu.item(
            PopupMenuItem::new(i18n::t(Key::RemoveFolderEllipsis))
                .icon(Icon::new(Lucide::FolderMinus))
                .on_click(move |_, window, cx| {
                    let _ = workspace
                        .update(cx, |this, cx| this.confirm_remove_library(id, window, cx));
                }),
        )
    });

    let has_children = tree.is_some_and(|t| !t.children.is_empty());
    let mut rows = vec![tree_row(
        format!("lib-{id}"),
        0,
        has_children.then(|| is_expanded("")),
        cx.listener(move |this, _, _, cx| this.toggle_expanded(id, String::new(), cx)),
        root_item,
        &theme,
    )];
    let Some(tree) = tree else {
        return rows;
    };
    for (depth, folder) in visible_folders(tree, is_expanded) {
        let path = folder.path.clone();
        let selected = selected_dir == Some(Some(path.as_str()));
        let item = nav_item(
            format!("nav-dir-{id}-{path}"),
            if selected {
                IconName::FolderOpen.into()
            } else {
                IconName::Folder.into()
            },
            folder.name.clone(),
            Some(folder.count.to_string()),
            selected,
            !online,
            cx.listener({
                let path = path.clone();
                move |this, _, _, cx| {
                    this.set_location(
                        Location::Library {
                            id,
                            dir: Some(path.clone()),
                        },
                        cx,
                    )
                }
            }),
            &theme,
        )
        .ml_0p5();
        let toggle_path = path.clone();
        rows.push(tree_row(
            format!("dir-{id}-{path}"),
            depth,
            (!folder.children.is_empty()).then(|| is_expanded(&path)),
            cx.listener(move |this, _, _, cx| this.toggle_expanded(id, toggle_path.clone(), cx)),
            item,
            &theme,
        ));
    }
    rows
}

/// Indents a sidebar item and puts a disclosure chevron in front of it.
/// `expanded` is `None` for folders with nothing to expand.
fn tree_row(
    id: String,
    depth: usize,
    expanded: Option<bool>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    item: impl IntoElement,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    h_flex()
        .pl(px(8.0 + depth as f32 * 12.0))
        .items_center()
        .child(
            div()
                .id(ElementId::Name(format!("toggle-{id}").into()))
                .size(px(16.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .when_some(expanded, |el, open| {
                    el.cursor_pointer().on_click(on_toggle).child(
                        Icon::new(if open {
                            Lucide::ChevronDown
                        } else {
                            Lucide::ChevronRight
                        })
                        .xsmall(),
                    )
                }),
        )
        .child(div().flex_1().min_w_0().child(item))
        .into_any_element()
}

pub(super) fn library_label(lib: &Library) -> &str {
    lib.root_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| i18n::t(Key::FolderFallback))
}

fn sidebar_section(title: &'static str, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    div()
        .px_4()
        .pt_4()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.muted_foreground)
        .child(title)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn nav_item(
    id: impl Into<SharedString>,
    icon: Icon,
    label: String,
    trailing: Option<String>,
    selected: bool,
    dimmed: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    h_flex()
        .id(ElementId::Name(id.into()))
        .mx_2()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .when(dimmed, |el| el.text_color(theme.muted_foreground))
        .when(selected, |el| {
            el.bg(theme.accent).text_color(theme.accent_foreground)
        })
        .hover(|el| el.bg(theme.accent))
        .on_click(on_click)
        .child(icon.small())
        .child(div().flex_1().min_w_0().truncate().child(label))
        .when_some(trailing, |el, trailing| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(trailing),
            )
        })
}

#[cfg(test)]
mod tests {
    // gpui-kit's glob re-export shadows Rust's #[test]; use the prelude path.
    #[::core::prelude::v1::test]
    fn folder_tree_counts_nested_assets_and_sorts_children() {
        use super::build_folder_tree;
        let tree = build_folder_tree([
            "top.stl",
            "b/one.stl",
            "B2/x.stl",
            "a/deep/two.stl",
            "a/deep/three.stl",
            "a/four.stl",
        ]);
        assert_eq!(tree.count, 6);
        assert_eq!(tree.path, "");
        let names: Vec<&str> = tree.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "B2"]);
        let a = &tree.children[0];
        assert_eq!((a.path.as_str(), a.count), ("a", 3));
        assert_eq!(a.children[0].path, "a/deep");
        assert_eq!(a.children[0].count, 2);
        assert!(build_folder_tree(["only.stl"]).children.is_empty());
    }

    #[::core::prelude::v1::test]
    fn visible_folders_follow_expansion() {
        use super::{build_folder_tree, folder_exists, visible_folders};
        let tree = build_folder_tree(["a/b/c/x.stl", "d/y.stl"]);
        let rows = |open: &[&str]| {
            visible_folders(&tree, |p| open.contains(&p))
                .into_iter()
                .map(|(depth, node)| (depth, node.path.clone()))
                .collect::<Vec<_>>()
        };
        assert!(rows(&[]).is_empty(), "collapsed library shows nothing");
        assert_eq!(rows(&[""]), [(1, "a".into()), (1, "d".into())]);
        assert_eq!(
            rows(&["", "a", "a/b"]),
            [
                (1, "a".into()),
                (2, "a/b".into()),
                (3, "a/b/c".into()),
                (1, "d".into())
            ]
        );
        // A collapsed parent hides expanded descendants.
        assert_eq!(rows(&["", "a/b"]), [(1, "a".into()), (1, "d".into())]);
        assert!(folder_exists(&tree, "a/b/c"));
        assert!(!folder_exists(&tree, "a/c"));
    }
}
