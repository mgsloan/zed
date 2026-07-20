//! The non-UI half of the preview: connection, page table, image cache, and the
//! outbound subscription.
//!
//! Owns *what* to show. `typst_viewer.rs` owns *where and how big*.

use anyhow::{Context as _, Result, anyhow};
use async_tungstenite::tungstenite::client::IntoClientRequest as _;
use async_tungstenite::{WebSocketStream, tungstenite::Message};
use collections::HashSet;
use futures::channel::mpsc;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task, WeakEntity};
use language::Buffer;
use lsp::{LanguageServer, LanguageServerId, LanguageServerName};
use project::Project;
use serde::Deserialize;
use settings::Settings as _;
use smallvec::SmallVec;
use smol::net::TcpStream;
use std::collections::HashSet as StdHashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::decode::decode_page;
use crate::page_table::{ImageCache, PageImage, PageTable, TableDelta, lookup_for_index};
use crate::protocol::{
    ClientMessage, ContentId, Encoding, MAX_FRAME_BYTES, SUBPROTOCOL, ServerMessage, parse_frame,
};

pub const TINYMIST_SERVER_NAME: LanguageServerName = LanguageServerName::new_static("tinymist");

/// How long to wait after a viewport change before telling the server.
const VIEW_DEBOUNCE: Duration = Duration::from_millis(80);
/// Longer, because the work a scale change triggers is a whole-document
/// re-render rather than a few pages.
const SCALE_DEBOUNCE: Duration = Duration::from_millis(250);
/// A settled scale a hair from the last one is a full re-render for no visible
/// gain.
const RERENDER_THRESHOLD: f32 = 0.05;
/// The server caps the held set at 64 pages, so asking for more is pointless.
const MAX_HELD: usize = 64;

/// Track which LSP server IDs already have notification handlers registered, so
/// we don't panic on double-registration when opening multiple previews.
static REGISTERED_SERVERS: Mutex<Option<StdHashSet<LanguageServerId>>> = Mutex::new(None);

/// Response from tinymist's `doStartPreview`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartPreviewResponse {
    pub data_plane_port: Option<u16>,
}

#[derive(Debug)]
enum DocumentOutline {}

impl lsp::notification::Notification for DocumentOutline {
    type Params = serde_json::Value;
    const METHOD: &'static str = "tinymist/documentOutline";
}

/// tinymist sends outline notifications this client does not consume; register
/// a no-op handler to suppress "unhandled notification" log spam.
pub fn register_tinymist_notifications(server: &LanguageServer) -> Option<lsp::Subscription> {
    let server_id = server.server_id();
    let mut guard = REGISTERED_SERVERS.lock().unwrap_or_else(|e| e.into_inner());
    let set = guard.get_or_insert_with(StdHashSet::new);
    if !set.insert(server_id) {
        return None;
    }
    Some(server.on_notification::<DocumentOutline, _>(|_params, _cx| {}))
}

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
                    (status.name == TINYMIST_SERVER_NAME).then_some(server_id)
                })
                .collect();
            (servers.len() == 1).then(|| servers[0])
        })
}

/// A connection failure that a retry cannot fix.
///
/// A refused WebSocket upgrade means this tinymist does not speak the
/// page-image protocol. Retrying is pointless and hides the real problem, which
/// is the one error here that is actionable by the user.
#[derive(Debug)]
pub struct UnsupportedServer;

impl std::fmt::Display for UnsupportedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this tinymist does not support the page-image preview protocol \
             ({SUBPROTOCOL}); a newer tinymist is required"
        )
    }
}

impl std::error::Error for UnsupportedServer {}

#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    Connecting,
    Connected,
    Disconnected { reason: String },
    Error { message: String },
}

pub enum SessionEvent {
    StatusChanged,
    TableChanged(TableDelta),
    /// These indices now draw differently.
    PagesChanged(SmallVec<[usize; 8]>),
}

/// What the view has on screen, in indices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Viewport {
    pub visible: std::ops::Range<usize>,
    pub prefetch: std::ops::Range<usize>,
}

pub struct PreviewSession {
    status: ConnectionStatus,
    table: PageTable,
    images: ImageCache,

    viewport: Viewport,
    /// Pixels per point, from the view.
    scale: f32,
    /// The scale the server was last *asked* for. Compared against rather than
    /// the last computed scale, so a slow continuous zoom cannot drift
    /// arbitrarily far with every step under the threshold.
    requested_scale: f32,
    /// Indices we still hold beyond `prefetch`, so scrolling back needs no
    /// resend.
    cached: Vec<usize>,

    outbox: Option<mpsc::UnboundedSender<ClientMessage>>,
    pending_view_send: Option<Task<()>>,
    _connection_task: Task<()>,
    _lsp_subscriptions: Vec<lsp::Subscription>,
}

impl EventEmitter<SessionEvent> for PreviewSession {}

impl PreviewSession {
    pub fn new(
        project: Entity<Project>,
        source_buffer: Option<Entity<Buffer>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            status: ConnectionStatus::Connecting,
            table: PageTable::default(),
            images: ImageCache::default(),
            viewport: Viewport::default(),
            scale: 2.0,
            requested_scale: f32::NAN,
            cached: Vec::new(),
            outbox: None,
            pending_view_send: None,
            _connection_task: Task::ready(()),
            _lsp_subscriptions: Vec::new(),
        };
        this.start_connection(project, source_buffer, cx);
        this
    }

    pub fn status(&self) -> &ConnectionStatus {
        &self.status
    }

    pub fn table(&self) -> &PageTable {
        &self.table
    }

    pub fn resident_bytes(&self) -> usize {
        self.images.resident_bytes()
    }

    /// The image to draw at `index`, and whether it is current.
    ///
    /// `&self` and does no work: decoding happens on arrival, not on demand, so
    /// nothing is assembled on the render path.
    pub fn image_at(&self, index: usize) -> PageImage {
        lookup_for_index(&self.table, &self.images, index, self.scale)
    }

    /// Declare what is on screen and at what resolution.
    ///
    /// Cheap and idempotent; call on every scroll and every zoom. Debouncing and
    /// wire traffic are this type's business.
    pub fn set_viewport(&mut self, viewport: Viewport, scale: f32, cx: &mut Context<Self>) {
        let viewport_changed = viewport != self.viewport;
        let scale_changed = (scale - self.scale).abs() > f32::EPSILON;

        self.viewport = viewport;
        if scale_changed {
            // Record immediately so `image_at` starts reporting Stale and the
            // view redraws at the new size; only the *wire send* is debounced.
            self.scale = scale;
            cx.notify();
        }
        if !viewport_changed && !scale_changed {
            return;
        }

        let scale_send_due = (scale - self.requested_scale).abs() > RERENDER_THRESHOLD;
        let delay = if scale_send_due {
            SCALE_DEBOUNCE
        } else {
            VIEW_DEBOUNCE
        };

        self.pending_view_send = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, _cx| this.send_view()).ok();
        }));
    }

    fn held_indices(&self) -> Vec<usize> {
        let mut seen = HashSet::default();
        self.viewport
            .visible
            .clone()
            .chain(self.viewport.prefetch.clone())
            .chain(self.cached.iter().copied())
            .filter(|index| *index < self.table.len())
            .filter(|index| seen.insert(*index))
            .take(MAX_HELD)
            .collect()
    }

    fn send_view(&mut self) {
        let Some(outbox) = self.outbox.clone() else {
            return;
        };
        let visible: Vec<usize> = self
            .viewport
            .visible
            .clone()
            .filter(|i| *i < self.table.len())
            .collect();
        let prefetch: Vec<usize> = self
            .viewport
            .prefetch
            .clone()
            .filter(|i| *i < self.table.len() && !visible.contains(i))
            .collect();
        let cached: Vec<usize> = self
            .cached
            .iter()
            .copied()
            .filter(|i| *i < self.table.len() && !visible.contains(i) && !prefetch.contains(i))
            .collect();

        self.requested_scale = self.scale;
        let _ = outbox.unbounded_send(ClientMessage::View {
            visible,
            prefetch,
            cached,
            scale: self.scale,
            encoding: Encoding::Raw,
        });
    }

    /// Applies a coalesced batch of decoded frames.
    ///
    /// **Only one of the two message kinds may be dropped, and knowing which is
    /// the point.** `Pages` messages are *deltas, not snapshots*: the table is
    /// the accumulated result of all of them, so dropping one silently corrupts
    /// it, and because a delta may be geometry-only or content-only, the
    /// corruption does not self-heal. Images are *self-contained* — an image for
    /// `c` fully determines what to draw — so keeping only the last per id is
    /// safe. Only self-contained messages may be coalesced.
    pub fn apply_batch(&mut self, batch: Vec<Decoded>, cx: &mut Context<Self>) {
        let mut changed_pages: SmallVec<[usize; 8]> = SmallVec::new();

        for item in batch {
            match item {
                Decoded::Pages {
                    total,
                    full,
                    entries,
                } => {
                    let delta = self.table.apply(total, full, &entries);
                    cx.emit(SessionEvent::TableChanged(delta));
                }
                Decoded::Image {
                    content,
                    image,
                    rendered_scale,
                } => {
                    self.images.insert(content, image, rendered_scale);
                    self.note_indices_for(content, &mut changed_pages);
                }
                Decoded::Failed { content, msg } => {
                    log::warn!("typst_viewer: page {content} will not render: {msg}");
                    self.images.mark_failed(content);
                    self.note_indices_for(content, &mut changed_pages);
                }
            }
        }

        self.record_displayed();
        self.prune_cache();

        if !changed_pages.is_empty() {
            cx.emit(SessionEvent::PagesChanged(changed_pages));
        }
        cx.notify();
    }

    fn note_indices_for(&self, content: ContentId, out: &mut SmallVec<[usize; 8]>) {
        for index in 0..self.table.len() {
            if self.table.content_at(index) == Some(content) {
                out.push(index);
            }
        }
    }

    /// Notes which content each held index is now drawing, so a later keystroke
    /// has something to show while the replacement decodes.
    fn record_displayed(&mut self) {
        for index in self.held_indices() {
            if let Some(content) = self.table.content_at(index)
                && matches!(
                    self.images.lookup(Some(content), self.scale),
                    PageImage::Current(_) | PageImage::Stale(_)
                )
            {
                self.table.record_displayed(index, content);
            }
        }
    }

    /// Drops images no held index relies on.
    ///
    /// Retention is declared in **indices** but the cache is keyed by **content
    /// id**, so it runs through the table. It covers both the id an index
    /// *should* show and the one it is *currently* showing — evicting the latter
    /// is what made a keystroke flash white, since the fallback then had nothing
    /// to fall back to.
    fn prune_cache(&mut self) {
        let held: HashSet<ContentId> =
            self.table.ids_in_use(self.held_indices()).into_iter().collect();
        self.images.retain_held(&held);
    }

    fn set_status(&mut self, status: ConnectionStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.emit(SessionEvent::StatusChanged);
        cx.notify();
    }

    fn start_connection(
        &mut self,
        project: Entity<Project>,
        source_buffer: Option<Entity<Buffer>>,
        cx: &mut Context<Self>,
    ) {
        self._connection_task = cx.spawn(async move |this, cx| {
            match connect_with_retry(&project, &source_buffer, &this, cx).await {
                Ok(()) => {}
                Err(err) => {
                    log::error!("typst_viewer: connection failed: {err:#}");
                    this.update(cx, |this, cx| {
                        this.set_status(
                            ConnectionStatus::Error {
                                message: format!("{err:#}"),
                            },
                            cx,
                        );
                    })
                    .ok();
                }
            }
        });
    }
}

/// A frame after decoding, ready to apply on the foreground.
pub enum Decoded {
    Pages {
        total: usize,
        full: bool,
        entries: Vec<crate::protocol::TableEntryDelta>,
    },
    Image {
        content: ContentId,
        image: Arc<gpui::RenderImage>,
        rendered_scale: f32,
    },
    Failed {
        content: ContentId,
        msg: String,
    },
}

async fn connect_with_retry(
    project: &Entity<Project>,
    source_buffer: &Option<Entity<Buffer>>,
    this: &WeakEntity<PreviewSession>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    // tinymist may dispose the preview immediately after starting it (e.g. a
    // configuration change triggers a project reload), so a few attempts.
    let max_attempts = 3;
    for attempt in 1..=max_attempts {
        match connect_once(project, source_buffer, this, cx).await {
            Ok(()) => return Ok(()),
            // Not retryable: the server will not grow support between attempts,
            // and retrying would bury the one actionable error in this protocol.
            Err(err) if err.downcast_ref::<UnsupportedServer>().is_some() => return Err(err),
            Err(err) if attempt == max_attempts => return Err(err),
            Err(err) => {
                log::warn!("typst_viewer: attempt {attempt}/{max_attempts} failed: {err:#}");
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        }
    }
    unreachable!()
}

async fn connect_once(
    project: &Entity<Project>,
    source_buffer: &Option<Entity<Buffer>>,
    this: &WeakEntity<PreviewSession>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let (server, request_timeout, entry_path) = project.read_with(cx, |project, cx| {
        let buffer = source_buffer.as_ref().map(|b| b.read(cx));
        let server_id =
            find_tinymist_server(project, buffer, cx).context("tinymist language server not found")?;
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

    if let Some(subscription) = register_tinymist_notifications(&server) {
        this.update(cx, |this, _cx| this._lsp_subscriptions.push(subscription))
            .ok();
    }

    let port = start_preview(server.clone(), &entry_path, request_timeout).await?;
    let ws = connect_data_plane(port).await?;

    this.update(cx, |this, cx| {
        this.set_status(ConnectionStatus::Connected, cx);
    })
    .ok();

    run_connection(ws, this, cx).await
}

async fn start_preview(
    server: Arc<LanguageServer>,
    entry_path: &Path,
    request_timeout: Duration,
) -> Result<u16> {
    let entry = entry_path
        .to_str()
        .context("entry file path is not valid UTF-8")?;
    // The file path doubles as the task id, so each file gets its own preview
    // and can be killed independently.
    let task_id = entry.to_string();

    // tinymist allows one preview per task id and the previous one may not have
    // been cleaned up, so clear it first.
    let _ = server
        .request::<lsp::request::ExecuteCommand>(
            lsp::ExecuteCommandParams {
                command: "tinymist.doKillPreview".into(),
                arguments: vec![serde_json::json!(task_id)],
                ..Default::default()
            },
            request_timeout,
        )
        .await;

    // `--page-images` gates *availability* server-side; which protocol this
    // connection speaks is then chosen by the subprotocol offered at connect.
    // Without it the server refuses the upgrade, since the setting defaults off.
    let args: Vec<String> = vec![
        "--page-images=true".into(),
        "--data-plane-host=127.0.0.1:0".into(),
        format!("--task-id={task_id}"),
        entry.into(),
    ];

    let result = server
        .request::<lsp::request::ExecuteCommand>(
            lsp::ExecuteCommandParams {
                command: "tinymist.doStartPreview".into(),
                arguments: vec![serde_json::to_value(args)?],
                ..Default::default()
            },
            request_timeout,
        )
        .await
        .into_response()
        .context("tinymist.doStartPreview request failed")?;

    let response: StartPreviewResponse =
        serde_json::from_value(result.context("tinymist.doStartPreview returned null")?)
            .context("failed to parse StartPreviewResponse")?;

    response
        .data_plane_port
        .context("StartPreviewResponse missing dataPlanePort")
}

/// Connects and negotiates the page-image subprotocol.
async fn connect_data_plane(port: u16) -> Result<WebSocketStream<TcpStream>> {
    let addr = format!("127.0.0.1:{port}");
    let url = format!("ws://{addr}");
    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("TCP connect to {addr}"))?;

    let mut request = url
        .as_str()
        .into_client_request()
        .context("building WebSocket request")?;
    let headers = request.headers_mut();
    headers.insert("Origin", format!("http://{addr}").parse()?);
    // Mode entry. A server that supports it echoes this back; one that does not
    // fails the upgrade, which is the capability signal.
    headers.insert("Sec-WebSocket-Protocol", SUBPROTOCOL.parse()?);

    let (ws, response) = match async_tungstenite::client_async(request, tcp).await {
        Ok(pair) => pair,
        Err(async_tungstenite::tungstenite::Error::Http(response))
            if response.status().is_client_error() =>
        {
            return Err(anyhow!(UnsupportedServer));
        }
        Err(err) => return Err(err).context("WebSocket handshake failed"),
    };

    // A server may accept the upgrade while declining the subprotocol; per RFC
    // 6455 that means it will speak something else, which we cannot read.
    let agreed = response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|value| value.to_str().ok());
    if agreed != Some(SUBPROTOCOL) {
        return Err(anyhow!(UnsupportedServer));
    }

    log::info!("typst_viewer: connected to {addr} speaking {SUBPROTOCOL}");
    Ok(ws)
}

async fn run_connection(
    ws: WebSocketStream<TcpStream>,
    this: &WeakEntity<PreviewSession>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let (mut writer, mut reader) = ws.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded::<ClientMessage>();
    let (inbound_tx, mut inbound_rx) = mpsc::unbounded::<Decoded>();

    this.update(cx, |this, _cx| {
        this.outbox = Some(outbound_tx);
        // The server sends the page table on connect without being asked, but
        // announce the viewport so images start flowing.
        this.send_view();
    })
    .ok();

    // Writer: never touches App state.
    let write_task = cx.background_spawn(async move {
        while let Some(message) = outbound_rx.next().await {
            if writer.send(Message::text(message.encode())).await.is_err() {
                break;
            }
        }
    });

    // Reader + decoder: parses headers and does the 8 MB conversion off the
    // foreground thread.
    let read_task = cx.background_spawn(async move {
        while let Some(frame) = reader.next().await {
            let payload = match frame {
                Ok(Message::Binary(bytes)) => bytes,
                Ok(Message::Text(text)) => text.as_bytes().to_vec().into(),
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            if payload.len() > MAX_FRAME_BYTES {
                log::error!("typst_viewer: frame of {} bytes is too large", payload.len());
                break;
            }
            match decode_frame(&payload) {
                Ok(Some(decoded)) => {
                    if inbound_tx.unbounded_send(decoded).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(err) => log::error!("typst_viewer: bad frame: {err:#}"),
            }
        }
    });

    // Foreground: drain in batches so a burst becomes one update.
    while let Some(first) = inbound_rx.next().await {
        let mut batch = vec![first];
        while let Ok(next) = inbound_rx.try_recv() {
            batch.push(next);
        }
        this.update(cx, |this, cx| this.apply_batch(batch, cx))?;
    }

    drop(write_task);
    drop(read_task);

    this.update(cx, |this, cx| {
        this.set_status(
            ConnectionStatus::Disconnected {
                reason: "preview server closed the connection".into(),
            },
            cx,
        );
    })
    .ok();
    Ok(())
}

fn decode_frame(payload: &[u8]) -> Result<Option<Decoded>> {
    let Some(message) = parse_frame(payload)? else {
        return Ok(None);
    };
    Ok(Some(match message {
        ServerMessage::Pages {
            total,
            full,
            entries,
        } => Decoded::Pages {
            total,
            full,
            entries,
        },
        ServerMessage::Image {
            content,
            px_width,
            px_height,
            scale,
            encoding,
            payload: range,
        } => {
            let bytes = payload
                .get(range)
                .context("image payload range is out of bounds")?;
            let image = decode_page(bytes, px_width, px_height, encoding, scale)?;
            Decoded::Image {
                content,
                image,
                rendered_scale: scale,
            }
        }
        ServerMessage::Error { content, msg } => Decoded::Failed { content, msg },
    }))
}

/// Best-effort teardown so the server stops compiling and rasterizing into a
/// dead socket. Server-side rendering makes leaking a preview meaningfully more
/// expensive than it was.
pub fn kill_preview(
    project: &Entity<Project>,
    source_buffer: &Option<Entity<Buffer>>,
    cx: &mut App,
) {
    let Some(buffer) = source_buffer.clone() else {
        return;
    };
    let Some((server, timeout, path)) = project
        .read_with(cx, |project, cx| {
            let buffer_ref = buffer.read(cx);
            let server_id = find_tinymist_server(project, Some(buffer_ref), cx)?;
            let server = project
                .lsp_store()
                .read(cx)
                .language_server_for_id(server_id)?;
            let timeout = project::project_settings::ProjectSettings::get_global(cx)
                .global_lsp_settings
                .get_request_timeout();
            let path = buffer_ref
                .file()
                .and_then(|file| file.as_local())
                .map(|file| file.abs_path(cx))?;
            Some((server, timeout, path))
        })
    else {
        return;
    };

    let Some(task_id) = path.to_str().map(str::to_string) else {
        return;
    };
    cx.background_spawn(async move {
        let _ = server
            .request::<lsp::request::ExecuteCommand>(
                lsp::ExecuteCommandParams {
                    command: "tinymist.doKillPreview".into(),
                    arguments: vec![serde_json::json!(task_id)],
                    ..Default::default()
                },
                timeout,
            )
            .await;
    })
    .detach();
}
