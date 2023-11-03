use crate::{
    display_map::ToDisplayPoint, link_go_to_definition::hide_link_definition,
    movement::surrounding_word, persistence::DB, scroll::ScrollAnchor, Anchor, Autoscroll, Editor,
    Event, ExcerptId, ExcerptRange, MultiBuffer, MultiBufferSnapshot, NavigationData, ToPoint as _,
};
use anyhow::{Context, Result};
use collections::HashSet;
use editor::{BufferSearchHighlights, MAX_TAB_TITLE_LEN};
use futures::future::try_join_all;
use gpui::{
    elements::*,
    geometry::vector::{vec2f, Vector2F},
    AppContext, AsyncAppContext, Entity, ModelHandle, Subscription, Task, View, ViewContext,
    ViewHandle, WeakViewHandle,
};
use language::{
    proto::serialize_anchor as serialize_text_anchor, Bias, Buffer, OffsetRangeExt, Point,
    SelectionGoal,
};
use project::{search::SearchQuery, FormatTrigger, Item as _, Project, ProjectPath};
use rpc::proto::{self, update_view, PeerId};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    cmp::{self, Ordering},
    fmt::Write,
    iter,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use text::Selection;
use util::{
    paths::{PathExt, FILE_ROW_COLUMN_DELIMITER},
    ResultExt, TryFutureExt,
};
use workspace::item::{BreadcrumbText, FollowableItemHandle};
use workspace::{
    item::{FollowableItem, Item, ItemEvent, ItemHandle, ProjectItem},
    searchable::{Direction, SearchEvent, SearchableItem, SearchableItemHandle},
    ItemId, ItemNavHistory, Pane, StatusItemView, ToolbarItemLocation, ViewId, Workspace,
    WorkspaceId,
};

impl FollowableItem for Editor {
    fn remote_id(&self) -> Option<ViewId> {
        self.remote_id
    }

    fn from_state_proto(
        pane: ViewHandle<workspace::Pane>,
        workspace: ViewHandle<Workspace>,
        remote_id: ViewId,
        state: &mut Option<proto::view::Variant>,
        cx: &mut AppContext,
    ) -> Option<Task<Result<ViewHandle<Self>>>> {
        let project = workspace.read(cx).project().to_owned();
        let Some(proto::view::Variant::Editor(_)) = state else {
            return None;
        };
        let Some(proto::view::Variant::Editor(state)) = state.take() else {
            unreachable!()
        };

        let client = project.read(cx).client();
        let replica_id = project.read(cx).replica_id();
        let buffer_ids = state
            .excerpts
            .iter()
            .map(|excerpt| excerpt.buffer_id)
            .collect::<HashSet<_>>();
        let buffers = project.update(cx, |project, cx| {
            buffer_ids
                .iter()
                .map(|id| project.open_buffer_by_id(*id, cx))
                .collect::<Vec<_>>()
        });

        let pane = pane.downgrade();
        Some(cx.spawn(|mut cx| async move {
            let mut buffers = futures::future::try_join_all(buffers).await?;
            let editor = pane.read_with(&cx, |pane, cx| {
                let mut editors = pane.items_of_type::<Self>();
                editors.find(|editor| {
                    let ids_match = editor.remote_id(&client, cx) == Some(remote_id);
                    let singleton_buffer_matches = state.singleton
                        && buffers.first()
                            == editor.read(cx).buffer.read(cx).as_singleton().as_ref();
                    ids_match || singleton_buffer_matches
                })
            })?;

            let editor = if let Some(editor) = editor {
                editor
            } else {
                pane.update(&mut cx, |_, cx| {
                    let multibuffer = cx.add_model(|cx| {
                        let mut multibuffer;
                        if state.singleton && buffers.len() == 1 {
                            multibuffer = MultiBuffer::singleton(buffers.pop().unwrap(), cx)
                        } else {
                            multibuffer = MultiBuffer::new(replica_id);
                            let mut excerpts = state.excerpts.into_iter().peekable();
                            while let Some(excerpt) = excerpts.peek() {
                                let buffer_id = excerpt.buffer_id;
                                let buffer_excerpts = iter::from_fn(|| {
                                    let excerpt = excerpts.peek()?;
                                    (excerpt.buffer_id == buffer_id)
                                        .then(|| excerpts.next().unwrap())
                                });
                                let buffer =
                                    buffers.iter().find(|b| b.read(cx).remote_id() == buffer_id);
                                if let Some(buffer) = buffer {
                                    multibuffer.push_excerpts(
                                        buffer.clone(),
                                        buffer_excerpts.filter_map(deserialize_excerpt_range),
                                        cx,
                                    );
                                }
                            }
                        };

                        if let Some(title) = &state.title {
                            multibuffer = multibuffer.with_title(title.clone())
                        }

                        multibuffer
                    });

                    cx.add_view(|cx| {
                        let mut editor = Editor::for_multibuffer(
                            multibuffer,
                            Some(Arc::new(project.clone())),
                            cx,
                        );
                        editor.remote_id = Some(remote_id);
                        editor
                    })
                })?
            };

            update_editor_from_message(
                editor.downgrade(),
                project,
                proto::update_view::Editor {
                    selections: state.selections,
                    pending_selection: state.pending_selection,
                    scroll_top_anchor: state.scroll_top_anchor,
                    scroll_x: state.scroll_x,
                    scroll_y: state.scroll_y,
                    ..Default::default()
                },
                &mut cx,
            )
            .await?;

            Ok(editor)
        }))
    }

    fn set_leader_peer_id(&mut self, leader_peer_id: Option<PeerId>, cx: &mut ViewContext<Self>) {
        self.leader_peer_id = leader_peer_id;
        if self.leader_peer_id.is_some() {
            self.buffer.update(cx, |buffer, cx| {
                buffer.remove_active_selections(cx);
            });
        } else {
            self.buffer.update(cx, |buffer, cx| {
                if self.focused {
                    buffer.set_active_selections(
                        &self.selections.disjoint_anchors(),
                        self.selections.line_mode,
                        self.cursor_shape,
                        cx,
                    );
                }
            });
        }
        cx.notify();
    }

    fn to_state_proto(&self, cx: &AppContext) -> Option<proto::view::Variant> {
        let buffer = self.buffer.read(cx);
        let scroll_anchor = self.scroll_manager.anchor();
        let excerpts = buffer
            .read(cx)
            .excerpts()
            .map(|(id, buffer, range)| proto::Excerpt {
                id: id.to_proto(),
                buffer_id: buffer.remote_id(),
                context_start: Some(serialize_text_anchor(&range.context.start)),
                context_end: Some(serialize_text_anchor(&range.context.end)),
                primary_start: range
                    .primary
                    .as_ref()
                    .map(|range| serialize_text_anchor(&range.start)),
                primary_end: range
                    .primary
                    .as_ref()
                    .map(|range| serialize_text_anchor(&range.end)),
            })
            .collect();

        Some(proto::view::Variant::Editor(proto::view::Editor {
            singleton: buffer.is_singleton(),
            title: (!buffer.is_singleton()).then(|| buffer.title(cx).into()),
            excerpts,
            scroll_top_anchor: Some(serialize_anchor(&scroll_anchor.anchor)),
            scroll_x: scroll_anchor.offset.x(),
            scroll_y: scroll_anchor.offset.y(),
            selections: self
                .selections
                .disjoint_anchors()
                .iter()
                .map(serialize_selection)
                .collect(),
            pending_selection: self
                .selections
                .pending_anchor()
                .as_ref()
                .map(serialize_selection),
        }))
    }

    fn add_event_to_update_proto(
        &self,
        event: &Self::Event,
        update: &mut Option<proto::update_view::Variant>,
        cx: &AppContext,
    ) -> bool {
        let update =
            update.get_or_insert_with(|| proto::update_view::Variant::Editor(Default::default()));

        match update {
            proto::update_view::Variant::Editor(update) => match event {
                Event::ExcerptsAdded {
                    buffer,
                    predecessor,
                    excerpts,
                } => {
                    let buffer_id = buffer.read(cx).remote_id();
                    let mut excerpts = excerpts.iter();
                    if let Some((id, range)) = excerpts.next() {
                        update.inserted_excerpts.push(proto::ExcerptInsertion {
                            previous_excerpt_id: Some(predecessor.to_proto()),
                            excerpt: serialize_excerpt(buffer_id, id, range),
                        });
                        update.inserted_excerpts.extend(excerpts.map(|(id, range)| {
                            proto::ExcerptInsertion {
                                previous_excerpt_id: None,
                                excerpt: serialize_excerpt(buffer_id, id, range),
                            }
                        }))
                    }
                    true
                }
                Event::ExcerptsRemoved { ids } => {
                    update
                        .deleted_excerpts
                        .extend(ids.iter().map(ExcerptId::to_proto));
                    true
                }
                Event::ScrollPositionChanged { .. } => {
                    let scroll_anchor = self.scroll_manager.anchor();
                    update.scroll_top_anchor = Some(serialize_anchor(&scroll_anchor.anchor));
                    update.scroll_x = scroll_anchor.offset.x();
                    update.scroll_y = scroll_anchor.offset.y();
                    true
                }
                Event::SelectionsChanged { .. } => {
                    update.selections = self
                        .selections
                        .disjoint_anchors()
                        .iter()
                        .map(serialize_selection)
                        .collect();
                    update.pending_selection = self
                        .selections
                        .pending_anchor()
                        .as_ref()
                        .map(serialize_selection);
                    true
                }
                _ => false,
            },
        }
    }

    fn apply_update_proto(
        &mut self,
        project: &ModelHandle<Project>,
        message: update_view::Variant,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        let update_view::Variant::Editor(message) = message;
        let project = project.clone();
        cx.spawn(|this, mut cx| async move {
            update_editor_from_message(this, project, message, &mut cx).await
        })
    }

    fn should_unfollow_on_event(event: &Self::Event, _: &AppContext) -> bool {
        match event {
            Event::Edited => true,
            Event::SelectionsChanged { local } => *local,
            Event::ScrollPositionChanged { local, .. } => *local,
            _ => false,
        }
    }

    fn is_project_item(&self, _cx: &AppContext) -> bool {
        true
    }
}

async fn update_editor_from_message(
    this: WeakViewHandle<Editor>,
    project: ModelHandle<Project>,
    message: proto::update_view::Editor,
    cx: &mut AsyncAppContext,
) -> Result<()> {
    // Open all of the buffers of which excerpts were added to the editor.
    let inserted_excerpt_buffer_ids = message
        .inserted_excerpts
        .iter()
        .filter_map(|insertion| Some(insertion.excerpt.as_ref()?.buffer_id))
        .collect::<HashSet<_>>();
    let inserted_excerpt_buffers = project.update(cx, |project, cx| {
        inserted_excerpt_buffer_ids
            .into_iter()
            .map(|id| project.open_buffer_by_id(id, cx))
            .collect::<Vec<_>>()
    });
    let _inserted_excerpt_buffers = try_join_all(inserted_excerpt_buffers).await?;

    // Update the editor's excerpts.
    this.update(cx, |editor, cx| {
        editor.buffer.update(cx, |multibuffer, cx| {
            let mut removed_excerpt_ids = message
                .deleted_excerpts
                .into_iter()
                .map(ExcerptId::from_proto)
                .collect::<Vec<_>>();
            removed_excerpt_ids.sort_by({
                let multibuffer = multibuffer.read(cx);
                move |a, b| a.cmp(&b, &multibuffer)
            });

            let mut insertions = message.inserted_excerpts.into_iter().peekable();
            while let Some(insertion) = insertions.next() {
                let Some(excerpt) = insertion.excerpt else {
                    continue;
                };
                let Some(previous_excerpt_id) = insertion.previous_excerpt_id else {
                    continue;
                };
                let buffer_id = excerpt.buffer_id;
                let Some(buffer) = project.read(cx).buffer_for_id(buffer_id, cx) else {
                    continue;
                };

                let adjacent_excerpts = iter::from_fn(|| {
                    let insertion = insertions.peek()?;
                    if insertion.previous_excerpt_id.is_none()
                        && insertion.excerpt.as_ref()?.buffer_id == buffer_id
                    {
                        insertions.next()?.excerpt
                    } else {
                        None
                    }
                });

                multibuffer.insert_excerpts_with_ids_after(
                    ExcerptId::from_proto(previous_excerpt_id),
                    buffer,
                    [excerpt]
                        .into_iter()
                        .chain(adjacent_excerpts)
                        .filter_map(|excerpt| {
                            Some((
                                ExcerptId::from_proto(excerpt.id),
                                deserialize_excerpt_range(excerpt)?,
                            ))
                        }),
                    cx,
                );
            }

            multibuffer.remove_excerpts(removed_excerpt_ids, cx);
        });
    })?;

    // Deserialize the editor state.
    let (selections, pending_selection, scroll_top_anchor) = this.update(cx, |editor, cx| {
        let buffer = editor.buffer.read(cx).read(cx);
        let selections = message
            .selections
            .into_iter()
            .filter_map(|selection| deserialize_selection(&buffer, selection))
            .collect::<Vec<_>>();
        let pending_selection = message
            .pending_selection
            .and_then(|selection| deserialize_selection(&buffer, selection));
        let scroll_top_anchor = message
            .scroll_top_anchor
            .and_then(|anchor| deserialize_anchor(&buffer, anchor));
        anyhow::Ok((selections, pending_selection, scroll_top_anchor))
    })??;

    // Wait until the buffer has received all of the operations referenced by
    // the editor's new state.
    this.update(cx, |editor, cx| {
        editor.buffer.update(cx, |buffer, cx| {
            buffer.wait_for_anchors(
                selections
                    .iter()
                    .chain(pending_selection.as_ref())
                    .flat_map(|selection| [selection.start, selection.end])
                    .chain(scroll_top_anchor),
                cx,
            )
        })
    })?
    .await?;

    // Update the editor's state.
    this.update(cx, |editor, cx| {
        if !selections.is_empty() || pending_selection.is_some() {
            editor.set_selections_from_remote(selections, pending_selection, cx);
            editor.request_autoscroll_remotely(Autoscroll::newest(), cx);
        } else if let Some(scroll_top_anchor) = scroll_top_anchor {
            editor.set_scroll_anchor_remote(
                ScrollAnchor {
                    anchor: scroll_top_anchor,
                    offset: vec2f(message.scroll_x, message.scroll_y),
                },
                cx,
            );
        }
    })?;
    Ok(())
}

fn serialize_excerpt(
    buffer_id: u64,
    id: &ExcerptId,
    range: &ExcerptRange<language::Anchor>,
) -> Option<proto::Excerpt> {
    Some(proto::Excerpt {
        id: id.to_proto(),
        buffer_id,
        context_start: Some(serialize_text_anchor(&range.context.start)),
        context_end: Some(serialize_text_anchor(&range.context.end)),
        primary_start: range
            .primary
            .as_ref()
            .map(|r| serialize_text_anchor(&r.start)),
        primary_end: range
            .primary
            .as_ref()
            .map(|r| serialize_text_anchor(&r.end)),
    })
}

fn serialize_selection(selection: &Selection<Anchor>) -> proto::Selection {
    proto::Selection {
        id: selection.id as u64,
        start: Some(serialize_anchor(&selection.start)),
        end: Some(serialize_anchor(&selection.end)),
        reversed: selection.reversed,
    }
}

fn serialize_anchor(anchor: &Anchor) -> proto::EditorAnchor {
    proto::EditorAnchor {
        excerpt_id: anchor.excerpt_id.to_proto(),
        anchor: Some(serialize_text_anchor(&anchor.text_anchor)),
    }
}

fn deserialize_excerpt_range(excerpt: proto::Excerpt) -> Option<ExcerptRange<language::Anchor>> {
    let context = {
        let start = language::proto::deserialize_anchor(excerpt.context_start?)?;
        let end = language::proto::deserialize_anchor(excerpt.context_end?)?;
        start..end
    };
    let primary = excerpt
        .primary_start
        .zip(excerpt.primary_end)
        .and_then(|(start, end)| {
            let start = language::proto::deserialize_anchor(start)?;
            let end = language::proto::deserialize_anchor(end)?;
            Some(start..end)
        });
    Some(ExcerptRange { context, primary })
}

fn deserialize_selection(
    buffer: &MultiBufferSnapshot,
    selection: proto::Selection,
) -> Option<Selection<Anchor>> {
    Some(Selection {
        id: selection.id as usize,
        start: deserialize_anchor(buffer, selection.start?)?,
        end: deserialize_anchor(buffer, selection.end?)?,
        reversed: selection.reversed,
        goal: SelectionGoal::None,
    })
}

fn deserialize_anchor(buffer: &MultiBufferSnapshot, anchor: proto::EditorAnchor) -> Option<Anchor> {
    let excerpt_id = ExcerptId::from_proto(anchor.excerpt_id);
    Some(Anchor {
        excerpt_id,
        text_anchor: language::proto::deserialize_anchor(anchor.anchor?)?,
        buffer_id: buffer.buffer_id_for_excerpt(excerpt_id),
    })
}

impl Item for Editor {
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

    fn tab_tooltip_text(&self, cx: &AppContext) -> Option<Cow<str>> {
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

    fn tab_description<'a>(&'a self, detail: usize, cx: &'a AppContext) -> Option<Cow<str>> {
        match path_for_buffer(&self.buffer, detail, true, cx)? {
            Cow::Borrowed(path) => Some(path.to_string_lossy()),
            Cow::Owned(path) => Some(path.to_string_lossy().to_string().into()),
        }
    }

    fn tab_content<T: 'static>(
        &self,
        detail: Option<usize>,
        style: &theme::Tab,
        cx: &AppContext,
    ) -> AnyElement<T> {
        Flex::row()
            .with_child(Label::new(self.title(cx).to_string(), style.label.clone()).into_any())
            .with_children(detail.and_then(|detail| {
                let path = path_for_buffer(&self.buffer, detail, false, cx)?;
                let description = path.to_string_lossy();
                Some(
                    Label::new(
                        util::truncate_and_trailoff(&description, MAX_TAB_TITLE_LEN),
                        style.description.text.clone(),
                    )
                    .contained()
                    .with_style(style.description.container)
                    .aligned(),
                )
            }))
            .align_children_center()
            .into_any()
    }

    fn for_each_project_item(&self, cx: &AppContext, f: &mut dyn FnMut(usize, &dyn project::Item)) {
        self.buffer
            .read(cx)
            .for_each_buffer(|buffer| f(buffer.id(), buffer.read(cx)));
    }

    fn is_singleton(&self, cx: &AppContext) -> bool {
        self.buffer.read(cx).is_singleton()
    }

    fn clone_on_split(&self, _workspace_id: WorkspaceId, cx: &mut ViewContext<Self>) -> Option<Self>
    where
        Self: Sized,
    {
        Some(self.clone(cx))
    }

    fn set_nav_history(&mut self, history: ItemNavHistory, _: &mut ViewContext<Self>) {
        self.nav_history = Some(history);
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        let selection = self.selections.newest_anchor();
        self.push_to_nav_history(selection.head(), None, cx);
    }

    fn workspace_deactivated(&mut self, cx: &mut ViewContext<Self>) {
        hide_link_definition(self, cx);
        self.link_go_to_definition_state.last_trigger_point = None;
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
        project: ModelHandle<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        self.report_editor_event("save", None, cx);
        let format = self.perform_format(Arc::new(project.clone()), FormatTrigger::Save, cx);
        let buffers = self.buffer().clone().read(cx).all_buffers();
        cx.spawn(|_, mut cx| async move {
            format.await?;

            if buffers.len() == 1 {
                project
                    .update(&mut cx, |project, cx| project.save_buffers(buffers, cx))
                    .await?;
            } else {
                // For multi-buffers, only save those ones that contain changes. For clean buffers
                // we simulate saving by calling `Buffer::did_save`, so that language servers or
                // other downstream listeners of save events get notified.
                let (dirty_buffers, clean_buffers) = buffers.into_iter().partition(|buffer| {
                    buffer.read_with(&cx, |buffer, _| buffer.is_dirty() || buffer.has_conflict())
                });

                project
                    .update(&mut cx, |project, cx| {
                        project.save_buffers(dirty_buffers, cx)
                    })
                    .await?;
                for buffer in clean_buffers {
                    buffer.update(&mut cx, |buffer, cx| {
                        let version = buffer.saved_version().clone();
                        let fingerprint = buffer.saved_version_fingerprint();
                        let mtime = buffer.saved_mtime();
                        buffer.did_save(version, fingerprint, mtime, cx);
                    });
                }
            }

            Ok(())
        })
    }

    fn save_as(
        &mut self,
        project: ModelHandle<Project>,
        abs_path: PathBuf,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        let buffer = self
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("cannot call save_as on an excerpt list");

        let file_extension = abs_path
            .extension()
            .map(|a| a.to_string_lossy().to_string());
        self.report_editor_event("save", file_extension, cx);

        project.update(cx, |project, cx| {
            project.save_buffer_as(buffer, abs_path, cx)
        })
    }

    fn reload(
        &mut self,
        project: ModelHandle<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer().clone();
        let buffers = self.buffer.read(cx).all_buffers();
        let reload_buffers =
            project.update(cx, |project, cx| project.reload_buffers(buffers, true, cx));
        cx.spawn(|this, mut cx| async move {
            let transaction = reload_buffers.log_err().await;
            this.update(&mut cx, |editor, cx| {
                editor.request_autoscroll(Autoscroll::fit(), cx)
            })?;
            buffer.update(&mut cx, |buffer, cx| {
                if let Some(transaction) = transaction {
                    if !buffer.is_singleton() {
                        buffer.push_transaction(&transaction.0, cx);
                    }
                }
            });
            Ok(())
        })
    }

    fn to_item_events(event: &Self::Event) -> SmallVec<[ItemEvent; 2]> {
        let mut result = SmallVec::new();
        match event {
            Event::Closed => result.push(ItemEvent::CloseItem),
            Event::Saved | Event::TitleChanged => {
                result.push(ItemEvent::UpdateTab);
                result.push(ItemEvent::UpdateBreadcrumbs);
            }
            Event::Reparsed => {
                result.push(ItemEvent::UpdateBreadcrumbs);
            }
            Event::SelectionsChanged { local } if *local => {
                result.push(ItemEvent::UpdateBreadcrumbs);
            }
            Event::DirtyChanged => {
                result.push(ItemEvent::UpdateTab);
            }
            Event::BufferEdited => {
                result.push(ItemEvent::Edit);
                result.push(ItemEvent::UpdateBreadcrumbs);
            }
            _ => {}
        }
        result
    }

    fn as_searchable(&self, handle: &ViewHandle<Self>) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn pixel_position_of_cursor(&self, _: &AppContext) -> Option<Vector2F> {
        self.pixel_position_of_newest_cursor
    }

    fn breadcrumb_location(&self) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft { flex: None }
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &AppContext) -> Option<Vec<BreadcrumbText>> {
        let cursor = self.selections.newest_anchor().head();
        let multibuffer = &self.buffer().read(cx);
        let (buffer_id, symbols) =
            multibuffer.symbols_containing(cursor, Some(&theme.editor.syntax), cx)?;
        let buffer = multibuffer.buffer(buffer_id)?;

        let buffer = buffer.read(cx);
        let filename = buffer
            .snapshot()
            .resolve_file_path(
                cx,
                self.project
                    .as_ref()
                    .map(|project| project.visible_worktrees_count(cx) > 1)
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
        let item_id = cx.view_id();
        self.workspace = Some((Arc::new(workspace.weak_handle()), workspace.database_id()));

        fn serialize(
            buffer: ModelHandle<Buffer>,
            workspace_id: WorkspaceId,
            item_id: ItemId,
            cx: &mut AppContext,
        ) {
            if let Some(file) = buffer.read(cx).file().and_then(|file| file.as_local()) {
                let path = file.abs_path(cx);

                cx.background()
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
                        serialize(buffer, *workspace_id, cx.view_id(), cx);
                    }
                }
            })
            .detach();
        }
    }

    fn serialized_item_kind() -> Option<&'static str> {
        Some("Editor")
    }

    fn deserialize(
        project: ModelHandle<Project>,
        _workspace: WeakViewHandle<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: ItemId,
        cx: &mut ViewContext<Pane>,
    ) -> Task<Result<ViewHandle<Self>>> {
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
                        .context("Project item at stored path was not a buffer")?;
                    Ok(pane.update(&mut cx, |_, cx| {
                        cx.add_view(|cx| {
                            let mut editor =
                                Editor::for_buffer(buffer, Some(Arc::new(project)), cx);
                            editor.read_scroll_position_from_db(DB, item_id, workspace_id, cx);
                            editor
                        })
                    })?)
                })
            })
            .unwrap_or_else(|error| Task::ready(Err(error)))
    }
}

impl ProjectItem for Editor {
    type Item = Buffer;

    fn for_project_item(
        project: ModelHandle<Project>,
        buffer: ModelHandle<Buffer>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        Self::for_buffer(buffer, Some(Arc::new(project)), cx)
    }
}

impl SearchableItem for Editor {
    type Match = Range<Anchor>;

    fn to_search_event(
        &mut self,
        event: &Self::Event,
        _: &mut ViewContext<Self>,
    ) -> Option<SearchEvent> {
        match event {
            Event::BufferEdited => Some(SearchEvent::MatchesInvalidated),
            Event::SelectionsChanged { .. } => {
                if self.selections.disjoint_anchors().len() == 1 {
                    Some(SearchEvent::ActiveMatchChanged)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn clear_matches(&mut self, cx: &mut ViewContext<Self>) {
        self.clear_background_highlights::<BufferSearchHighlights>(cx);
    }

    fn update_matches(&mut self, matches: Vec<Range<Anchor>>, cx: &mut ViewContext<Self>) {
        self.highlight_background::<BufferSearchHighlights>(
            matches,
            |theme| theme.search.match_background,
            cx,
        );
    }

    fn query_suggestion(&mut self, cx: &mut ViewContext<Self>) -> String {
        let display_map = self.snapshot(cx).display_snapshot;
        let selection = self.selections.newest::<usize>(cx);
        if selection.start == selection.end {
            let point = selection.start.to_display_point(&display_map);
            let range = surrounding_word(&display_map, point);
            let range = range.start.to_offset(&display_map, Bias::Left)
                ..range.end.to_offset(&display_map, Bias::Right);
            let text: String = display_map.buffer_snapshot.text_for_range(range).collect();
            if text.trim().is_empty() {
                String::new()
            } else {
                text
            }
        } else {
            display_map
                .buffer_snapshot
                .text_for_range(selection.start..selection.end)
                .collect()
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
        cx.background().spawn(async move {
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
                                buffer.anchor_in_excerpt(excerpt.id.clone(), start)
                                    ..buffer.anchor_in_excerpt(excerpt.id.clone(), end)
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
            if probe.end.cmp(cursor, &*buffer).is_lt() {
                Ordering::Less
            } else if probe.start.cmp(cursor, &*buffer).is_gt() {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }) {
            Ok(i) | Err(i) => Some(cmp::min(i, ranges.len() - 1)),
        }
    }
}

pub struct CursorPosition {
    position: Option<Point>,
    selected_count: usize,
    _observe_active_editor: Option<Subscription>,
}

impl Default for CursorPosition {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorPosition {
    pub fn new() -> Self {
        Self {
            position: None,
            selected_count: 0,
            _observe_active_editor: None,
        }
    }

    fn update_position(&mut self, editor: ViewHandle<Editor>, cx: &mut ViewContext<Self>) {
        let editor = editor.read(cx);
        let buffer = editor.buffer().read(cx).snapshot(cx);

        self.selected_count = 0;
        let mut last_selection: Option<Selection<usize>> = None;
        for selection in editor.selections.all::<usize>(cx) {
            self.selected_count += selection.end - selection.start;
            if last_selection
                .as_ref()
                .map_or(true, |last_selection| selection.id > last_selection.id)
            {
                last_selection = Some(selection);
            }
        }
        self.position = last_selection.map(|s| s.head().to_point(&buffer));

        cx.notify();
    }
}

impl Entity for CursorPosition {
    type Event = ();
}

impl View for CursorPosition {
    fn ui_name() -> &'static str {
        "CursorPosition"
    }

    fn render(&mut self, cx: &mut ViewContext<Self>) -> AnyElement<Self> {
        if let Some(position) = self.position {
            let theme = &theme::current(cx).workspace.status_bar;
            let mut text = format!(
                "{}{FILE_ROW_COLUMN_DELIMITER}{}",
                position.row + 1,
                position.column + 1
            );
            if self.selected_count > 0 {
                write!(text, " ({} selected)", self.selected_count).unwrap();
            }
            Label::new(text, theme.cursor_position.clone()).into_any()
        } else {
            Empty::new().into_any()
        }
    }
}

impl StatusItemView for CursorPosition {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut ViewContext<Self>,
    ) {
        if let Some(editor) = active_pane_item.and_then(|item| item.act_as::<Editor>(cx)) {
            self._observe_active_editor = Some(cx.observe(&editor, Self::update_position));
            self.update_position(editor, cx);
        } else {
            self.position = None;
            self._observe_active_editor = None;
        }

        cx.notify();
    }
}

fn path_for_buffer<'a>(
    buffer: &ModelHandle<MultiBuffer>,
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
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::SystemTime,
    };

    #[gpui::test]
    fn test_path_for_file(cx: &mut AppContext) {
        let file = TestFile {
            path: Path::new("").into(),
            full_path: PathBuf::from(""),
        };
        assert_eq!(path_for_file(&file, 0, false, cx), None);
    }

    struct TestFile {
        path: Arc<Path>,
        full_path: PathBuf,
    }

    impl language::File for TestFile {
        fn path(&self) -> &Arc<Path> {
            &self.path
        }

        fn full_path(&self, _: &gpui::AppContext) -> PathBuf {
            self.full_path.clone()
        }

        fn as_local(&self) -> Option<&dyn language::LocalFile> {
            unimplemented!()
        }

        fn mtime(&self) -> SystemTime {
            unimplemented!()
        }

        fn file_name<'a>(&'a self, _: &'a gpui::AppContext) -> &'a std::ffi::OsStr {
            unimplemented!()
        }

        fn worktree_id(&self) -> usize {
            0
        }

        fn is_deleted(&self) -> bool {
            unimplemented!()
        }

        fn as_any(&self) -> &dyn std::any::Any {
            unimplemented!()
        }

        fn to_proto(&self) -> rpc::proto::File {
            unimplemented!()
        }
    }
}