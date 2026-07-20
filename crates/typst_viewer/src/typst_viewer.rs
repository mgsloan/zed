//! Typst preview: a viewer for page images streamed by tinymist.
//!
//! This file owns *where and how big*: list layout, scroll, zoom, placeholder
//! boxes, and which indices are visible. It performs no decoding and holds no
//! page memory — [`preview_session::PreviewSession`] owns *what* to show.

pub mod decode;
pub mod page_table;
pub mod preview_session;
pub mod protocol;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ListAlignment,
    ListOffset, ListState, Render, SharedString, Subscription, Window, list, px,
};
use language::Buffer;
use multi_buffer::MultiBuffer;
use project::Project;
use ui::prelude::*;
use workspace::item::Item;
use workspace::{Pane, Workspace};

use crate::page_table::{PageImage, TableDelta};
use crate::preview_session::{ConnectionStatus, PreviewSession, SessionEvent, Viewport};

pub use crate::preview_session::{
    StartPreviewResponse, TINYMIST_SERVER_NAME, find_tinymist_server,
    register_tinymist_notifications,
};
pub use zed_actions::preview::typst::{OpenPreview, OpenPreviewToTheSide};

/// Extra items the list renders beyond the visible range.
const OVERDRAW: f32 = 1024.0;
/// Pages beyond the visible range to subscribe to, derived from `OVERDRAW` so
/// we never ask for pages the list will not render, nor render pages we never
/// subscribed to.
const PREFETCH_MARGIN: usize = 2;
/// Gap between pages, in display pixels.
const PAGE_GAP: f32 = 12.0;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        TypstPreviewView::register(workspace, window, cx);
    })
    .detach();
}

pub struct TypstPreviewView {
    session: Entity<PreviewSession>,
    source_buffer: Option<Entity<Buffer>>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    list_state: ListState,
    zoom: f32,
    _subscription: Subscription,
}

impl TypstPreviewView {
    pub fn new(
        active_buffer: Entity<MultiBuffer>,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let source_buffer = active_buffer.read_with(cx, |buffer, _cx| buffer.as_singleton());
            let session = cx.new(|cx| {
                PreviewSession::new(project.clone(), source_buffer.clone(), cx)
            });

            let subscription = cx.subscribe(&session, Self::on_session_event);
            let list_state = ListState::new(0, ListAlignment::Top, px(OVERDRAW));

            // The list did the layout, so it knows the visible range; the old
            // code estimated it by dividing scroll offset by a guessed uniform
            // page height, which is wrong the moment a document mixes page
            // sizes.
            list_state.set_scroll_handler({
                let view = cx.weak_entity();
                move |event, window, cx| {
                    let range = event.visible_range.clone();
                    view.update(cx, |view, cx| {
                        let scale = window.scale_factor() * view.zoom;
                        view.session.update(cx, |session, cx| {
                            session.set_viewport(viewport_for(range), scale, cx);
                        });
                    })
                    .ok();
                }
            });

            // Closing the pane must stop the tinymist-side render loop. This
            // matters more than it used to: with server-side rendering a leaked
            // preview keeps compiling and rasterizing 8 MB pages into a socket
            // nobody is reading.
            cx.on_release(|this: &mut Self, cx| {
                preview_session::kill_preview(&this.project, &this.source_buffer, cx);
            })
            .detach();

            Self {
                session,
                source_buffer,
                project,
                focus_handle: cx.focus_handle(),
                list_state,
                zoom: 1.0,
                _subscription: subscription,
            }
        })
    }

    fn on_session_event(
        &mut self,
        _session: Entity<PreviewSession>,
        event: &SessionEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::StatusChanged | SessionEvent::PagesChanged(_) => cx.notify(),
            SessionEvent::TableChanged(delta) => {
                self.apply_table_delta(delta, cx);
                cx.notify();
            }
        }
    }

    /// Keeps `ListState` in sync with the page table.
    ///
    /// Three questions, three mechanisms; conflating them is the easy way to get
    /// this wrong:
    ///
    /// - *Which slots exist?* -> `splice`
    /// - *Which existing slots changed height?* -> `remeasure_items`
    /// - *Where should the viewport point afterwards?* -> the content anchor
    fn apply_table_delta(&mut self, delta: &TableDelta, cx: &mut Context<Self>) {
        // Capture the anchor before the table moves under us.
        let anchor = self.list_state.logical_scroll_top();
        let anchor_content = self
            .session
            .read(cx)
            .table()
            .content_at(anchor.item_ix);

        let old_count = self.list_state.item_count();
        let new_count = delta.total;

        // For `ListState` an index is purely a position, and our structural
        // change is only ever at the tail even when the content change starts
        // anywhere. Restricting `splice` to the tail sidesteps its sharp edge:
        // a range that contains the anchor collapses the anchor to the range
        // start, so the tempting `splice(0..old, new)` would jump to the top on
        // every repagination and discard every cached measurement.
        if new_count > old_count {
            self.list_state.splice(old_count..old_count, new_count - old_count);
        } else if new_count < old_count {
            self.list_state.splice(new_count..old_count, 0);
        }

        if let Some(first) = delta.geometry_changed.iter().min() {
            self.list_state.remeasure_items(*first..new_count);
        }

        // `splice`/`remeasure_items` pin the same *index*. We want the same
        // *content*, because editing above the viewport shifts your page down
        // without changing it.
        if delta.content_moved
            && let Some(content) = anchor_content
            && let Some(index) = self
                .session
                .read(cx)
                .table()
                .find_content(content, anchor.item_ix)
            && index != anchor.item_ix
        {
            self.list_state.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: anchor.offset_in_item,
            });
        }
        // else: the index anchor the list already preserved is the fallback.
    }

    fn render_page(&self, index: usize, cx: &App) -> AnyElement {
        let session = self.session.read(cx);
        let Some(entry) = session.table().get(index) else {
            return div().into_any_element();
        };

        // Display size comes from the *table*, never from the image's pixel
        // dimensions. Deriving it from the image is what makes a page resize as
        // it loads; deriving it from the table means the placeholder and the
        // loaded page occupy identical space.
        let width = px(entry.size.width * self.zoom);
        let height = px(entry.size.height * self.zoom);

        let content = match session.image_at(index) {
            // A stale image is correct content at the wrong resolution, so it is
            // drawn in the right place at the right size and merely looks soft.
            // Deliberately not dimmed or spinner-overlaid: it is the steady
            // state during any zoom gesture, and drawing attention to it makes a
            // smooth interaction look broken.
            PageImage::Current(image) | PageImage::Stale(image) => {
                gpui::img(gpui::ImageSource::Render(image))
                    .debug_selector(|| "TYPST_PREVIEW_IMG".into())
                    .w(width)
                    .h(height)
                    .into_any_element()
            }
            PageImage::Failed => div()
                .w(width)
                .h(height)
                .bg(gpui::rgb(0xffffff))
                .flex()
                .items_center()
                .justify_center()
                .child(SharedString::from(format!(
                    "Page {} could not be rendered",
                    index + 1
                )))
                .into_any_element(),
            PageImage::Missing => div()
                .w(width)
                .h(height)
                .bg(gpui::rgb(0xffffff))
                .into_any_element(),
        };

        div()
            .flex()
            .justify_center()
            .pb(px(PAGE_GAP))
            .child(content)
            .into_any_element()
    }

    pub fn resolve_active_item_as_typst_buffer(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<MultiBuffer>> {
        workspace
            .active_item(cx)?
            .act_as::<MultiBuffer>(cx)
            .filter(|buffer| Self::is_typst_file(buffer, cx))
    }

    pub fn is_typst_file(buffer: &Entity<MultiBuffer>, cx: &App) -> bool {
        buffer
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .is_some_and(|file| {
                std::path::Path::new(file.file_name(cx))
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("typ"))
            })
    }

    fn find_existing_preview_item_idx(
        pane: &Pane,
        buffer: &Entity<MultiBuffer>,
        cx: &App,
    ) -> Option<usize> {
        let buffer_id = buffer.read(cx).as_singleton()?.entity_id();
        pane.items_of_type::<TypstPreviewView>()
            .find(|view| {
                view.read(cx)
                    .source_buffer
                    .as_ref()
                    .is_some_and(|buffer| buffer.entity_id() == buffer_id)
            })
            .and_then(|view| pane.index_for_item(&view))
    }

    /// Open (or focus an existing) preview for the active typst buffer. When
    /// `to_the_side`, the preview goes in a split to the right and doesn't steal
    /// focus; otherwise it opens (and activates) in the current pane.
    fn open_preview(
        workspace: &mut Workspace,
        to_the_side: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(buffer) = Self::resolve_active_item_as_typst_buffer(workspace, cx) else {
            return;
        };
        let project = workspace.project().clone();
        let view = TypstPreviewView::new(buffer.clone(), project, window, cx);
        let pane = if to_the_side {
            workspace
                .find_pane_in_direction(workspace::SplitDirection::Right, cx)
                .unwrap_or_else(|| {
                    workspace.split_pane(
                        workspace.active_pane().clone(),
                        workspace::SplitDirection::Right,
                        window,
                        cx,
                    )
                })
        } else {
            workspace.active_pane().clone()
        };
        pane.update(cx, |pane, cx| {
            if let Some(existing_idx) = Self::find_existing_preview_item_idx(pane, &buffer, cx) {
                pane.activate_item(existing_idx, true, true, window, cx);
            } else {
                let activate = !to_the_side;
                pane.add_item(Box::new(view), activate, activate, None, window, cx);
            }
        });
        cx.notify();
    }

    pub fn register(workspace: &mut Workspace, _window: &mut Window, _cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &OpenPreview, window, cx| {
            Self::open_preview(workspace, false, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenPreviewToTheSide, window, cx| {
            Self::open_preview(workspace, true, window, cx);
        });
    }
}

fn viewport_for(visible: std::ops::Range<usize>) -> Viewport {
    let prefetch_start = visible.start.saturating_sub(PREFETCH_MARGIN);
    let prefetch_end = visible.end + PREFETCH_MARGIN;
    Viewport {
        visible,
        prefetch: prefetch_start..prefetch_end,
    }
}

impl Render for TypstPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let total = session.table().len();

        let content = match session.status() {
            ConnectionStatus::Connecting => centered("Connecting to preview server…".to_string()),
            ConnectionStatus::Error { message } => centered(format!("Error: {message}")),
            ConnectionStatus::Disconnected { reason } => centered(format!("Disconnected: {reason}")),
            ConnectionStatus::Connected if total == 0 => {
                centered("Waiting for the document to compile…".to_string())
            }
            ConnectionStatus::Connected => {
                if self.list_state.item_count() != total {
                    self.list_state.reset(total);
                }
                let view = cx.entity().downgrade();
                list(self.list_state.clone(), move |index, _window, cx| {
                    view.read_with(cx, |view, cx| view.render_page(index, cx))
                        .unwrap_or_else(|_| div().into_any_element())
                })
                .flex_1()
                .into_any_element()
            }
        };

        div()
            .key_context("TypstPreview")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0xf0f0f0))
            .child(content)
    }
}

fn centered(message: String) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(SharedString::from(message))
        .into_any_element()
}

impl Focusable for TypstPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for TypstPreviewView {}

impl Item for TypstPreviewView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.source_buffer
            .as_ref()
            .and_then(|buffer| buffer.read(cx).file())
            .map(|file| SharedString::from(format!("Preview {}", file.file_name(cx))))
            .unwrap_or_else(|| SharedString::from("Typst Preview"))
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Eye))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Typst Preview Opened")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use gpui::RenderImage;
    use std::sync::Arc;
    use gpui::{TestAppContext, div, px};
    use image::Frame;
    use smallvec::SmallVec;

    /// A minimal view that displays a RenderImage the same way TypstPreviewView does.
    struct TestImageView {
        image: Arc<RenderImage>,
    }

    impl gpui::Render for TestImageView {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            let image_size = self.image.size(0);
            let image_w = image_size.width.0 as f32;
            let image_h = image_size.height.0 as f32;
            let aspect = if image_w > 0.0 {
                image_h / image_w
            } else {
                1.0
            };
            let display_w = px(image_w / 2.0);
            let display_h = px(image_w / 2.0 * aspect);

            div()
                .id("test-container")
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .overflow_y_scroll()
                .child(
                    gpui::img(gpui::ImageSource::Render(self.image.clone()))
                        .id(ElementId::Integer(self.image.id.0 as u64))
                        .debug_selector(|| "TEST_IMG".into())
                        .w(display_w)
                        .h(display_h),
                )
        }
    }

    fn make_test_image(width: u32, height: u32, scale: f32) -> Arc<RenderImage> {
        // Create a minimal BGRA pixmap.
        let data = vec![128u8; (width * height * 4) as usize];
        let buffer =
            image::ImageBuffer::from_raw(width, height, data).expect("buffer size mismatch");
        Arc::new(
            RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1)).with_scale_factor(scale),
        )
    }

    #[gpui::test]
    async fn test_image_display_bounds_consistent_across_updates(cx: &mut TestAppContext) {
        // Create a window with a TestImageView showing image #1.
        let image1 = make_test_image(1119, 1588, 2.0);
        let (view, cx) = cx.add_window_view(|_window, _cx| TestImageView {
            image: image1.clone(),
        });

        // Read bounds of image #1.
        let bounds1 = cx.debug_bounds("TEST_IMG");
        assert!(
            bounds1.is_some(),
            "TEST_IMG element should exist after first render"
        );
        let bounds1 = bounds1.unwrap();
        assert!(
            bounds1.size.width.as_f32() > 100.0,
            "Image should have reasonable width, got {}",
            bounds1.size.width.as_f32(),
        );

        // Swap to image #2 (same pixel dimensions, different RenderImage instance).
        let image2 = make_test_image(1119, 1588, 2.0);
        assert_ne!(
            image1.id, image2.id,
            "Two RenderImages should have different IDs"
        );

        view.update_in(cx, |view, _window, cx| {
            view.image = image2;
            cx.notify();
        });
        cx.run_until_parked();

        // Read bounds of image #2.
        let bounds2 = cx.debug_bounds("TEST_IMG");
        assert!(
            bounds2.is_some(),
            "TEST_IMG element should exist after second render"
        );
        let bounds2 = bounds2.unwrap();

        // The display bounds must be identical.
        assert_eq!(
            bounds1.size.width.as_f32(),
            bounds2.size.width.as_f32(),
            "Width changed between image updates: {} -> {}",
            bounds1.size.width.as_f32(),
            bounds2.size.width.as_f32(),
        );
        assert_eq!(
            bounds1.size.height.as_f32(),
            bounds2.size.height.as_f32(),
            "Height changed between image updates: {} -> {}",
            bounds1.size.height.as_f32(),
            bounds2.size.height.as_f32(),
        );
    }

    #[gpui::test]
    async fn test_image_display_size_matches_expected_dimensions(cx: &mut TestAppContext) {
        // 1119x1588 pixels at 2x scale → 559.5x794 display points
        let image = make_test_image(1119, 1588, 2.0);
        let (_view, cx) = cx.add_window_view(|_window, _cx| TestImageView { image });

        let bounds = cx.debug_bounds("TEST_IMG").expect("TEST_IMG should exist");

        // The display width should be pixel_width / scale = 1119 / 2 = 559.5
        let expected_w = 1119.0 / 2.0;
        let expected_h = 1588.0 / 2.0;
        assert!(
            (bounds.size.width.as_f32() - expected_w).abs() < 1.0,
            "Expected display width ~{expected_w}, got {}",
            bounds.size.width.as_f32(),
        );
        assert!(
            (bounds.size.height.as_f32() - expected_h).abs() < 1.0,
            "Expected display height ~{expected_h}, got {}",
            bounds.size.height.as_f32(),
        );
    }
}
