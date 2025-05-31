use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    AnyElement, BackgroundExecutor, Empty, Entity, Focusable, FontWeight, ListSizingBehavior,
    ScrollStrategy, SharedString, Size, StrikethroughStyle, StyledText, Task,
    UniformListScrollHandle, div, px, uniform_list,
};
use itertools::Itertools;
use language::language_settings::WordsCompletionMode;
use language::{
    Buffer, CodeLabel, LanguageName, LanguageRegistry, language_settings::language_settings,
};
use language::{BufferSnapshot, CharKind, WordsQuery};
use lsp::{CompletionContext, CompletionTriggerKind, InsertTextMode};
use markdown::{Markdown, MarkdownElement};
use multi_buffer::{Anchor, ExcerptId};
use ordered_float::OrderedFloat;
use project::{
    CodeAction, Completion, CompletionSource, TaskSourceKind, lsp_store::CompletionDocumentation,
};
use settings::Settings as _;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::{
    cell::RefCell,
    cmp::{Reverse, min},
    iter,
    ops::Range,
    rc::Rc,
};
use task::DebugScenario;
use task::ResolvedTask;
use task::TaskContext;
use text::{Bias, Point, ToOffset as _};
use ui::{Color, IntoElement, ListItem, Pixels, Popover, Styled, prelude::*};
use util::ResultExt;

use crate::editor_settings::SnippetSortOrder;
use crate::hover_popover::{hover_markdown_style, open_markdown_url};
use crate::{
    CodeActionProvider, CompletionItemKind, CompletionProvider, DisplayRow, Editor, EditorStyle,
    ResolvedTasks,
    actions::{ConfirmCodeAction, ConfirmCompletion},
    split_words, styled_runs_for_code_label,
};
use crate::{CodeActionSource, EditorSettings};

pub const MENU_GAP: Pixels = px(4.);
pub const MENU_ASIDE_X_PADDING: Pixels = px(16.);
pub const MENU_ASIDE_MIN_WIDTH: Pixels = px(260.);
pub const MENU_ASIDE_MAX_WIDTH: Pixels = px(500.);

// Constants for the markdown cache. The purpose of this cache is to reduce flickering due to
// documentation not yet being parsed.
//
// The size of the cache is set to the number of items fetched around the current selection plus one
// for the current selection and another to avoid cases where and adjacent selection exits the
// cache. The only current benefit of a larger cache would be doing less markdown parsing when the
// selection revisits items.
//
// One future benefit of a larger cache would be reducing flicker on backspace. This would require
// not recreating the menu on every change, by not re-querying the language server when
// `is_incomplete = false`.
const MARKDOWN_CACHE_MAX_SIZE: usize = MARKDOWN_CACHE_BEFORE_ITEMS + MARKDOWN_CACHE_AFTER_ITEMS + 2;
const MARKDOWN_CACHE_BEFORE_ITEMS: usize = 2;
const MARKDOWN_CACHE_AFTER_ITEMS: usize = 2;

// Number of items beyond the visible items to resolve documentation.
const RESOLVE_BEFORE_ITEMS: usize = 4;
const RESOLVE_AFTER_ITEMS: usize = 4;

pub enum CodeContextMenu {
    Completions(CompletionMenu),
    CodeActions(CodeActionsMenu),
}

pub enum MenuSelectionChange {
    First,
    Last,
    Prev,
    Next,
}

impl MenuSelectionChange {
    fn flip(self) -> MenuSelectionChange {
        match self {
            Self::First => Self::Last,
            Self::Last => Self::First,
            Self::Prev => Self::Next,
            Self::Next => Self::Prev,
        }
    }

    fn new_selection(self, index: usize, length: usize) -> usize {
        match self {
            Self::First => 0,
            Self::Last => length.saturating_sub(1),
            Self::Prev if index == 0 => length.saturating_sub(1),
            Self::Next if index + 1 >= length => 0,
            Self::Prev => index - 1,
            Self::Next => index + 1,
        }
    }
}

impl CodeContextMenu {
    pub fn change_selection(
        &mut self,
        change: MenuSelectionChange,
        provider: Option<&dyn CompletionProvider>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> bool {
        if self.visible() {
            match self {
                CodeContextMenu::Completions(menu) => {
                    menu.change_selection(change, provider, window, cx)
                }
                CodeContextMenu::CodeActions(menu) => menu.change_selection(change, cx),
            }
            // todo! return bool from impls?
            true
        } else {
            false
        }
    }

    pub fn visible(&self) -> bool {
        match self {
            CodeContextMenu::Completions(menu) => menu.visible(),
            CodeContextMenu::CodeActions(menu) => menu.visible(),
        }
    }

    pub fn origin(&self) -> ContextMenuOrigin {
        match self {
            CodeContextMenu::Completions(menu) => menu.origin(),
            CodeContextMenu::CodeActions(menu) => menu.origin(),
        }
    }

    pub fn render(
        &self,
        style: &EditorStyle,
        max_height_in_lines: u32,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> AnyElement {
        match self {
            CodeContextMenu::Completions(menu) => {
                menu.render(style, max_height_in_lines, window, cx)
            }
            CodeContextMenu::CodeActions(menu) => {
                menu.render(style, max_height_in_lines, window, cx)
            }
        }
    }

    pub fn render_aside(
        &mut self,
        max_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Option<AnyElement> {
        match self {
            CodeContextMenu::Completions(menu) => menu.render_aside(max_size, window, cx),
            CodeContextMenu::CodeActions(_) => None,
        }
    }

    pub fn focused(&self, window: &mut Window, cx: &mut Context<Editor>) -> bool {
        // todo!
        false
        /*
        match self {
            CodeContextMenu::Completions(completions_menu) => completions_menu
                .get_or_create_entry_markdown(completions_menu.selected_item, cx)
                .as_ref()
                .is_some_and(|markdown| markdown.focus_handle(cx).contains_focused(window, cx)),
            CodeContextMenu::CodeActions(_) => false,
        }
        */
    }
}

pub enum ContextMenuOrigin {
    Cursor,
    GutterIndicator(DisplayRow),
    QuickActionBar,
}

pub struct CompletionMenu {
    position: Anchor,
    buffer: Entity<Buffer>,
    // todo! does it need to be Rc RefCell?
    contents: Option<Rc<RefCell<CompletionMenuContents>>>,
    tasks: Vec<CompletionTask>,
}

pub struct CompletionTask {
    query: Option<String>,
    task: Task<()>,
}

// todo! rename
pub struct QueriedCompletions {
    /// Fetched completions. This will stay the same length, but uses `RefCell` when resolving more
    /// information (typically documentation) from the provider.
    completions: Rc<RefCell<Box<[Completion]>>>,
    /// Query that was used when populating.
    query: Option<String>,
    /// Whether `completions` is incomplete and so should be refetched instead of filtering.
    is_incomplete: bool,
}

pub struct CompletionMenuConfiguration {
    pub sort_completions: bool,
    pub resolve_completions: bool,
    pub show_completion_documentation: bool,
    pub ignore_completion_provider: bool,
    pub snippet_sort_order: SnippetSortOrder,
}

pub struct CompletionMenuContents {
    queried_completions: QueriedCompletions,
    /// todo! doc
    language: Option<LanguageName>,
    /// todo! doc
    language_registry: Option<Arc<LanguageRegistry>>,
    /// Match candidates for entry filtering. Immutable and uses the same indices as `completions`.
    match_candidates: Rc<[StringMatchCandidate]>,
    /// Completion entries filtered / sorted for display. `StringMatch::candidate_id` is an index
    /// into `completions` / `match_candidates`.
    ///
    /// todo! does it still need to be rc refcell?
    entries: Rc<RefCell<Vec<StringMatch>>>,
    /// Index into `entries` for the item currently selected by the user.
    selected_item: usize,
    /// Index range in `entries` that was last rendered. Used for resolving visible completions in
    /// case this affects the display of inline docs.
    last_rendered_range: Rc<RefCell<Option<Range<usize>>>>,
    /// todo! document. Move?
    scroll_handle: UniformListScrollHandle,
    /// Cache of parsed documentation markdown. The `usize` is an index into `completions`.
    markdown_cache: Rc<RefCell<VecDeque<(usize, Entity<Markdown>)>>>,
}

impl CompletionMenuContents {
    pub fn new_completions(
        queried_completions: QueriedCompletions,
        language: Option<LanguageName>,
        language_registry: Option<Arc<LanguageRegistry>>,
    ) -> Self {
        let match_candidates = queried_completions
            .completions
            .borrow()
            .iter()
            .enumerate()
            .map(|(id, completion)| StringMatchCandidate::new(id, &completion.label.filter_text()))
            .collect();

        let this = CompletionMenuContents {
            queried_completions,
            language,
            language_registry,
            match_candidates,
            entries: RefCell::new(Vec::new()).into(),
            selected_item: 0,
            last_rendered_range: RefCell::new(None).into(),
            scroll_handle: UniformListScrollHandle::new(),
            markdown_cache: RefCell::new(VecDeque::with_capacity(MARKDOWN_CACHE_MAX_SIZE)).into(),
        };

        // todo!
        // this.start_markdown_parse_for_nearby_entries(cx);

        this
    }

    pub fn new_snippets(choices: &Vec<String>, selection: Range<Anchor>) -> Self {
        let completions = choices
            .iter()
            .map(|choice| Completion {
                replace_range: selection.start.text_anchor..selection.end.text_anchor,
                new_text: choice.to_string(),
                label: CodeLabel {
                    text: choice.to_string(),
                    runs: Default::default(),
                    filter_range: Default::default(),
                },
                icon_path: None,
                documentation: None,
                confirm: None,
                insert_text_mode: None,
                source: CompletionSource::Custom,
            })
            .collect::<Box<[_]>>();
        let match_candidates = choices
            .iter()
            .enumerate()
            .map(|(id, completion)| StringMatchCandidate::new(id, &completion))
            .collect();
        let entries = choices
            .iter()
            .enumerate()
            .map(|(id, completion)| StringMatch {
                candidate_id: id,
                score: 1.,
                positions: vec![],
                string: completion.clone(),
            })
            .collect::<Vec<_>>();
        Self {
            queried_completions: QueriedCompletions {
                completions: Rc::new(RefCell::new(completions)),
                query: None,
                is_incomplete: false,
            },
            language: None,
            language_registry: None,
            match_candidates,
            entries: RefCell::new(entries).into(),
            selected_item: 0,
            last_rendered_range: RefCell::new(None).into(),
            scroll_handle: UniformListScrollHandle::new(),
            markdown_cache: RefCell::new(VecDeque::new()).into(),
        }
    }

    fn change_selection(
        &mut self,
        mut change: MenuSelectionChange,
        provider: Option<&dyn CompletionProvider>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        if self.scroll_handle.y_flipped() {
            change = change.flip();
        }
        let new_selection = change.new_selection(self.selected_item, self.entries.borrow().len());
        if new_selection != self.selected_item {
            self.selected_item = new_selection;
            self.scroll_handle
                .scroll_to_item(self.selected_item, ScrollStrategy::Top);
            /* todo!
            self.resolve_visible_completions(provider, cx);
            self.start_markdown_parse_for_nearby_entries(cx);
            if let Some(provider) = provider {
                self.handle_selection_changed(provider, window, cx);
            }
            */
            cx.notify();
        }
    }

    fn render(
        &self,
        style: &EditorStyle,
        max_height_in_lines: u32,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> AnyElement {
        let show_completion_documentation = self.show_completion_documentation;
        let selected_item = self.selected_item;
        let completions = self.completions.clone();
        let entries = self.entries.clone();
        let last_rendered_range = self.last_rendered_range.clone();
        let style = style.clone();
        let list = uniform_list(
            cx.entity().clone(),
            "completions",
            self.entries.borrow().len(),
            move |_editor, range, _window, cx| {
                last_rendered_range.borrow_mut().replace(range.clone());
                let start_ix = range.start;
                let completions_guard = completions.borrow_mut();

                entries.borrow()[range]
                    .iter()
                    .enumerate()
                    .map(|(ix, mat)| {
                        let item_ix = start_ix + ix;
                        let completion = &completions_guard[mat.candidate_id];
                        let documentation = if show_completion_documentation {
                            &completion.documentation
                        } else {
                            &None
                        };

                        let filter_start = completion.label.filter_range.start;
                        let highlights = gpui::combine_highlights(
                            mat.ranges().map(|range| {
                                (
                                    filter_start + range.start..filter_start + range.end,
                                    FontWeight::BOLD.into(),
                                )
                            }),
                            styled_runs_for_code_label(&completion.label, &style.syntax).map(
                                |(range, mut highlight)| {
                                    // Ignore font weight for syntax highlighting, as we'll use it
                                    // for fuzzy matches.
                                    highlight.font_weight = None;
                                    if completion
                                        .source
                                        .lsp_completion(false)
                                        .and_then(|lsp_completion| lsp_completion.deprecated)
                                        .unwrap_or(false)
                                    {
                                        highlight.strikethrough = Some(StrikethroughStyle {
                                            thickness: 1.0.into(),
                                            ..Default::default()
                                        });
                                        highlight.color = Some(cx.theme().colors().text_muted);
                                    }

                                    (range, highlight)
                                },
                            ),
                        );

                        let completion_label = StyledText::new(completion.label.text.clone())
                            .with_default_highlights(&style.text, highlights);

                        let documentation_label = match documentation {
                            Some(CompletionDocumentation::SingleLine(text))
                            | Some(CompletionDocumentation::SingleLineAndMultiLinePlainText {
                                single_line: text,
                                ..
                            }) => {
                                if text.trim().is_empty() {
                                    None
                                } else {
                                    Some(
                                        Label::new(text.clone())
                                            .ml_4()
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                }
                            }
                            _ => None,
                        };

                        let start_slot = completion
                            .color()
                            .map(|color| {
                                div()
                                    .flex_shrink_0()
                                    .size_3p5()
                                    .rounded_xs()
                                    .bg(color)
                                    .into_any_element()
                            })
                            .or_else(|| {
                                completion.icon_path.as_ref().map(|path| {
                                    Icon::from_path(path)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted)
                                        .into_any_element()
                                })
                            });

                        div().min_w(px(280.)).max_w(px(540.)).child(
                            ListItem::new(mat.candidate_id)
                                .inset(true)
                                .toggle_state(item_ix == selected_item)
                                .on_click(cx.listener(move |editor, _event, window, cx| {
                                    cx.stop_propagation();
                                    if let Some(task) = editor.confirm_completion(
                                        &ConfirmCompletion {
                                            item_ix: Some(item_ix),
                                        },
                                        window,
                                        cx,
                                    ) {
                                        task.detach_and_log_err(cx)
                                    }
                                }))
                                .start_slot::<AnyElement>(start_slot)
                                .child(h_flex().overflow_hidden().child(completion_label))
                                .end_slot::<Label>(documentation_label),
                        )
                    })
                    .collect()
            },
        )
        .occlude()
        .max_h(max_height_in_lines as f32 * window.line_height())
        .track_scroll(self.scroll_handle.clone())
        .with_sizing_behavior(ListSizingBehavior::Infer)
        .w(rems(34.));

        Popover::new().child(list).into_any_element()
    }

    fn render_aside(
        &mut self,
        max_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Option<AnyElement> {
        if !self.show_completion_documentation {
            return None;
        }

        let mat = &self.entries.borrow()[self.selected_item];
        let multiline_docs = match self.completions.borrow_mut()[mat.candidate_id]
            .documentation
            .as_ref()?
        {
            CompletionDocumentation::MultiLinePlainText(text) => div().child(text.clone()),
            CompletionDocumentation::SingleLineAndMultiLinePlainText {
                plain_text: Some(text),
                ..
            } => div().child(text.clone()),
            CompletionDocumentation::MultiLineMarkdown(source) if !source.is_empty() => {
                let (is_parsing, markdown) =
                    self.get_or_create_markdown(mat.candidate_id, source.clone(), true, cx);
                if is_parsing {
                    return None;
                }
                div().child(
                    MarkdownElement::new(markdown, hover_markdown_style(window, cx))
                        .code_block_renderer(markdown::CodeBlockRenderer::Default {
                            copy_button: false,
                            copy_button_on_hover: false,
                            border: false,
                        })
                        .on_url_click(open_markdown_url),
                )
            }
            CompletionDocumentation::MultiLineMarkdown(_) => return None,
            CompletionDocumentation::SingleLine(_) => return None,
            CompletionDocumentation::Undocumented => return None,
            CompletionDocumentation::SingleLineAndMultiLinePlainText {
                plain_text: None, ..
            } => {
                return None;
            }
        };

        Some(
            Popover::new()
                .child(
                    multiline_docs
                        .id("multiline_docs")
                        .px(MENU_ASIDE_X_PADDING / 2.)
                        .max_w(max_size.width)
                        .max_h(max_size.height)
                        .overflow_y_scroll()
                        .occlude(),
                )
                .into_any_element(),
        )
    }
}

impl CompletionMenu {
    pub fn new(position: Anchor, buffer: Entity<Buffer>) -> Self {
        Self {
            position,
            buffer,
            contents: None,
            tasks: Vec::new(),
        }
    }

    fn change_selection(
        &mut self,
        change: MenuSelectionChange,
        provider: Option<&dyn CompletionProvider>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        if let Some(contents) = &self.contents {
            contents
                .borrow_mut()
                .change_selection(change, provider, window, cx);
        }
    }

    pub fn visible(&self) -> bool {
        if let Some(contents) = &self.contents {
            !contents.borrow().entries.borrow().is_empty()
        } else {
            false
        }
    }

    fn origin(&self) -> ContextMenuOrigin {
        ContextMenuOrigin::Cursor
    }

    fn render(
        &self,
        style: &EditorStyle,
        max_height_in_lines: u32,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> AnyElement {
        if let Some(contents) = &self.contents {
            contents
                .borrow()
                .render(style, max_height_in_lines, window, cx)
        } else {
            Empty.into_any_element()
        }
    }

    fn render_aside(
        &mut self,
        max_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Option<AnyElement> {
        if let Some(contents) = &self.contents {
            contents.borrow_mut().render_aside(max_size, window, cx)
        } else {
            None
        }
    }

    pub fn query_completions(
        &mut self,
        editor: &Editor,
        cursor_position: Anchor,
        trigger: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> bool {
        if cursor_position.buffer_id != self.position.buffer_id {
            return false;
        }
        let buffer = self.buffer.read(cx);
        let buffer_snapshot = buffer.snapshot();
        let cursor_offset = cursor_position.text_anchor.to_offset(&buffer_snapshot);
        if cursor_position != self.position {
            if cursor_offset != self.position.text_anchor.to_offset(&buffer_snapshot) {
                return false;
            }
        }
        let excerpt_id = cursor_position.excerpt_id;
        let cursor_position = cursor_position.text_anchor;

        let (word_range, kind) = buffer_snapshot.surrounding_word(cursor_offset, true);
        let query = if cursor_offset > word_range.start && kind == Some(CharKind::Word) {
            Some(
                buffer_snapshot
                    .text_for_range(word_range.start..cursor_offset)
                    .collect::<String>(),
            )
        } else {
            None
        };

        /* todo!
        let provider = if only_word_completions {
            None
        } else {
            self.completion_provider.clone()
        };
        */

        let provider = editor.completion_provider();

        let sort_completions = provider
            .as_ref()
            .map_or(false, |provider| provider.sort_completions());

        let filter_completions = provider
            .as_ref()
            .map_or(true, |provider| provider.filter_completions());

        if let Some(contents) = self.contents {
            let contents = contents.borrow_mut();
            let can_filter = if !contents.queried_completions.is_incomplete && filter_completions {
                // If the new query is a suffix of the old query (typing more characters) and
                // the previous result was complete, the existing completions can be filtered.
                match (&contents.queried_completions.query, &query) {
                    (Some(initial_query), Some(query)) => query.starts_with(initial_query),
                    // Also valid to re-use the "no query" case.
                    //
                    // todo! actually true?
                    (None, _) => true,
                    _ => false,
                }
            } else {
                false
            };
            if can_filter {
                // todo!
                return true;
            }
        }

        let trigger_kind = match trigger {
            Some(trigger) if buffer.completion_triggers().contains(trigger) => {
                CompletionTriggerKind::TRIGGER_CHARACTER
            }
            _ => CompletionTriggerKind::INVOKED,
        };
        let completion_context = CompletionContext {
            trigger_character: trigger.and_then(|trigger| {
                if trigger_kind == CompletionTriggerKind::TRIGGER_CHARACTER {
                    Some(String::from(trigger))
                } else {
                    None
                }
            }),
            trigger_kind,
        };

        // todo! should for_completion actually be true?!?
        let (replace_range, word_kind) = buffer_snapshot.surrounding_word(cursor_offset, false);
        let (replace_range, word_to_exclude) = if word_kind == Some(CharKind::Word) {
            let word_to_exclude = buffer_snapshot
                .text_for_range(replace_range.clone())
                .collect::<String>();
            (
                buffer_snapshot.anchor_before(replace_range.start)
                    ..buffer_snapshot.anchor_after(replace_range.end),
                Some(word_to_exclude),
            )
        } else {
            (cursor_position..cursor_position, None)
        };

        let language_registry = editor
            .workspace
            .as_ref()
            .and_then(|(workspace, _)| workspace.upgrade())
            .map(|workspace| workspace.read(cx).app_state().languages.clone());

        let language = buffer_snapshot
            .language_at(cursor_position)
            .map(|language| language.name());

        let completion_settings =
            language_settings(language.clone(), buffer_snapshot.file(), cx).completions;

        let show_completion_documentation = buffer_snapshot
            .settings_at(cursor_position, cx)
            .show_completion_documentation;

        let snippet_sort_order = EditorSettings::get_global(cx).snippet_sort_order;

        let (provider_task, mut words_task) = match &provider {
            Some(provider) => {
                let provider_task = provider.completions(
                    excerpt_id,
                    &self.buffer,
                    cursor_position,
                    completion_context,
                    window,
                    cx,
                );

                let words_task = match completion_settings.words {
                    WordsCompletionMode::Disabled => Task::ready(BTreeMap::default()),
                    WordsCompletionMode::Enabled | WordsCompletionMode::Fallback => {
                        Self::query_word_completions(
                            query.as_ref(),
                            &cursor_position,
                            buffer_snapshot,
                            cx,
                        )
                    }
                };

                (provider_task, words_task)
            }
            None => {
                // todo! shouldn't this respect WordsCompletionMode::Disabled?
                let provider_task = Task::ready(Ok(Vec::new()));
                let words_task = Self::query_word_completions(
                    query.as_ref(),
                    &cursor_position,
                    buffer_snapshot,
                    cx,
                );
                (provider_task, words_task)
            }
        };

        let task = cx.spawn_in(window, async move |editor, cx| {
            // todo! Ideally would selectively refresh completions instead of treating them all as
            // incomplete if one source is incomplete.
            let mut completions = Vec::new();
            let mut is_incomplete = false;
            if let Some(provider_responses) = provider_task.await.log_err() {
                if !provider_responses.is_empty() {
                    for response in provider_responses {
                        completions.extend(response.completions);
                        is_incomplete = is_incomplete || response.is_incomplete;
                    }
                    if completion_settings.words == WordsCompletionMode::Fallback {
                        words_task = Task::ready(BTreeMap::default());
                    }
                }
            }

            let mut words = words_task.await;
            if let Some(word_to_exclude) = &word_to_exclude {
                words.remove(word_to_exclude);
            }
            for lsp_completion in &completions {
                words.remove(&lsp_completion.new_text);
            }
            completions.extend(words.into_iter().map(|(word, word_range)| Completion {
                replace_range: replace_range.clone(),
                new_text: word.clone(),
                label: CodeLabel::plain(word, None),
                icon_path: None,
                documentation: None,
                source: CompletionSource::BufferWord {
                    word_range,
                    resolved: false,
                },
                insert_text_mode: Some(InsertTextMode::AS_IS),
                confirm: None,
            }));

            /* todo!
            menu.filter(
                if filter_completions {
                    query.as_deref()
                } else {
                    None
                },
                cx.background_executor(),
            )
            .await;
            */

            if completions.is_empty() {
                self.contents = None;
            } else {
                let queried_completions = QueriedCompletions {
                    completions: Rc::new(RefCell::new(completions.into_boxed_slice())),
                    query,
                    is_incomplete,
                };
                let contents = CompletionMenuContents::new_completions(
                    queried_completions,
                    language,
                    language_registry,
                );
                self.contents = Some(Rc::new(RefCell::new(contents)));
            }
            // todo! also call when filtering?
            editor.update_in(cx, |editor, window, cx| {
                editor.handle_completions_menu_updated(window, cx);
            });
        });

        self.tasks.clear();

        /* todo!
        // Keep completion tasks which could provide the completions needed for the current query if
        // `is_incomplete == false`.
        self.tasks.retain(|completion_task| match (&query, &completion_task.query) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(query), Some(task_query)) => query.starts_with(task_query),
        });
        */

        self.tasks.push(CompletionTask { query, task });

        true
    }

    // todo! Do this after every entries change
    // menu.resolve_visible_completions(self.completion_provider.as_deref(), cx);

    fn query_word_completions(
        query: Option<&str>,
        cursor_position: &text::Anchor,
        buffer_snapshot: BufferSnapshot,
        cx: &App,
    ) -> Task<BTreeMap<String, Range<text::Anchor>>> {
        // The document can be large, so stay in reasonable bounds when searching for words,
        // otherwise completion pop-up might be slow to appear.
        const WORD_LOOKUP_ROWS: u32 = 5_000;
        let buffer_row = text::ToPoint::to_point(cursor_position, &buffer_snapshot).row;
        let min_word_search = buffer_snapshot.clip_point(
            Point::new(buffer_row.saturating_sub(WORD_LOOKUP_ROWS), 0),
            Bias::Left,
        );
        let max_word_search = buffer_snapshot.clip_point(
            Point::new(buffer_row + WORD_LOOKUP_ROWS, 0).min(buffer_snapshot.max_point()),
            Bias::Right,
        );
        let word_search_range = buffer_snapshot.point_to_offset(min_word_search)
            ..buffer_snapshot.point_to_offset(max_word_search);

        let skip_digits = query.map_or(true, |query| !query.chars().any(|c| c.is_digit(10)));

        cx.background_spawn(async move {
            buffer_snapshot.words_in_range(WordsQuery {
                fuzzy_contents: None,
                range: word_search_range,
                skip_digits,
            })
        })
    }
}

struct OldImpl;

impl OldImpl {
    /*
    fn handle_selection_changed(
        &self,
        provider: &dyn CompletionProvider,
        window: &mut Window,
        cx: &mut App,
    ) {
        let entries = self.entries.borrow();
        let entry = if self.selected_item < entries.len() {
            Some(&entries[self.selected_item])
        } else {
            None
        };
        provider.selection_changed(entry, window, cx);
    }

    pub fn resolve_visible_completions(
        &mut self,
        provider: Option<&dyn CompletionProvider>,
        cx: &mut Context<Editor>,
    ) {
        if !self.resolve_completions {
            return;
        }
        let Some(provider) = provider else {
            return;
        };

        let entries = self.entries.borrow();
        if entries.is_empty() {
            return;
        }
        if self.selected_item >= entries.len() {
            log::error!(
                "bug: completion selected_item >= entries.len(): {} >= {}",
                self.selected_item,
                entries.len()
            );
            self.selected_item = entries.len() - 1;
        }

        // Attempt to resolve completions for every item that will be displayed. This matters
        // because single line documentation may be displayed inline with the completion.
        //
        // When navigating to the very beginning or end of completions, `last_rendered_range` may
        // have no overlap with the completions that will be displayed, so instead use a range based
        // on the last rendered count.
        const APPROXIMATE_VISIBLE_COUNT: usize = 12;
        let last_rendered_range = self.last_rendered_range.borrow().clone();
        let visible_count = last_rendered_range
            .clone()
            .map_or(APPROXIMATE_VISIBLE_COUNT, |range| range.count());
        let entry_range = if self.selected_item == 0 {
            0..min(visible_count, entries.len())
        } else if self.selected_item == entries.len() - 1 {
            entries.len().saturating_sub(visible_count)..entries.len()
        } else {
            last_rendered_range.map_or(0..0, |range| {
                min(range.start, entries.len())..min(range.end, entries.len())
            })
        };

        // Expand the range to resolve more completions than are predicted to be visible, to reduce
        // jank on navigation.
        let entry_indices = util::expanded_and_wrapped_usize_range(
            entry_range.clone(),
            RESOLVE_BEFORE_ITEMS,
            RESOLVE_AFTER_ITEMS,
            entries.len(),
        );

        // Avoid work by sometimes filtering out completions that already have documentation.
        // This filtering doesn't happen if the completions are currently being updated.
        let completions = self.completions.borrow();
        let candidate_ids = entry_indices
            .map(|i| entries[i].candidate_id)
            .filter(|i| completions[*i].documentation.is_none());

        // Current selection is always resolved even if it already has documentation, to handle
        // out-of-spec language servers that return more results later.
        let selected_candidate_id = entries[self.selected_item].candidate_id;
        let candidate_ids = iter::once(selected_candidate_id)
            .chain(candidate_ids.filter(|id| *id != selected_candidate_id))
            .collect::<Vec<usize>>();
        drop(entries);

        if candidate_ids.is_empty() {
            return;
        }

        let resolve_task = provider.resolve_completions(
            self.buffer.clone(),
            candidate_ids,
            self.completions.clone(),
            cx,
        );

        let completion_id = self.id;
        cx.spawn(async move |editor, cx| {
            if let Some(true) = resolve_task.await.log_err() {
                editor
                    .update(cx, |editor, cx| {
                        // `resolve_completions` modified state affecting display.
                        cx.notify();
                        editor.with_completions_menu_matching_id(completion_id, |menu| {
                            if let Some(menu) = menu {
                                menu.start_markdown_parse_for_nearby_entries(cx)
                            }
                        });
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn start_markdown_parse_for_nearby_entries(&self, cx: &mut Context<Editor>) {
        // Enqueue parse tasks of nearer items first.
        //
        // TODO: This means that the nearer items will actually be further back in the cache, which
        // is not ideal. In practice this is fine because `get_or_create_markdown` moves the current
        // selection to the front (when `is_render = true`).
        let entry_indices = util::wrapped_usize_outward_from(
            self.selected_item,
            MARKDOWN_CACHE_BEFORE_ITEMS,
            MARKDOWN_CACHE_AFTER_ITEMS,
            self.entries.borrow().len(),
        );

        for index in entry_indices {
            self.get_or_create_entry_markdown(index, cx);
        }
    }

    fn get_or_create_entry_markdown(
        &self,
        index: usize,
        cx: &mut Context<Editor>,
    ) -> Option<Entity<Markdown>> {
        let entries = self.entries.borrow();
        if index >= entries.len() {
            return None;
        }
        let candidate_id = entries[index].candidate_id;
        match &self.completions.borrow()[candidate_id].documentation {
            Some(CompletionDocumentation::MultiLineMarkdown(source)) if !source.is_empty() => Some(
                self.get_or_create_markdown(candidate_id, source.clone(), false, cx)
                    .1,
            ),
            Some(_) => None,
            _ => None,
        }
    }

    fn get_or_create_markdown(
        &self,
        candidate_id: usize,
        source: SharedString,
        is_render: bool,
        cx: &mut Context<Editor>,
    ) -> (bool, Entity<Markdown>) {
        let mut markdown_cache = self.markdown_cache.borrow_mut();
        if let Some((cache_index, (_, markdown))) = markdown_cache
            .iter()
            .find_position(|(id, _)| *id == candidate_id)
        {
            let markdown = if is_render && cache_index != 0 {
                // Move the current selection's cache entry to the front.
                markdown_cache.rotate_right(1);
                let cache_len = markdown_cache.len();
                markdown_cache.swap(0, (cache_index + 1) % cache_len);
                &markdown_cache[0].1
            } else {
                markdown
            };

            let is_parsing = markdown.update(cx, |markdown, cx| {
                // `reset` is called as it's possible for documentation to change due to resolve
                // requests. It does nothing if `source` is unchanged.
                markdown.reset(source, cx);
                markdown.is_parsing()
            });
            return (is_parsing, markdown.clone());
        }

        if markdown_cache.len() < MARKDOWN_CACHE_MAX_SIZE {
            let markdown = cx.new(|cx| {
                Markdown::new(
                    source,
                    self.language_registry.clone(),
                    self.language.clone(),
                    cx,
                )
            });
            // Handles redraw when the markdown is done parsing. The current render is for a
            // deferred draw, and so without this did not redraw when `markdown` notified.
            cx.observe(&markdown, |_, _, cx| cx.notify()).detach();
            markdown_cache.push_front((candidate_id, markdown.clone()));
            (true, markdown)
        } else {
            debug_assert_eq!(markdown_cache.capacity(), MARKDOWN_CACHE_MAX_SIZE);
            // Moves the last cache entry to the start. The ring buffer is full, so this does no
            // copying and just shifts indexes.
            markdown_cache.rotate_right(1);
            markdown_cache[0].0 = candidate_id;
            let markdown = &markdown_cache[0].1;
            markdown.update(cx, |markdown, cx| markdown.reset(source, cx));
            (true, markdown.clone())
        }
    }

    pub fn sort_matches(
        matches: &mut Vec<SortableMatch<'_>>,
        query: Option<&str>,
        snippet_sort_order: SnippetSortOrder,
    ) {
        #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
        enum MatchTier<'a> {
            WordStartMatch {
                sort_mixed_case_prefix_length: Reverse<usize>,
                sort_snippet: Reverse<i32>,
                sort_kind: usize,
                sort_fuzzy_bracket: Reverse<usize>,
                sort_text: Option<&'a str>,
                sort_score: Reverse<OrderedFloat<f64>>,
                sort_label: &'a str,
            },
            OtherMatch {
                sort_score: Reverse<OrderedFloat<f64>>,
            },
        }

        // Our goal here is to intelligently sort completion suggestions. We want to
        // balance the raw fuzzy match score with hints from the language server

        // In a fuzzy bracket, matches with a score of 1.0 are prioritized.
        // The remaining matches are partitioned into two groups at 3/5 of the max_score.
        let max_score = matches
            .iter()
            .map(|mat| mat.string_match.score)
            .fold(0.0, f64::max);
        let fuzzy_bracket_threshold = max_score * (3.0 / 5.0);

        let query_start_lower = query
            .and_then(|q| q.chars().next())
            .and_then(|c| c.to_lowercase().next());

        matches.sort_unstable_by_key(|mat| {
            let score = mat.string_match.score;
            let sort_score = Reverse(OrderedFloat(score));

            let query_start_doesnt_match_split_words = query_start_lower
                .map(|query_char| {
                    !split_words(&mat.string_match.string).any(|word| {
                        word.chars()
                            .next()
                            .and_then(|c| c.to_lowercase().next())
                            .map_or(false, |word_char| word_char == query_char)
                    })
                })
                .unwrap_or(false);

            if query_start_doesnt_match_split_words {
                MatchTier::OtherMatch { sort_score }
            } else {
                let sort_fuzzy_bracket = Reverse(if score >= fuzzy_bracket_threshold {
                    1
                } else {
                    0
                });
                let sort_snippet = match snippet_sort_order {
                    SnippetSortOrder::Top => Reverse(if mat.is_snippet { 1 } else { 0 }),
                    SnippetSortOrder::Bottom => Reverse(if mat.is_snippet { 0 } else { 1 }),
                    SnippetSortOrder::Inline => Reverse(0),
                };
                let sort_mixed_case_prefix_length = Reverse(
                    query
                        .map(|q| {
                            q.chars()
                                .zip(mat.string_match.string.chars())
                                .enumerate()
                                .take_while(|(i, (q_char, match_char))| {
                                    if *i == 0 {
                                        // Case-sensitive comparison for first character
                                        q_char == match_char
                                    } else {
                                        // Case-insensitive comparison for other characters
                                        q_char.to_lowercase().eq(match_char.to_lowercase())
                                    }
                                })
                                .count()
                        })
                        .unwrap_or(0),
                );
                MatchTier::WordStartMatch {
                    sort_mixed_case_prefix_length,
                    sort_snippet,
                    sort_kind: mat.sort_kind,
                    sort_fuzzy_bracket,
                    sort_text: mat.sort_text,
                    sort_score,
                    sort_label: mat.sort_label,
                }
            }
        });
    }

    pub async fn filter(&mut self, query: Option<&str>, background_executor: &BackgroundExecutor) {
        let mut matches = if let Some(query) = query {
            fuzzy::match_strings(
                &self.match_candidates,
                query,
                query.chars().any(|c| c.is_uppercase()),
                100,
                &Default::default(),
                background_executor.clone(),
            )
            .await
        } else {
            self.match_candidates
                .iter()
                .enumerate()
                .map(|(candidate_id, candidate)| StringMatch {
                    candidate_id,
                    score: Default::default(),
                    positions: Default::default(),
                    string: candidate.string.clone(),
                })
                .collect()
        };

        if self.sort_completions {
            let completions = self.completions.borrow();

            let mut sortable_items: Vec<SortableMatch<'_>> = matches
                .into_iter()
                .map(|string_match| {
                    let completion = &completions[string_match.candidate_id];

                    let is_snippet = matches!(
                        &completion.source,
                        CompletionSource::Lsp { lsp_completion, .. }
                        if lsp_completion.kind == Some(CompletionItemKind::SNIPPET)
                    );

                    let sort_text =
                        if let CompletionSource::Lsp { lsp_completion, .. } = &completion.source {
                            lsp_completion.sort_text.as_deref()
                        } else {
                            None
                        };

                    let (sort_kind, sort_label) = completion.sort_key();

                    SortableMatch {
                        string_match,
                        is_snippet,
                        sort_text,
                        sort_kind,
                        sort_label,
                    }
                })
                .collect();

            Self::sort_matches(&mut sortable_items, query, self.snippet_sort_order);

            matches = sortable_items
                .into_iter()
                .map(|sortable| sortable.string_match)
                .collect();
        }

        *self.entries.borrow_mut() = matches;
        self.selected_item = 0;
        // This keeps the display consistent when y_flipped.
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);

        if let Some(provider) = provider {
            cx.update(|window, cx| {
                // Since this is async, it's possible the menu has been closed and possibly even
                // another opened. `provider.selection_changed` should not be called in this case.
                let this_menu_still_active = editor
                    .read_with(cx, |editor, _cx| {
                        editor.with_completions_menu_matching_id(self.id, |menu| menu.is_some())
                    })
                    .unwrap_or(false);
                if this_menu_still_active {
                    self.handle_selection_changed(&*provider, window, cx);
                }
            })
            .ok();
        }
    }
    */
}

#[derive(Debug)]
pub struct SortableMatch<'a> {
    pub string_match: StringMatch,
    pub is_snippet: bool,
    pub sort_text: Option<&'a str>,
    pub sort_kind: usize,
    pub sort_label: &'a str,
}

#[derive(Clone)]
pub struct AvailableCodeAction {
    pub excerpt_id: ExcerptId,
    pub action: CodeAction,
    pub provider: Rc<dyn CodeActionProvider>,
}

#[derive(Clone)]
pub struct CodeActionContents {
    tasks: Option<Rc<ResolvedTasks>>,
    actions: Option<Rc<[AvailableCodeAction]>>,
    debug_scenarios: Vec<DebugScenario>,
    pub(crate) context: TaskContext,
}

impl CodeActionContents {
    pub(crate) fn new(
        tasks: Option<ResolvedTasks>,
        actions: Option<Rc<[AvailableCodeAction]>>,
        debug_scenarios: Vec<DebugScenario>,
        context: TaskContext,
    ) -> Self {
        Self {
            tasks: tasks.map(Rc::new),
            actions,
            debug_scenarios,
            context,
        }
    }

    pub fn tasks(&self) -> Option<&ResolvedTasks> {
        self.tasks.as_deref()
    }

    fn len(&self) -> usize {
        let tasks_len = self.tasks.as_ref().map_or(0, |tasks| tasks.templates.len());
        let code_actions_len = self.actions.as_ref().map_or(0, |actions| actions.len());
        tasks_len + code_actions_len + self.debug_scenarios.len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn iter(&self) -> impl Iterator<Item = CodeActionsItem> + '_ {
        self.tasks
            .iter()
            .flat_map(|tasks| {
                tasks
                    .templates
                    .iter()
                    .map(|(kind, task)| CodeActionsItem::Task(kind.clone(), task.clone()))
            })
            .chain(self.actions.iter().flat_map(|actions| {
                actions.iter().map(|available| CodeActionsItem::CodeAction {
                    excerpt_id: available.excerpt_id,
                    action: available.action.clone(),
                    provider: available.provider.clone(),
                })
            }))
            .chain(
                self.debug_scenarios
                    .iter()
                    .cloned()
                    .map(CodeActionsItem::DebugScenario),
            )
    }

    pub fn get(&self, mut index: usize) -> Option<CodeActionsItem> {
        if let Some(tasks) = &self.tasks {
            if let Some((kind, task)) = tasks.templates.get(index) {
                return Some(CodeActionsItem::Task(kind.clone(), task.clone()));
            } else {
                index -= tasks.templates.len();
            }
        }
        if let Some(actions) = &self.actions {
            if let Some(available) = actions.get(index) {
                return Some(CodeActionsItem::CodeAction {
                    excerpt_id: available.excerpt_id,
                    action: available.action.clone(),
                    provider: available.provider.clone(),
                });
            } else {
                index -= actions.len();
            }
        }

        self.debug_scenarios
            .get(index)
            .cloned()
            .map(CodeActionsItem::DebugScenario)
    }
}

#[derive(Clone)]
pub enum CodeActionsItem {
    Task(TaskSourceKind, ResolvedTask),
    CodeAction {
        excerpt_id: ExcerptId,
        action: CodeAction,
        provider: Rc<dyn CodeActionProvider>,
    },
    DebugScenario(DebugScenario),
}

impl CodeActionsItem {
    fn as_task(&self) -> Option<&ResolvedTask> {
        let Self::Task(_, task) = self else {
            return None;
        };
        Some(task)
    }

    fn as_code_action(&self) -> Option<&CodeAction> {
        let Self::CodeAction { action, .. } = self else {
            return None;
        };
        Some(action)
    }
    fn as_debug_scenario(&self) -> Option<&DebugScenario> {
        let Self::DebugScenario(scenario) = self else {
            return None;
        };
        Some(scenario)
    }

    pub fn label(&self) -> String {
        match self {
            Self::CodeAction { action, .. } => action.lsp_action.title().to_owned(),
            Self::Task(_, task) => task.resolved_label.clone(),
            Self::DebugScenario(scenario) => scenario.label.to_string(),
        }
    }
}

pub struct CodeActionsMenu {
    pub actions: CodeActionContents,
    pub buffer: Entity<Buffer>,
    pub selected_item: usize,
    pub scroll_handle: UniformListScrollHandle,
    pub deployed_from: Option<CodeActionSource>,
}

impl CodeActionsMenu {
    pub fn change_selection(&mut self, change: MenuSelectionChange, cx: &mut Context<Editor>) {
        if self.scroll_handle.y_flipped() {
            change = change.flip();
        }
        let new_selection = change.new_selection(self.selected_item, self.actions.len());
        if new_selection != self.selected_item {
            self.selected_item = new_selection;
            self.scroll_handle
                .scroll_to_item(self.selected_item, ScrollStrategy::Top);
            cx.notify()
        }
    }

    fn visible(&self) -> bool {
        !self.actions.is_empty()
    }

    fn origin(&self) -> ContextMenuOrigin {
        match &self.deployed_from {
            Some(CodeActionSource::Indicator(row)) => ContextMenuOrigin::GutterIndicator(*row),
            Some(CodeActionSource::QuickActionBar) => ContextMenuOrigin::QuickActionBar,
            None => ContextMenuOrigin::Cursor,
        }
    }

    fn render(
        &self,
        _style: &EditorStyle,
        max_height_in_lines: u32,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> AnyElement {
        let actions = self.actions.clone();
        let selected_item = self.selected_item;
        let list = uniform_list(
            cx.entity().clone(),
            "code_actions_menu",
            self.actions.len(),
            move |_this, range, _, cx| {
                actions
                    .iter()
                    .skip(range.start)
                    .take(range.end - range.start)
                    .enumerate()
                    .map(|(ix, action)| {
                        let item_ix = range.start + ix;
                        let selected = item_ix == selected_item;
                        let colors = cx.theme().colors();
                        div().min_w(px(220.)).max_w(px(540.)).child(
                            ListItem::new(item_ix)
                                .inset(true)
                                .toggle_state(selected)
                                .when_some(action.as_code_action(), |this, action| {
                                    this.child(
                                        h_flex()
                                            .overflow_hidden()
                                            .child(
                                                // TASK: It would be good to make lsp_action.title a SharedString to avoid allocating here.
                                                action.lsp_action.title().replace("\n", ""),
                                            )
                                            .when(selected, |this| {
                                                this.text_color(colors.text_accent)
                                            }),
                                    )
                                })
                                .when_some(action.as_task(), |this, task| {
                                    this.child(
                                        h_flex()
                                            .overflow_hidden()
                                            .child(task.resolved_label.replace("\n", ""))
                                            .when(selected, |this| {
                                                this.text_color(colors.text_accent)
                                            }),
                                    )
                                })
                                .when_some(action.as_debug_scenario(), |this, scenario| {
                                    this.child(
                                        h_flex()
                                            .overflow_hidden()
                                            .child("debug: ")
                                            .child(scenario.label.clone())
                                            .when(selected, |this| {
                                                this.text_color(colors.text_accent)
                                            }),
                                    )
                                })
                                .on_click(cx.listener(move |editor, _, window, cx| {
                                    cx.stop_propagation();
                                    if let Some(task) = editor.confirm_code_action(
                                        &ConfirmCodeAction {
                                            item_ix: Some(item_ix),
                                        },
                                        window,
                                        cx,
                                    ) {
                                        task.detach_and_log_err(cx)
                                    }
                                })),
                        )
                    })
                    .collect()
            },
        )
        .occlude()
        .max_h(max_height_in_lines as f32 * window.line_height())
        .track_scroll(self.scroll_handle.clone())
        .with_width_from_item(
            self.actions
                .iter()
                .enumerate()
                .max_by_key(|(_, action)| match action {
                    CodeActionsItem::Task(_, task) => task.resolved_label.chars().count(),
                    CodeActionsItem::CodeAction { action, .. } => {
                        action.lsp_action.title().chars().count()
                    }
                    CodeActionsItem::DebugScenario(scenario) => {
                        format!("debug: {}", scenario.label).chars().count()
                    }
                })
                .map(|(ix, _)| ix),
        )
        .with_sizing_behavior(ListSizingBehavior::Infer);

        Popover::new().child(list).into_any_element()
    }
}
