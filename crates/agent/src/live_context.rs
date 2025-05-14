use anyhow::Result;
use editor::{Editor, EditorEvent, ExcerptRange, GotoDefinitionKind, MultiBuffer};
use futures::future;
use gpui::{
    AnyView, App, AppContext, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Task, WeakEntity, Window, prelude::*,
};
use itertools::Itertools;
use language::Capability;
use project::{Project, ProjectPath};
use std::any::{Any, TypeId};
use ui::prelude::*;
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{BreadcrumbText, ItemEvent},
    searchable::SearchableItemHandle,
};

pub struct LiveContextPane {
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    active_editor_subscription: Option<Subscription>,
    refresh_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl LiveContextPane {
    pub fn deploy(workspace: Entity<Workspace>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        workspace.update(cx, {
            let workspace = workspace.clone();
            |workspace_ref, cx| {
                let existing_pane = workspace_ref.items_of_type::<Self>(cx).next();
                if let Some(existing_pane) = existing_pane {
                    workspace_ref.activate_item(&existing_pane, true, true, window, cx);
                    existing_pane
                } else {
                    let pane = cx.new(|cx| Self::new(workspace, workspace_ref, window, cx));
                    // todo! How to deploy to the side
                    workspace_ref.add_item_to_center(Box::new(pane.clone()), window, cx);
                    pane
                }
            }
        })
    }

    pub fn new(
        workspace: Entity<Workspace>,
        workspace_ref: &Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));

        let project = workspace_ref.project().clone();
        let editor = cx.new(|cx| {
            let editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);
            // todo! desired?
            // editor.disable_inline_diagnostics();
            // editor.register_addon(AgentDiffAddon);
            editor
        });

        let this = Self {
            _subscriptions: vec![cx.subscribe_in(
                &workspace,
                window,
                |this, _workspace, event, window, cx| match event {
                    workspace::Event::ActiveItemChanged => {
                        this.handle_active_item_changed(window, cx)
                    }
                    _ => {}
                },
            )],
            active_editor_subscription: None,
            multibuffer,
            editor,
            focus_handle,
            refresh_task: None,
            workspace: workspace.downgrade(),
        };
        cx.defer_in(window, |this, window, cx| {
            this.handle_active_item_changed(window, cx)
        });
        this
    }

    fn handle_active_item_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_editor = self
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item(cx).and_then(|active_item| {
                    if active_item.downcast::<LiveContextPane>().is_some() {
                        None
                    } else {
                        active_item.act_as::<Editor>(cx)
                    }
                })
            })
            .ok()
            .flatten();

        if let Some(active_editor) = active_editor {
            self.active_editor_subscription = Some(cx.subscribe_in(
                &active_editor,
                window,
                |this, active_editor, event, window, cx| match event {
                    EditorEvent::SelectionsChanged { local: _ } => {
                        this.update_excerpts(active_editor, window, cx);
                    }
                    _ => {}
                },
            ));
            self.update_excerpts(&active_editor, window, cx);
        } else {
            self.active_editor_subscription = None;
        }
    }

    fn update_excerpts(
        &mut self,
        active_editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_editor = active_editor.read(cx);
        let multibuffer = active_editor.buffer().read(cx);
        let Some(semantics_provider) = active_editor.semantics_provider() else {
            dbg!("!!!");
            return;
        };
        let cursor = active_editor.selections.newest_anchor().head();
        let Some((buffer_id, symbols)) = multibuffer.symbols_containing(cursor, None, cx) else {
            dbg!("!!!");
            return;
        };
        let Some(symbol) = symbols.last() else {
            dbg!("!!!");
            return;
        };
        /* todo! use?
        let Some(body_range) = dbg!(symbol).body_range.as_ref() else {
            return;
        };
        */
        let Some(buffer) = multibuffer.buffer(buffer_id) else {
            dbg!("!!!");
            return;
        };
        let buffer_snapshot = buffer.read(cx).snapshot();
        let start_offset = buffer_snapshot.offset_for_anchor(&symbol.range.start.text_anchor);
        // todo! handle more layers.
        let Some(layer) = buffer_snapshot.syntax_layer_at(start_offset) else {
            dbg!("!!!");
            return;
        };
        dbg!("!!!");
        let mut definition_tasks = Vec::new();
        let mut cursor = layer.node().walk();
        let mut descendant_index = cursor.descendant_index();
        while cursor.goto_next_sibling() {
            let byte_range = cursor.node().byte_range();
            let start_anchor = buffer_snapshot.anchor_after(byte_range.start);
            dbg!(&byte_range);
            // todo! Other types of definitions?
            definition_tasks.extend(semantics_provider.definitions(
                &buffer,
                start_anchor,
                GotoDefinitionKind::Symbol,
                cx,
            ));
        }
        dbg!(definition_tasks.len());
        let task = future::join_all(definition_tasks);
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let definitions = task.await;
            // todo! How to handle errors?
            //
            // todo! How to handle overlap?
            let excerpts_by_buffer = definitions
                .into_iter()
                .flat_map(|result| result.ok().into_iter().flatten())
                .map(|link| (link.target.buffer, ExcerptRange::new(link.target.range)))
                .into_grouping_map()
                .collect::<Vec<ExcerptRange<text::Anchor>>>();
            dbg!(excerpts_by_buffer.len());
            this.update(cx, |this, cx| {
                this.multibuffer.update(cx, |multibuffer, cx| {
                    multibuffer.clear(cx);
                    for (buffer, ranges) in excerpts_by_buffer {
                        dbg!(ranges.len());
                        multibuffer.push_excerpts(buffer, ranges, cx);
                    }
                })
            })
            .ok();
        }));
    }
}

impl EventEmitter<EditorEvent> for LiveContextPane {}

impl Focusable for LiveContextPane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Item for LiveContextPane {
    type Event = EditorEvent;

    // todo!
    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ZedAssistant).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: impl FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Box<dyn Any>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Live Context".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Live Context Opened")
    }

    fn as_searchable(&self, _: &Entity<Self>) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn is_singleton(&self, _: &App) -> bool {
        false
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Self>>
    where
        Self: Sized,
    {
        self.workspace.upgrade().map(|workspace| {
            // todo! cleanup
            let workspace_clone = workspace.clone();
            workspace.update(cx, |workspace_ref, cx| {
                cx.new(|cx| Self::new(workspace_clone, workspace_ref, window, cx))
            })
        })
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        format: bool,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(format, project, window, cx)
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.reload(project, window, cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyView> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.to_any())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.to_any())
        } else {
            None
        }
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &App) -> Option<Vec<BreadcrumbText>> {
        self.editor.breadcrumbs(theme, cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for LiveContextPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.multibuffer.read(cx).is_empty();
        let focus_handle = &self.focus_handle;

        div()
            .track_focus(focus_handle)
            // todo! Is a new key context actually desired?
            .key_context(if is_empty { "EmptyPane" } else { "LiveContext" })
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            // todo! Handle empty case better?
            .child(self.editor.clone())
    }
}
