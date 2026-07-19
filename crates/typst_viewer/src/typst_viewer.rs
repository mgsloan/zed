use anyhow::{Context as _, Result};
use async_tungstenite::tungstenite::client::IntoClientRequest as _;
use async_tungstenite::{WebSocketStream, tungstenite::Message};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    App, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    RenderImage, ScrollHandle, SharedString, Task, Window,
};
use language::Buffer;
use lsp::{LanguageServer, LanguageServerId, LanguageServerName, Subscription};
use multi_buffer::MultiBuffer;
use project::Project;
use serde::Deserialize;
use settings::Settings as _;
use smol::net::TcpStream;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ui::{WithScrollbar, prelude::*};
use workspace::item::Item;
use workspace::{Pane, Workspace};

/// Track which LSP server IDs already have notification handlers registered,
/// so we don't panic on double-registration when opening multiple previews.
static REGISTERED_SERVERS: Mutex<Option<HashSet<LanguageServerId>>> = Mutex::new(None);

pub use zed_actions::preview::typst::{OpenPreview, OpenPreviewToTheSide};

pub const TINYMIST_SERVER_NAME: LanguageServerName = LanguageServerName::new_static("tinymist");

/// Response from tinymist's `doStartPreview` / `startPreview` command.
///
/// tinymist also returns `staticServerPort`/`staticServerAddr` and `isPrimary`, but these are not needed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartPreviewResponse {
    pub data_plane_port: Option<u16>,
}

/// tinymist sends document outline notifications that Zed doesn't consume.
/// Register a no-op handler to suppress "unhandled notification" log spam.
#[derive(Debug)]
enum DocumentOutline {}

impl lsp::notification::Notification for DocumentOutline {
    type Params = serde_json::Value;
    const METHOD: &'static str = "tinymist/documentOutline";
}

/// Register handlers for tinymist-specific notifications.
/// Returns a subscription that must be kept alive (stored or detached).
/// Safe to call multiple times for the same server — subsequent calls
/// return None instead of panicking on double-registration.
pub fn register_tinymist_notifications(server: &LanguageServer) -> Option<Subscription> {
    let server_id = server.server_id();
    let mut guard = REGISTERED_SERVERS.lock().unwrap_or_else(|e| e.into_inner());
    let set = guard.get_or_insert_with(HashSet::new);
    if !set.insert(server_id) {
        // Already registered for this server.
        return None;
    }
    Some(server.on_notification::<DocumentOutline, _>(|_params, _cx| {}))
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        TypstPreviewView::register(workspace, window, cx);
    })
    .detach();
}

/// Find the tinymist language server for a given buffer.
///
/// Searches by buffer association first, then falls back to scanning all
/// running language servers for one named "tinymist" (mirrors the
/// rust-analyzer pattern).
pub fn find_tinymist_server(
    project: &Project,
    buffer: Option<&Buffer>,
    cx: &App,
) -> Option<LanguageServerId> {
    buffer
        .and_then(|buffer| project.language_server_id_for_name(buffer, &TINYMIST_SERVER_NAME, cx))
        .or_else(|| {
            let servers: Vec<_> = project
                .language_server_statuses(cx)
                .filter_map(|(server_id, status)| {
                    if status.name == TINYMIST_SERVER_NAME {
                        Some(server_id)
                    } else {
                        None
                    }
                })
                .collect();
            if servers.len() == 1 {
                servers.first().copied()
            } else {
                None
            }
        })
}

/// Send `tinymist.doStartPreview` to the language server and return the
/// WebSocket URL for the data plane.
///
/// The command accepts CLI-style arguments as a JSON array of strings.
/// We pass `["--server-svg", "--strip-svg-glyph-defs", "--data-plane-host=127.0.0.1:0"]` so that:
/// - tinymist renders complete SVG server-side
/// - unchanged glyph defs are stripped after the first frame (~200KB vs ~2MB)
/// - the data plane binds to an OS-assigned port
pub async fn start_preview_via_lsp(
    server: Arc<LanguageServer>,
    entry_path: &std::path::Path,
    request_timeout: Duration,
) -> Result<String> {
    let entry_str = entry_path
        .to_str()
        .context("entry file path is not valid UTF-8")?;

    // Use the file path as the task ID so each file gets its own preview
    // and we can kill/restart a specific one without affecting others.
    let task_id = entry_str.to_string();

    // Kill any existing preview for this file — tinymist only allows one
    // preview per task ID, and the previous one may not have been cleaned
    // up (e.g. the pane was closed without sending doKillPreview).
    let kill_params = lsp::ExecuteCommandParams {
        command: "tinymist.doKillPreview".into(),
        arguments: vec![serde_json::json!(task_id)],
        ..Default::default()
    };
    log::info!("typst_viewer: sending tinymist.doKillPreview for {task_id}");
    let _ = server
        .request::<lsp::request::ExecuteCommand>(kill_params, request_timeout)
        .await;

    let args: Vec<String> = vec![
        "--server-svg".into(),
        "--strip-svg-glyph-defs".into(),
        "--data-plane-host=127.0.0.1:0".into(),
        format!("--task-id={task_id}"),
        entry_str.into(),
    ];

    let params = lsp::ExecuteCommandParams {
        command: "tinymist.doStartPreview".into(),
        arguments: vec![serde_json::to_value(args)?],
        ..Default::default()
    };

    log::info!("typst_viewer: sending tinymist.doStartPreview");

    let result = server
        .request::<lsp::request::ExecuteCommand>(params, request_timeout)
        .await
        .into_response()
        .context("tinymist.doStartPreview request failed")?;

    let response: StartPreviewResponse =
        serde_json::from_value(result.context("tinymist.doStartPreview returned null")?)
            .context("failed to parse StartPreviewResponse")?;

    log::info!("typst_viewer: StartPreviewResponse: {response:?}");

    let port = response
        .data_plane_port
        .context("StartPreviewResponse missing data_plane_port")?;

    Ok(format!("ws://127.0.0.1:{port}"))
}

/// Connect to a preview server's WebSocket endpoint.
///
/// Returns the WebSocket stream. Callers can use `futures::StreamExt::next()`
/// to read messages and `futures::SinkExt::send()` to write, or call
/// `.split()` to get independent read/write halves.
pub async fn connect(url: &str) -> Result<WebSocketStream<TcpStream>> {
    let parsed_url = url::Url::parse(url).context("parsing WebSocket URL")?;
    let host = parsed_url
        .host_str()
        .context("WebSocket URL missing host")?;
    let port = parsed_url.port().unwrap_or(80);
    let addr = format!("{host}:{port}");

    log::info!("typst_viewer: connecting to preview server at {addr}");

    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("TCP connect to {addr}"))?;

    let mut request = url
        .into_client_request()
        .context("building WebSocket request")?;
    request.headers_mut().insert(
        "Origin",
        format!("http://{addr}")
            .parse()
            .context("building Origin header")?,
    );

    let (ws, _response) = async_tungstenite::client_async(request, tcp)
        .await
        .context("WebSocket handshake failed")?;

    log::info!("typst_viewer: WebSocket connected to {addr}");

    Ok(ws)
}

enum PreviewState {
    Connecting,
    Rendering {
        pages: Vec<Option<Arc<RenderImage>>>,
    },
    Disconnected {
        reason: String,
    },
    Error {
        message: String,
    },
}

pub struct TypstPreviewView {
    source_buffer: Option<Entity<Buffer>>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    state: PreviewState,
    _connection_task: Task<()>,
    _lsp_subscriptions: Vec<lsp::Subscription>,
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
            let focus_handle = cx.focus_handle();

            let mut this = Self {
                source_buffer,
                project: project.clone(),
                focus_handle,
                scroll_handle: ScrollHandle::new(),
                state: PreviewState::Connecting,
                _connection_task: Task::ready(()),
                _lsp_subscriptions: Vec::new(),
            };

            this.start_connection(cx);
            this
        })
    }

    fn start_connection(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let source_buffer = self.source_buffer.clone();

        self._connection_task = cx.spawn(async move |this, cx| {
            if let Err(err) = Self::connect_and_receive(project, source_buffer, &this, cx).await {
                log::error!("typst_viewer: connection failed: {err:#}");
                this.update(cx, |this, cx| {
                    this.state = PreviewState::Error {
                        message: format!("{err:#}"),
                    };
                    cx.notify();
                })
                .ok();
            }
        });
    }

    /// Try to get a WebSocket URL by asking the tinymist LSP to start a
    /// preview. Returns `None` if tinymist isn't available.
    /// Also returns any LSP subscriptions that must be kept alive.
    async fn request_preview_url(
        project: &Entity<Project>,
        source_buffer: &Option<Entity<Buffer>>,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<(String, Vec<lsp::Subscription>)> {
        let (server, request_timeout, entry_path) = project.read_with(cx, |project, cx| {
            let buffer = source_buffer.as_ref().map(|b| b.read(cx));
            let server_id = crate::find_tinymist_server(project, buffer, cx)
                .context("tinymist language server not found")?;
            let server = project
                .lsp_store()
                .read(cx)
                .language_server_for_id(server_id)
                .context("tinymist server not running")?;
            let request_timeout = project::project_settings::ProjectSettings::get_global(cx)
                .global_lsp_settings
                .get_request_timeout();
            let entry_path = source_buffer
                .as_ref()
                .and_then(|b| b.read(cx).file())
                .and_then(|file| file.as_local())
                .map(|file| file.abs_path(cx))
                .context("buffer has no file path")?;
            anyhow::Ok((server, request_timeout, entry_path))
        })?;

        // Suppress "unhandled notification" log spam from tinymist.
        // Returns None if already registered for this server (safe on re-open).
        let subscription = crate::register_tinymist_notifications(&server);

        let url = crate::start_preview_via_lsp(server, &entry_path, request_timeout).await?;
        log::info!("typst_viewer: LSP provided WebSocket URL: {url}");
        let subscriptions: Vec<_> = subscription.into_iter().collect();
        Ok((url, subscriptions))
    }

    async fn connect_and_receive(
        project: Entity<Project>,
        source_buffer: Option<Entity<Buffer>>,
        this: &gpui::WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<()> {
        // tinymist may dispose the preview immediately after starting it
        // (e.g. if a workspace/didChangeConfiguration triggers a project
        // reload).  Retry a few times with a delay to let it settle.
        let max_attempts = 3;
        for attempt in 1..=max_attempts {
            match Self::try_connect_and_receive(&project, &source_buffer, this, cx).await {
                Ok(()) => return Ok(()),
                Err(err) if attempt == max_attempts => return Err(err),
                Err(err) => {
                    log::warn!(
                        "typst_viewer: attempt {attempt}/{max_attempts} failed: {err:#}, retrying in 1s"
                    );
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                }
            }
        }
        unreachable!()
    }

    async fn try_connect_and_receive(
        project: &Entity<Project>,
        source_buffer: &Option<Entity<Buffer>>,
        this: &gpui::WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<()> {
        let (url, lsp_subscriptions) =
            Self::request_preview_url(project, source_buffer, cx).await?;

        if !lsp_subscriptions.is_empty() {
            this.update(cx, |this, _cx| {
                this._lsp_subscriptions.extend(lsp_subscriptions);
            })
            .ok();
        }

        log::info!("typst_viewer: connecting to LSP-provided preview at {url}");
        let mut ws = connect(&url)
            .await
            .with_context(|| format!("failed to connect to preview server at {url}"))?;

        // tinymist expects the client to send "current" to trigger a full render.
        log::info!("typst_viewer: sending 'current' to request initial render");
        ws.send(Message::text("current")).await?;

        Self::receive_loop(&mut ws, this, cx).await
    }

    /// Parse a single WebSocket text message into page metadata + SVG bytes,
    /// or return None for non-SVG messages (which are logged and skipped).
    fn parse_svg_message(text: &str) -> Option<(usize, usize, Vec<u8>)> {
        let (page_index, page_total, svg_text) =
            if let Some((header, svg)) = parse_page_header(text) {
                (header.index, header.total, svg)
            } else if text.contains("<svg") {
                (0, 1, text)
            } else {
                let preview = &text[..text.len().min(120)];
                log::info!(
                    "typst_viewer: received text message ({} bytes): {preview}",
                    text.len()
                );
                return None;
            };

        if !svg_text.contains("<svg") {
            log::warn!("typst_viewer: page {page_index}/{page_total} has no <svg tag, skipping");
            return None;
        }

        Some((page_index, page_total, svg_text.as_bytes().to_vec()))
    }

    async fn receive_loop(
        ws: &mut WebSocketStream<TcpStream>,
        this: &gpui::WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<()> {
        let mut cached_glyph_defs: HashMap<usize, String> = HashMap::new();
        let svg_renderer = cx.update(|cx| cx.svg_renderer());

        while let Some(msg_result) = ws.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    // --- Phase 1: Parse the triggering message ---
                    let Some((page_index, page_total, svg_bytes)) = Self::parse_svg_message(&text)
                    else {
                        continue;
                    };

                    // Collect the latest SVG for each page.  Start with
                    // the message we just received, then drain everything
                    // queued behind it so we only rasterize the freshest
                    // compile output.
                    let mut latest: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
                    let mut latest_total = page_total;
                    latest.insert(page_index, svg_bytes);
                    let mut skipped = 0u64;

                    // Drain everything queued behind the triggering message.
                    // Stops at the first non-text message or when the queue is
                    // empty (`now_or_never` yields None).
                    while let Some(Some(Ok(Message::Text(newer)))) = ws.next().now_or_never() {
                        if let Some((pi, pt, bytes)) = Self::parse_svg_message(&newer) {
                            // If this starts a newer batch (page 0 with a
                            // possibly different total), clear stale pages from
                            // the previous batch.
                            if pi == 0 && pt != latest_total {
                                latest.clear();
                                latest_total = pt;
                            }
                            if latest.insert(pi, bytes).is_some() {
                                skipped += 1;
                            }
                        }
                    }
                    if skipped > 0 {
                        log::info!(
                            "typst_viewer: frame drop — kept {} pages, \
                             skipped {skipped} stale SVGs",
                            latest.len(),
                        );
                    }

                    // --- Phase 2: Rasterize pages, visible first ---
                    // Read the current scroll offset to prioritise the page
                    // the user is actually looking at.
                    let visible_page = this
                        .update(cx, |this, _cx| {
                            let scroll_y: f32 = this.scroll_handle.offset().y.abs().into();
                            // Each page is roughly the same height.  Estimate
                            // which page index is at the current scroll position.
                            let page_count = match &this.state {
                                PreviewState::Rendering { pages } => pages.len().max(1),
                                _ => latest_total,
                            };
                            // Use the first rendered page to get the height,
                            // or fall back to a reasonable default.
                            let page_height = match &this.state {
                                PreviewState::Rendering { pages } => {
                                    pages
                                        .iter()
                                        .find_map(|p| {
                                            let img = p.as_ref()?;
                                            let h = img.size(0).height.0 as f32 / 2.0;
                                            Some(h + 12.0) // display_h + page_gap
                                        })
                                        .unwrap_or(1200.0)
                                }
                                _ => 1200.0,
                            };
                            let idx = (scroll_y / page_height) as usize;
                            idx.min(page_count.saturating_sub(1))
                        })
                        .unwrap_or(0);

                    // Sort page indices: visible page first, then nearest
                    // neighbours expanding outward, then the rest.
                    let mut page_order: Vec<usize> = latest.keys().copied().collect();
                    page_order.sort_by_key(|&idx| {
                        let dist = (idx as isize - visible_page as isize).unsigned_abs();
                        dist
                    });

                    for &page_index in &page_order {
                        let Some(svg_bytes) = latest.get(&page_index) else {
                            continue;
                        };
                        let svg_bytes = resolve_glyph_defs(
                            svg_bytes.clone(),
                            page_index,
                            &mut cached_glyph_defs,
                        );

                        let raster_start = std::time::Instant::now();
                        let image_result = cx
                            .background_executor()
                            .spawn({
                                let svg_renderer = svg_renderer.clone();
                                async move {
                                    svg_renderer
                                        .render_single_frame(&svg_bytes, 1.0)
                                        .context("failed to rasterize typst SVG")
                                }
                            })
                            .await;

                        let elapsed_ms = raster_start.elapsed().as_secs_f64() * 1000.0;

                        match image_result {
                            Ok(image) => {
                                log::info!(
                                    "typst_viewer: page {page_index}/{latest_total} \
                                     rasterized in {elapsed_ms:.0}ms"
                                );
                                this.update(cx, |this, cx| {
                                    if !matches!(this.state, PreviewState::Rendering { .. }) {
                                        this.state = PreviewState::Rendering {
                                            pages: vec![None; latest_total],
                                        };
                                    }
                                    let PreviewState::Rendering { pages } = &mut this.state else {
                                        unreachable!("state was just set to Rendering");
                                    };
                                    pages.resize(latest_total, None);
                                    if page_index < pages.len() {
                                        pages[page_index] = Some(image);
                                    }
                                    cx.notify();
                                })?;
                            }
                            Err(err) => {
                                log::error!(
                                    "typst_viewer: rasterization failed in \
                                     {elapsed_ms:.0}ms: {err}"
                                );
                            }
                        }
                    }
                }
                Ok(Message::Binary(data)) => {
                    log::debug!(
                        "typst_viewer: received binary message ({} bytes), skipping",
                        data.len()
                    );
                }
                Ok(Message::Close(frame)) => {
                    log::info!("typst_viewer: server closed WebSocket: {frame:?}");
                    this.update(cx, |this, cx| {
                        this.state = PreviewState::Disconnected {
                            reason: "Server closed connection".into(),
                        };
                        cx.notify();
                    })?;
                    break;
                }
                Ok(Message::Ping(_)) => {
                    log::debug!("typst_viewer: received ping");
                }
                Ok(other) => {
                    log::debug!("typst_viewer: received other message: {other:?}");
                }
                Err(err) => {
                    log::error!("typst_viewer: WebSocket receive error: {err}");
                    this.update(cx, |this, cx| {
                        this.state = PreviewState::Disconnected {
                            reason: format!("WebSocket error: {err}"),
                        };
                        cx.notify();
                    })?;
                    break;
                }
            }
        }

        Ok(())
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

impl Render for TypstPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.state {
            PreviewState::Connecting => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(SharedString::from("Connecting to preview server…"))
                .into_any_element(),

            PreviewState::Rendering { pages } => {
                log::debug!("typst_viewer: [render] {} pages", pages.len());

                let page_gap = 12.0_f32;

                let mut pages_column = div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(gpui::px(page_gap));

                for (i, page_opt) in pages.iter().enumerate() {
                    match page_opt {
                        Some(image) => {
                            let image_size = image.size(0);
                            let image_w = image_size.width.0 as f32;
                            let image_h = image_size.height.0 as f32;
                            let aspect = if image_w > 0.0 {
                                image_h / image_w
                            } else {
                                1.0
                            };
                            let display_w = image_w / 2.0;
                            let display_h = image_w / 2.0 * aspect;

                            pages_column = pages_column.child(
                                div().pb(gpui::px(page_gap)).child(
                                    gpui::img(gpui::ImageSource::Render(image.clone()))
                                        .id(ElementId::Integer(image.id.0 as u64))
                                        .debug_selector(|| "TYPST_PREVIEW_IMG".into())
                                        .w(gpui::px(display_w))
                                        .h(gpui::px(display_h)),
                                ),
                            );
                        }
                        None => {
                            pages_column = pages_column.child(
                                div()
                                    .pb(gpui::px(page_gap))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .h(gpui::px(200.0))
                                    .child(SharedString::from(format!("Loading page {}…", i + 1))),
                            );
                        }
                    }
                }

                div()
                    .id("typst-viewer-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .child(pages_column)
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx)
                    .into_any_element()
            }

            PreviewState::Disconnected { reason } => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(SharedString::from(format!("Disconnected: {reason}")))
                .into_any_element(),

            PreviewState::Error { message } => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(SharedString::from(format!("Error: {message}")))
                .into_any_element(),
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
            .map(|file| format!("Preview {}", file.file_name(cx)).into())
            .unwrap_or_else(|| "Typst Preview".into())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Eye))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("typst preview: open")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}

// =========================================================================
// Live-path helpers: glyph defs caching, page header parsing
// =========================================================================

pub const GLYPH_DEFS_OPEN: &str = r#"<defs id="glyph">"#;
pub const DEFS_CLOSE: &str = "</defs>";

pub struct PageHeader {
    pub index: usize,
    pub total: usize,
}

/// Parse a `page:{index}:{total}\n` prefix from a server message.
pub fn parse_page_header(text: &str) -> Option<(PageHeader, &str)> {
    let rest = text.strip_prefix("page:")?;
    let newline_pos = rest.find('\n')?;
    let header_str = &rest[..newline_pos];
    let svg = &rest[newline_pos + 1..];
    let (index_str, total_str) = header_str.split_once(':')?;
    let index: usize = index_str.parse().ok()?;
    let total: usize = total_str.parse().ok()?;
    Some((PageHeader { index, total }, svg))
}

/// Ensure `svg_bytes` carries glyph defs so `<use>` references resolve.
///
/// The first frame for a page carries full defs, which we cache; later frames
/// have them stripped by the server, so we inject the cached copy back in.
fn resolve_glyph_defs(
    svg_bytes: Vec<u8>,
    page_index: usize,
    cache: &mut HashMap<usize, String>,
) -> Vec<u8> {
    let defs = {
        let svg_str = String::from_utf8_lossy(&svg_bytes);
        svg_str.find(GLYPH_DEFS_OPEN).and_then(|start| {
            let end = start + svg_str[start..].find(DEFS_CLOSE)? + DEFS_CLOSE.len();
            Some(svg_str[start..end].to_string())
        })
    };
    match defs {
        Some(defs) => {
            cache.insert(page_index, defs);
            svg_bytes
        }
        None => match cache.get(&page_index) {
            Some(defs) => inject_glyph_defs(&svg_bytes, defs),
            None => {
                log::warn!("typst_viewer: page {page_index} — no defs and no cache");
                svg_bytes
            }
        },
    }
}

/// Insert cached glyph defs into an SVG that had them stripped by the server.
/// Inserts right after the opening `<svg ...>` tag.
pub fn inject_glyph_defs(svg_bytes: &[u8], cached_defs: &str) -> Vec<u8> {
    let svg_str = String::from_utf8_lossy(svg_bytes);
    let s: &str = &svg_str;
    if let Some(close_bracket) = s.find('>') {
        let insert_pos = close_bracket + 1;
        let before = &s[..insert_pos];
        let after = &s[insert_pos..];
        let mut result = Vec::with_capacity(svg_bytes.len() + cached_defs.len());
        result.extend_from_slice(before.as_bytes());
        result.extend_from_slice(cached_defs.as_bytes());
        result.extend_from_slice(after.as_bytes());
        result
    } else {
        svg_bytes.to_vec()
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
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
