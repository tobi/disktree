//! Bounded semantic navigation over the complete scan, never its preview.

use crate::semantics::AccessibleButton as _;

use std::fmt::Write as _;
use std::path::Path;

use disktree_core::filter::Keep;
use disktree_core::size::human_bytes;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, InteractiveElement as _, ParentElement as _, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled, div,
};
use gpui_omarchy::{ActiveTheme as _, ButtonVariant, button};

use crate::state::Disktree;
use crate::ui::{size, space, text};

/// Explicit pages keep offscreen entries reachable to AT while bounding
/// layout and semantic work independently of the size of the scan.
pub const PAGE_SIZE: usize = 100;

/// Lossless IDs distinguish paths even when their display text is identical.
pub fn path_id(path: &Path) -> SharedString {
    let mut id = String::from("path-");
    for byte in path.as_os_str().as_encoded_bytes() {
        write!(id, "{byte:02x}").expect("writing a string");
    }
    id.into()
}

fn entry_semantics(
    path: &Path,
    label: String,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(path_id(path))
        .accessibility_id(path_id(path))
        .role(Role::ListBoxOption)
        .aria_label(label)
        .aria_selected(selected)
        .when(
            selected,
            gpui_kit::StatefulInteractiveElement::aria_active_descendant,
        )
}

impl Disktree {
    pub fn file_turn_page(&mut self, step: isize, cx: &mut Context<'_, Self>) {
        let entries =
            self.layout_filter().is_some().then(|| self.file_entries());
        let count = entries.as_ref().map_or_else(
            || self.current().map_or(0, |node| node.children().len()),
            Vec::len,
        );
        if count == 0 {
            return;
        }
        let last = (count - 1) / PAGE_SIZE;
        let page = self
            .file_page
            .min(last)
            .saturating_add_signed(step)
            .min(last);
        let position = page * PAGE_SIZE;
        let mut crumbs = self.crumbs.clone();
        crumbs.push(
            entries
                .as_ref()
                .map_or(position, |entries| entries[position]),
        );
        self.file_page = page;
        self.file_scroll.scroll_to_item(0);
        self.pointer_active = false;
        self.select(Some(crumbs), cx);
    }
    /// What the list's header adds when a filter thins it. Compact, the
    /// legend row is not drawn, so the list itself says what the legend
    /// keeps; a search says so in its own field.
    pub fn list_filter_note(&self) -> String {
        if self.filter_applied {
            " · filtered".to_owned()
        } else if self.legend.is_active() {
            format!(" · filtered: {} (esc clears)", self.legend.summary())
        } else {
            String::new()
        }
    }

    /// An entry's size as its row says it. A partly shown folder says what
    /// passed and what it holds in all, as its tile does: "48 MiB of 196
    /// MiB".
    pub fn entry_size(
        &self,
        crumbs: &[usize],
        node: disktree_core::tree::Node<'_>,
    ) -> String {
        match self.layout_filter().and_then(|m| m.keep(crumbs)) {
            Some(Keep::Partial { bytes, .. }) => {
                format!("{} of {}", human_bytes(bytes), human_bytes(node.bytes))
            }
            _ => human_bytes(node.bytes),
        }
    }

    pub fn file_entries(&self) -> Vec<usize> {
        let Some(node) = self.current() else {
            return Vec::new();
        };
        let children = node.children();
        let Some(filter) = self.layout_filter() else {
            return (0..children.len()).collect();
        };
        // One buffer for every child: under a filter this runs per frame,
        // in directories that can hold millions of files.
        let mut crumbs = self.crumbs.clone();
        crumbs.push(0);
        let last = crumbs.len() - 1;
        (0..children.len())
            .filter(|index| {
                crumbs[last] = *index;
                filter.keep_at(&crumbs, children.at(*index)).is_some()
            })
            .collect()
    }

    pub fn file_step(&mut self, step: isize, cx: &mut Context<'_, Self>) {
        let entries =
            self.layout_filter().is_some().then(|| self.file_entries());
        let count = entries.as_ref().map_or_else(
            || self.current().map_or(0, |node| node.children().len()),
            Vec::len,
        );
        if count == 0 {
            return;
        }
        let current = self.selected.as_ref().and_then(|crumbs| {
            (crumbs.len() == self.crumbs.len() + 1
                && crumbs.starts_with(&self.crumbs))
            .then(|| {
                entries.as_ref().map_or_else(
                    || crumbs.last().copied().filter(|index| *index < count),
                    |entries| {
                        entries.iter().position(|i| Some(i) == crumbs.last())
                    },
                )
            })
            .flatten()
        });
        let next =
            current.map_or(0, |i| i.saturating_add_signed(step).min(count - 1));
        let mut crumbs = self.crumbs.clone();
        crumbs.push(entries.as_ref().map_or(next, |entries| entries[next]));
        self.pointer_active = false;
        self.file_page = next / PAGE_SIZE;
        self.file_scroll.scroll_to_item(next % PAGE_SIZE);
        self.select(Some(crumbs), cx);
    }
}

pub fn compact(window: &gpui_kit::Window, rem: f32) -> bool {
    window.viewport_size().height.as_f32() / rem
        < size::FILE_LIST_FULL_DETAILS.0
}

pub fn list(
    app: &Disktree,
    window: &gpui_kit::Window,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    let compact = compact(window, app.rem);
    let theme = cx.omarchy();
    // Unfiltered navigation needs only the visible page, even in a directory
    // with millions of zero-weight files that the mosaic cannot draw.
    let entries = app.layout_filter().is_some().then(|| app.file_entries());
    let count = entries.as_ref().map_or_else(
        || app.current().map_or(0, |node| node.children().len()),
        Vec::len,
    );
    let page = app.file_page.min(count.saturating_sub(1) / PAGE_SIZE);
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(count);
    let filtered = app.list_filter_note();
    let summary = format!(
        "{} · {} entries · showing {}–{}{filtered}",
        app.current_path().display(),
        count,
        usize::from(count > 0) + start,
        end,
    );
    let mut rows = div().id("file-rows").role(Role::ListBox)
        .track_focus(&app.file_focus)
        .tab_index(0).tab_stop(true)
        .border_2().border_color(theme.background.opacity(0.))
        .focus_visible(|style| style.border_color(theme.accent))
        .aria_label(summary.clone())
        .aria_description("Up/Down select; Enter opens a directory; Backspace returns to its parent; Space marks; c reviews. PageUp/PageDown change pages.")
        .flex().flex_col().flex_1().min_h_0().track_scroll(&app.file_scroll).overflow_y_scroll();
    if count == 0 {
        rows = rows.child(
            crate::semantics::text(
                "empty",
                "No entries in this directory or filter",
            )
            .p(space::LG)
            .child("No entries in this directory or filter"),
        );
    }
    for position in start..end {
        let index = entries
            .as_ref()
            .map_or(position, |entries| entries[position]);
        let mut crumbs = app.crumbs.clone();
        crumbs.push(index);
        let Some(node) = app.node_at(&crumbs) else {
            continue;
        };
        let Some(path) = app.path_at(&crumbs) else {
            continue;
        };
        let selected = app.selected.as_ref() == Some(&crumbs);
        let marked = app.marks.contains(&path, app.path_style());
        let covered = app.marks.ancestor(&path, app.path_style()).is_some();
        let filtered = app
            .matches
            .as_ref()
            .is_some_and(|m| m.keep_at(&crumbs, node).is_none());
        let size = app.entry_size(&crumbs, node);
        // Clone blocks are not what removing the entry frees; say so
        // where the size is read, and nowhere when there are none.
        let split = crate::views::clone_split(node);
        let clones = split
            .as_ref()
            .map_or_else(String::new, |split| format!(" · {split}"));
        let spoken = split
            .as_ref()
            .map_or_else(String::new, |split| format!("; {split}"));
        let label = format!(
            "{}; {}; {} ({} bytes){}; {}; last written {}; {}{}{}",
            path.display(),
            if node.is_dir() { "directory" } else { "file" },
            size,
            node.bytes,
            spoken,
            node.category.label(),
            crate::widgets::ago(crate::state::now_seconds(), node.modified),
            if marked {
                "marked"
            } else if covered {
                "inside a marked directory"
            } else {
                "not marked"
            },
            if node.read_error { "; unreadable" } else { "" },
            if filtered {
                "; does not match filter"
            } else {
                ""
            }
        );
        let row = entry_semantics(&path, label, selected)
            .a11y_synthetic_children(move |b| {
                b.parent_node().set_position_in_set(position + 1);
                b.parent_node().set_size_of_set(count);
            })
            .debug_selector(move || format!("file-row-{position}"))
            .flex_shrink_0()
            .min_h(size::FILE_ENTRY)
            .p(space::SM)
            .border_b_1()
            .border_color(theme.divider())
            .when(selected, |row| row.bg(theme.hover_fill()).border_l_2())
            .child(format!(
                "{}{} · {}{clones} · {}",
                if node.is_dir() { "▸ " } else { "" },
                node.name(),
                size,
                if marked {
                    "Marked"
                } else {
                    node.category.label()
                }
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.pointer_active = false;
                this.select(Some(crumbs.clone()), cx);
                window.focus(&this.file_focus, cx);
            }));
        rows = rows.child(row);
    }
    div()
        .id("file-list")
        .debug_selector(|| "file-list".into())
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(
            crate::semantics::text("file-summary", summary.clone())
                .text_size(text::CAPTION)
                .p(space::SM)
                .when(compact, |header| {
                    header
                        .flex()
                        .items_center()
                        .gap(space::SM)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(
                                    app.current_path().display().to_string(),
                                ),
                        )
                        .child(div().flex_shrink_0().child(format!(
                            "{count} entries · {}–{end}{filtered}",
                            usize::from(count > 0) + start
                        )))
                })
                .when(!compact, |header| header.child(summary)),
        )
        .child(rows)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .when(compact, |actions| {
                    actions.flex_row().flex_wrap().gap(space::SM)
                })
                .child(
                    div()
                        .flex()
                        .gap(space::SM)
                        .child(
                            button(
                                "file-open",
                                if compact {
                                    "Open"
                                } else {
                                    "Open selected directory"
                                },
                                ButtonVariant::Outline,
                                cx,
                            )
                            .accessibility_label("Open selected directory")
                            .min_h(size::POINTER_TARGET)
                            .semantic_disabled(
                                !app.selected
                                    .as_ref()
                                    .and_then(|c| app.node_at(c))
                                    .is_some_and(
                                        disktree_core::tree::Node::is_dir,
                                    ),
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.pointer_active = false;
                                    this.descend(cx);
                                    window.focus(&this.file_focus, cx);
                                },
                            )),
                        )
                        .child(
                            button(
                                "file-mark",
                                if compact {
                                    // Visible words stay inside the
                                    // accessible name, for voice control.
                                    "Mark or unmark"
                                } else {
                                    "Mark or unmark selected path"
                                },
                                ButtonVariant::Outline,
                                cx,
                            )
                            .accessibility_label("Mark or unmark selected path")
                            .min_h(size::POINTER_TARGET)
                            .semantic_disabled(app.selected.is_none())
                            .on_click(cx.listener(
                                |this, _, _, cx| this.toggle_mark_selected(cx),
                            )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(space::SM)
                        .child(
                            button(
                                "file-previous",
                                if compact {
                                    "Previous"
                                } else {
                                    "Previous 100 entries"
                                },
                                ButtonVariant::Outline,
                                cx,
                            )
                            .accessibility_label("Previous 100 entries")
                            .min_h(size::POINTER_TARGET)
                            .debug_selector(|| "file-previous".into())
                            .semantic_disabled(page == 0)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.file_turn_page(-1, cx);
                                    window.focus(&this.file_focus, cx);
                                },
                            )),
                        )
                        .child(
                            button(
                                "file-next",
                                if compact {
                                    "Next"
                                } else {
                                    "Next 100 entries"
                                },
                                ButtonVariant::Outline,
                                cx,
                            )
                            .accessibility_label("Next 100 entries")
                            .min_h(size::POINTER_TARGET)
                            .debug_selector(|| "file-next".into())
                            .semantic_disabled(end == count)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.file_turn_page(1, cx);
                                    window.focus(&this.file_focus, cx);
                                },
                            )),
                        ),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{Element as _, accesskit};

    #[test]
    fn entry_role_selection_and_identity_follow_the_path() {
        let path = Path::new("/Fixtures/Résumé 日本語.txt");
        for selected in [false, true] {
            let element = entry_semantics(
                path,
                "Résumé 日本語.txt; 4096 bytes; marked".into(),
                selected,
            );
            let mut node =
                accesskit::Node::new(element.a11y_role().expect("entry role"));
            element.write_a11y_info(&mut node);
            assert_eq!(node.role(), Role::ListBoxOption);
            assert_eq!(node.is_selected(), Some(selected));
            assert_eq!(
                node.label(),
                Some("Résumé 日本語.txt; 4096 bytes; marked")
            );
            assert_eq!(node.author_id(), Some(path_id(path).as_ref()));
        }
        assert_ne!(path_id(path), path_id(Path::new("/Fixtures/other.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn native_path_ids_do_not_alias_lossy_display_names() {
        use std::os::unix::ffi::OsStrExt as _;
        let a = Path::new(std::ffi::OsStr::from_bytes(b"/Fixtures/\xff"));
        let b = Path::new(std::ffi::OsStr::from_bytes(b"/Fixtures/\xfe"));
        assert_eq!(a.to_string_lossy(), b.to_string_lossy());
        assert_ne!(path_id(a), path_id(b));
    }
}
