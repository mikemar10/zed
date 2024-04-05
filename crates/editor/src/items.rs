use crate::{
    editor_settings::SeedQuerySetting, persistence::DB, Anchor, Autoscroll, Editor, EditorEvent,
    EditorSettings, MultiBuffer, MultiBufferSnapshot, NavigationData, ToPoint as _,
};
use anyhow::{anyhow, Context as _, Result};

use gpui::{
    AnyElement, AppContext, Entity, EntityId, EventEmitter, IntoElement, Model, ParentElement,
    Pixels, SharedString, Styled, Task, View, ViewContext, VisualContext, WeakView, WindowContext,
};
use language::{Bias, Buffer, CharKind, OffsetRangeExt, Point};
use project::repository::GitFileStatus;
use project::{search::SearchQuery, FormatTrigger, Item as _, Project, ProjectPath};
use settings::Settings;
use workspace::item::ItemSettings;

use std::{
    borrow::Cow,
    cmp::{self, Ordering},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use theme::Theme;
use ui::{h_flex, prelude::*, Label};
use util::{paths::PathExt, ResultExt, TryFutureExt};
use workspace::item::BreadcrumbText;
use workspace::{
    item::{Item, ItemEvent, ItemHandle, ProjectItem},
    searchable::{Direction, SearchEvent, SearchableItem, SearchableItemHandle},
    ItemId, ItemNavHistory, Pane, ToolbarItemLocation, Workspace, WorkspaceId,
};

pub const MAX_TAB_TITLE_LEN: usize = 24;

impl Item for Editor {
    type Event = EditorEvent;

    fn navigate(&mut self, data: Box<dyn std::any::Any>, cx: &mut ViewContext<Self>) -> bool {
        if let Ok(data) = data.downcast::<NavigationData>() {
            let newest_selection = self.selections.newest::<Point>(cx);
            let buffer = self.buffer.read(cx).read(cx);
            let offset = if buffer.can_resolve(&data.cursor_anchor) {
                data.cursor_anchor.to_point(&buffer)
            } else {
                buffer.clip_point(data.cursor_position, Bias::Left)
            };

            let mut scroll_anchor = data.scroll_anchor;
            if !buffer.can_resolve(&scroll_anchor.anchor) {
                scroll_anchor.anchor = buffer.anchor_before(
                    buffer.clip_point(Point::new(data.scroll_top_row, 0), Bias::Left),
                );
            }

            drop(buffer);

            if newest_selection.head() == offset {
                false
            } else {
                let nav_history = self.nav_history.take();
                self.set_scroll_anchor(scroll_anchor, cx);
                self.change_selections(Some(Autoscroll::fit()), cx, |s| {
                    s.select_ranges([offset..offset])
                });
                self.nav_history = nav_history;
                true
            }
        } else {
            false
        }
    }

    fn tab_tooltip_text(&self, cx: &AppContext) -> Option<SharedString> {
        let file_path = self
            .buffer()
            .read(cx)
            .as_singleton()?
            .read(cx)
            .file()
            .and_then(|f| f.as_local())?
            .abs_path(cx);

        let file_path = file_path.compact().to_string_lossy().to_string();

        Some(file_path.into())
    }

    fn tab_description(&self, detail: usize, cx: &AppContext) -> Option<SharedString> {
        let path = path_for_buffer(&self.buffer, detail, true, cx)?;
        Some(path.to_string_lossy().to_string().into())
    }

    fn tab_content(&self, detail: Option<usize>, selected: bool, cx: &WindowContext) -> AnyElement {
        let label_color = if ItemSettings::get_global(cx).git_status {
            self.buffer()
                .read(cx)
                .as_singleton()
                .and_then(|buffer| buffer.read(cx).project_path(cx))
                .and_then(|path| self.project.as_ref()?.read(cx).entry_for_path(&path, cx))
                .map(|entry| {
                    entry_git_aware_label_color(entry.git_status, entry.is_ignored, selected)
                })
                .unwrap_or_else(|| entry_label_color(selected))
        } else {
            entry_label_color(selected)
        };

        let description = detail.and_then(|detail| {
            let path = path_for_buffer(&self.buffer, detail, false, cx)?;
            let description = path.to_string_lossy();
            let description = description.trim();

            if description.is_empty() {
                return None;
            }

            Some(util::truncate_and_trailoff(&description, MAX_TAB_TITLE_LEN))
        });

        h_flex()
            .gap_2()
            .child(Label::new(self.title(cx).to_string()).color(label_color))
            .when_some(description, |this, description| {
                this.child(
                    Label::new(description)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn for_each_project_item(
        &self,
        cx: &AppContext,
        f: &mut dyn FnMut(EntityId, &dyn project::Item),
    ) {
        self.buffer
            .read(cx)
            .for_each_buffer(|buffer| f(buffer.entity_id(), buffer.read(cx)));
    }

    fn is_singleton(&self, cx: &AppContext) -> bool {
        self.buffer.read(cx).is_singleton()
    }

    fn clone_on_split(
        &self,
        _workspace_id: WorkspaceId,
        cx: &mut ViewContext<Self>,
    ) -> Option<View<Editor>>
    where
        Self: Sized,
    {
        Some(cx.new_view(|cx| self.clone(cx)))
    }

    fn set_nav_history(&mut self, history: ItemNavHistory, _: &mut ViewContext<Self>) {
        self.nav_history = Some(history);
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        let selection = self.selections.newest_anchor();
        self.push_to_nav_history(selection.head(), None, cx);
    }

    fn workspace_deactivated(&mut self, cx: &mut ViewContext<Self>) {
        self.hide_hovered_link(cx);
    }

    fn is_dirty(&self, cx: &AppContext) -> bool {
        self.buffer().read(cx).read(cx).is_dirty()
    }

    fn has_conflict(&self, cx: &AppContext) -> bool {
        self.buffer().read(cx).read(cx).has_conflict()
    }

    fn can_save(&self, cx: &AppContext) -> bool {
        let buffer = &self.buffer().read(cx);
        if let Some(buffer) = buffer.as_singleton() {
            buffer.read(cx).project_path(cx).is_some()
        } else {
            true
        }
    }

    fn save(
        &mut self,
        format: bool,
        project: Model<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        let buffers = self.buffer().clone().read(cx).all_buffers();
        cx.spawn(|this, mut cx| async move {
            if format {
                this.update(&mut cx, |editor, cx| {
                    editor.perform_format(project.clone(), FormatTrigger::Save, cx)
                })?
                .await?;
            }

            // Only format and save the buffers with changes. For clean buffers,
            // we simulate saving by calling `Buffer::did_save`, so that language servers or
            // other downstream listeners of save events get notified.
            let (dirty_buffers, clean_buffers) = buffers.into_iter().partition(|buffer| {
                buffer
                    .update(&mut cx, |buffer, _| {
                        buffer.is_dirty() || buffer.has_conflict()
                    })
                    .unwrap_or(false)
            });

            project
                .update(&mut cx, |project, cx| {
                    project.save_buffers(dirty_buffers, cx)
                })?
                .await?;
            for buffer in clean_buffers {
                buffer
                    .update(&mut cx, |buffer, cx| {
                        let version = buffer.saved_version().clone();
                        let fingerprint = buffer.saved_version_fingerprint();
                        let mtime = buffer.saved_mtime();
                        buffer.did_save(version, fingerprint, mtime, cx);
                    })
                    .ok();
            }

            Ok(())
        })
    }

    fn save_as(
        &mut self,
        project: Model<Project>,
        abs_path: PathBuf,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        let buffer = self
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("cannot call save_as on an excerpt list");

        project.update(cx, |project, cx| {
            project.save_buffer_as(buffer, abs_path, cx)
        })
    }

    fn reload(&mut self, project: Model<Project>, cx: &mut ViewContext<Self>) -> Task<Result<()>> {
        let buffer = self.buffer().clone();
        let buffers = self.buffer.read(cx).all_buffers();
        let reload_buffers =
            project.update(cx, |project, cx| project.reload_buffers(buffers, true, cx));
        cx.spawn(|this, mut cx| async move {
            let transaction = reload_buffers.log_err().await;
            this.update(&mut cx, |editor, cx| {
                editor.request_autoscroll(Autoscroll::fit(), cx)
            })?;
            buffer
                .update(&mut cx, |buffer, cx| {
                    if let Some(transaction) = transaction {
                        if !buffer.is_singleton() {
                            buffer.push_transaction(&transaction.0, cx);
                        }
                    }
                })
                .ok();
            Ok(())
        })
    }

    fn as_searchable(&self, handle: &View<Self>) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn pixel_position_of_cursor(&self, _: &AppContext) -> Option<gpui::Point<Pixels>> {
        self.pixel_position_of_newest_cursor
    }

    fn breadcrumb_location(&self) -> ToolbarItemLocation {
        if self.show_breadcrumbs {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, variant: &Theme, cx: &AppContext) -> Option<Vec<BreadcrumbText>> {
        let cursor = self.selections.newest_anchor().head();
        let multibuffer = &self.buffer().read(cx);
        let (buffer_id, symbols) =
            multibuffer.symbols_containing(cursor, Some(&variant.syntax()), cx)?;
        let buffer = multibuffer.buffer(buffer_id)?;

        let buffer = buffer.read(cx);
        let filename = buffer
            .snapshot()
            .resolve_file_path(
                cx,
                self.project
                    .as_ref()
                    .map(|project| project.read(cx).visible_worktrees(cx).count() > 1)
                    .unwrap_or_default(),
            )
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".to_string());

        let mut breadcrumbs = vec![BreadcrumbText {
            text: filename,
            highlights: None,
        }];
        breadcrumbs.extend(symbols.into_iter().map(|symbol| BreadcrumbText {
            text: symbol.text,
            highlights: Some(symbol.highlight_ranges),
        }));
        Some(breadcrumbs)
    }

    fn added_to_workspace(&mut self, workspace: &mut Workspace, cx: &mut ViewContext<Self>) {
        let workspace_id = workspace.database_id();
        let item_id = cx.view().item_id().as_u64() as ItemId;
        self.workspace = Some((workspace.weak_handle(), workspace.database_id()));

        fn serialize(
            buffer: Model<Buffer>,
            workspace_id: WorkspaceId,
            item_id: ItemId,
            cx: &mut AppContext,
        ) {
            if let Some(file) = buffer.read(cx).file().and_then(|file| file.as_local()) {
                let path = file.abs_path(cx);

                cx.background_executor()
                    .spawn(async move {
                        DB.save_path(item_id, workspace_id, path.clone())
                            .await
                            .log_err()
                    })
                    .detach();
            }
        }

        if let Some(buffer) = self.buffer().read(cx).as_singleton() {
            serialize(buffer.clone(), workspace_id, item_id, cx);

            cx.subscribe(&buffer, |this, buffer, event, cx| {
                if let Some((_, workspace_id)) = this.workspace.as_ref() {
                    if let language::Event::FileHandleChanged = event {
                        serialize(
                            buffer,
                            *workspace_id,
                            cx.view().item_id().as_u64() as ItemId,
                            cx,
                        );
                    }
                }
            })
            .detach();
        }
    }

    fn serialized_item_kind() -> Option<&'static str> {
        Some("Editor")
    }

    fn to_item_events(event: &EditorEvent, mut f: impl FnMut(ItemEvent)) {
        match event {
            EditorEvent::Closed => f(ItemEvent::CloseItem),

            EditorEvent::Saved | EditorEvent::TitleChanged => {
                f(ItemEvent::UpdateTab);
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::Reparsed => {
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::SelectionsChanged { local } if *local => {
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::DirtyChanged => {
                f(ItemEvent::UpdateTab);
            }

            EditorEvent::BufferEdited => {
                f(ItemEvent::Edit);
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::ExcerptsAdded { .. } | EditorEvent::ExcerptsRemoved { .. } => {
                f(ItemEvent::Edit);
            }

            _ => {}
        }
    }

    fn deserialize(
        project: Model<Project>,
        _workspace: WeakView<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: ItemId,
        cx: &mut ViewContext<Pane>,
    ) -> Task<Result<View<Self>>> {
        let project_item: Result<_> = project.update(cx, |project, cx| {
            // Look up the path with this key associated, create a self with that path
            let path = DB
                .get_path(item_id, workspace_id)?
                .context("No path stored for this editor")?;

            let (worktree, path) = project
                .find_local_worktree(&path, cx)
                .with_context(|| format!("No worktree for path: {path:?}"))?;
            let project_path = ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: path.into(),
            };

            Ok(project.open_path(project_path, cx))
        });

        project_item
            .map(|project_item| {
                cx.spawn(|pane, mut cx| async move {
                    let (_, project_item) = project_item.await?;
                    let buffer = project_item
                        .downcast::<Buffer>()
                        .map_err(|_| anyhow!("Project item at stored path was not a buffer"))?;
                    pane.update(&mut cx, |_, cx| {
                        cx.new_view(|cx| {
                            let mut editor = Editor::for_buffer(buffer, Some(project), cx);

                            editor.read_scroll_position_from_db(item_id, workspace_id, cx);
                            editor
                        })
                    })
                })
            })
            .unwrap_or_else(|error| Task::ready(Err(error)))
    }
}

impl ProjectItem for Editor {
    type Item = Buffer;

    fn for_project_item(
        project: Model<Project>,
        buffer: Model<Buffer>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        Self::for_buffer(buffer, Some(project), cx)
    }
}

impl EventEmitter<SearchEvent> for Editor {}

pub(crate) enum BufferSearchHighlights {}
impl SearchableItem for Editor {
    type Match = Range<Anchor>;

    fn clear_matches(&mut self, cx: &mut ViewContext<Self>) {
        self.clear_background_highlights::<BufferSearchHighlights>(cx);
    }

    fn update_matches(&mut self, matches: Vec<Range<Anchor>>, cx: &mut ViewContext<Self>) {
        self.highlight_background::<BufferSearchHighlights>(
            matches,
            |theme| theme.search_match_background,
            cx,
        );
    }

    fn query_suggestion(&mut self, cx: &mut ViewContext<Self>) -> String {
        let setting = EditorSettings::get_global(cx).seed_search_query_from_cursor;
        let snapshot = &self.snapshot(cx).buffer_snapshot;
        let selection = self.selections.newest::<usize>(cx);

        match setting {
            SeedQuerySetting::Never => String::new(),
            SeedQuerySetting::Selection | SeedQuerySetting::Always if !selection.is_empty() => {
                snapshot
                    .text_for_range(selection.start..selection.end)
                    .collect()
            }
            SeedQuerySetting::Selection => String::new(),
            SeedQuerySetting::Always => {
                let (range, kind) = snapshot.surrounding_word(selection.start);
                if kind == Some(CharKind::Word) {
                    let text: String = snapshot.text_for_range(range).collect();
                    if !text.trim().is_empty() {
                        return text;
                    }
                }
                String::new()
            }
        }
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: Vec<Range<Anchor>>,
        cx: &mut ViewContext<Self>,
    ) {
        self.unfold_ranges([matches[index].clone()], false, true, cx);
        let range = self.range_for_match(&matches[index]);
        self.change_selections(Some(Autoscroll::fit()), cx, |s| {
            s.select_ranges([range]);
        })
    }

    fn select_matches(&mut self, matches: Vec<Self::Match>, cx: &mut ViewContext<Self>) {
        self.unfold_ranges(matches.clone(), false, false, cx);
        let mut ranges = Vec::new();
        for m in &matches {
            ranges.push(self.range_for_match(&m))
        }
        self.change_selections(None, cx, |s| s.select_ranges(ranges));
    }
    fn replace(
        &mut self,
        identifier: &Self::Match,
        query: &SearchQuery,
        cx: &mut ViewContext<Self>,
    ) {
        let text = self.buffer.read(cx);
        let text = text.snapshot(cx);
        let text = text.text_for_range(identifier.clone()).collect::<Vec<_>>();
        let text: Cow<_> = if text.len() == 1 {
            text.first().cloned().unwrap().into()
        } else {
            let joined_chunks = text.join("");
            joined_chunks.into()
        };

        if let Some(replacement) = query.replacement_for(&text) {
            self.transact(cx, |this, cx| {
                this.edit([(identifier.clone(), Arc::from(&*replacement))], cx);
            });
        }
    }
    fn match_index_for_direction(
        &mut self,
        matches: &Vec<Range<Anchor>>,
        current_index: usize,
        direction: Direction,
        count: usize,
        cx: &mut ViewContext<Self>,
    ) -> usize {
        let buffer = self.buffer().read(cx).snapshot(cx);
        let current_index_position = if self.selections.disjoint_anchors().len() == 1 {
            self.selections.newest_anchor().head()
        } else {
            matches[current_index].start
        };

        let mut count = count % matches.len();
        if count == 0 {
            return current_index;
        }
        match direction {
            Direction::Next => {
                if matches[current_index]
                    .start
                    .cmp(&current_index_position, &buffer)
                    .is_gt()
                {
                    count = count - 1
                }

                (current_index + count) % matches.len()
            }
            Direction::Prev => {
                if matches[current_index]
                    .end
                    .cmp(&current_index_position, &buffer)
                    .is_lt()
                {
                    count = count - 1;
                }

                if current_index >= count {
                    current_index - count
                } else {
                    matches.len() - (count - current_index)
                }
            }
        }
    }

    fn find_matches(
        &mut self,
        query: Arc<project::search::SearchQuery>,
        cx: &mut ViewContext<Self>,
    ) -> Task<Vec<Range<Anchor>>> {
        let buffer = self.buffer().read(cx).snapshot(cx);
        cx.background_executor().spawn(async move {
            let mut ranges = Vec::new();
            if let Some((_, _, excerpt_buffer)) = buffer.as_singleton() {
                ranges.extend(
                    query
                        .search(excerpt_buffer, None)
                        .await
                        .into_iter()
                        .map(|range| {
                            buffer.anchor_after(range.start)..buffer.anchor_before(range.end)
                        }),
                );
            } else {
                for excerpt in buffer.excerpt_boundaries_in_range(0..buffer.len()) {
                    let excerpt_range = excerpt.range.context.to_offset(&excerpt.buffer);
                    ranges.extend(
                        query
                            .search(&excerpt.buffer, Some(excerpt_range.clone()))
                            .await
                            .into_iter()
                            .map(|range| {
                                let start = excerpt
                                    .buffer
                                    .anchor_after(excerpt_range.start + range.start);
                                let end = excerpt
                                    .buffer
                                    .anchor_before(excerpt_range.start + range.end);
                                buffer.anchor_in_excerpt(excerpt.id, start).unwrap()
                                    ..buffer.anchor_in_excerpt(excerpt.id, end).unwrap()
                            }),
                    );
                }
            }
            ranges
        })
    }

    fn active_match_index(
        &mut self,
        matches: Vec<Range<Anchor>>,
        cx: &mut ViewContext<Self>,
    ) -> Option<usize> {
        active_match_index(
            &matches,
            &self.selections.newest_anchor().head(),
            &self.buffer().read(cx).snapshot(cx),
        )
    }
}

pub fn active_match_index(
    ranges: &[Range<Anchor>],
    cursor: &Anchor,
    buffer: &MultiBufferSnapshot,
) -> Option<usize> {
    if ranges.is_empty() {
        None
    } else {
        match ranges.binary_search_by(|probe| {
            if probe.end.cmp(cursor, buffer).is_lt() {
                Ordering::Less
            } else if probe.start.cmp(cursor, buffer).is_gt() {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }) {
            Ok(i) | Err(i) => Some(cmp::min(i, ranges.len() - 1)),
        }
    }
}

pub fn entry_label_color(selected: bool) -> Color {
    if selected {
        Color::Default
    } else {
        Color::Muted
    }
}

pub fn entry_git_aware_label_color(
    git_status: Option<GitFileStatus>,
    ignored: bool,
    selected: bool,
) -> Color {
    if ignored {
        Color::Disabled
    } else {
        match git_status {
            Some(GitFileStatus::Added) => Color::Created,
            Some(GitFileStatus::Modified) => Color::Modified,
            Some(GitFileStatus::Conflict) => Color::Conflict,
            None => entry_label_color(selected),
        }
    }
}

fn path_for_buffer<'a>(
    buffer: &Model<MultiBuffer>,
    height: usize,
    include_filename: bool,
    cx: &'a AppContext,
) -> Option<Cow<'a, Path>> {
    let file = buffer.read(cx).as_singleton()?.read(cx).file()?;
    path_for_file(file.as_ref(), height, include_filename, cx)
}

fn path_for_file<'a>(
    file: &'a dyn language::File,
    mut height: usize,
    include_filename: bool,
    cx: &'a AppContext,
) -> Option<Cow<'a, Path>> {
    // Ensure we always render at least the filename.
    height += 1;

    let mut prefix = file.path().as_ref();
    while height > 0 {
        if let Some(parent) = prefix.parent() {
            prefix = parent;
            height -= 1;
        } else {
            break;
        }
    }

    // Here we could have just always used `full_path`, but that is very
    // allocation-heavy and so we try to use a `Cow<Path>` if we haven't
    // traversed all the way up to the worktree's root.
    if height > 0 {
        let full_path = file.full_path(cx);
        if include_filename {
            Some(full_path.into())
        } else {
            Some(full_path.parent()?.to_path_buf().into())
        }
    } else {
        let mut path = file.path().strip_prefix(prefix).ok()?;
        if !include_filename {
            path = path.parent()?;
        }
        Some(path.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;
    use language::TestFile;
    use std::path::Path;

    #[gpui::test]
    fn test_path_for_file(cx: &mut AppContext) {
        let file = TestFile {
            path: Path::new("").into(),
            root_name: String::new(),
        };
        assert_eq!(path_for_file(&file, 0, false, cx), None);
    }
}
