# Typst preview: Zed-side implementation design

Status: **unimplemented.** Companion to [`design.md`](./design.md), which specifies
the **wire protocol** between tinymist and Zed and is already implemented on the
tinymist side. This document covers only the **Zed side**: how the crate is
structured to implement that protocol, what state lives where, and in what order to
build it. Implementing it will be the first end-to-end exercise of the protocol.

Section references like §5.3 point at `design.md`.

## 1. The split

Today `typst_viewer.rs` is ~1000 lines holding everything: LSP handshake,
WebSocket connect, frame coalescing, glyph-defs string surgery, resvg
rasterization, and the GPUI view. Streaming page images deletes a great deal of
that (§2.1) but adds a real memory-management problem, so the file still needs a
seam.

```
crates/typst_viewer/src/
  typst_viewer.rs    crate root, actions, TypstPreviewView: presentation only
  preview_session.rs LSP handshake, connection, protocol, page table, image cache
```

An earlier draft had a third file, `transport.rs`, owning a length-prefixed Unix
socket alongside the WebSocket. `design.md` §6 has since dropped the Unix socket:
there is one transport, WebSocket delivers discrete messages, and the framing
that justified a separate file no longer exists.

### 1.1 Where the line falls

The tempting split is *the session owns content, the view owns presentation, and
the session never touches a pixel.* **That line does not survive contact with this
protocol.** The wire delivers finished bitmaps at a resolution the session had to
ask for, so:

- **The session holds pixels**, because it holds the retention contract (§5.3) and
  those are the same object here. There is exactly **one** page cache, and it must
  live where `held` is computed.
- **Scale crosses the seam.** Zoom and DPI are presentation, decided by the view,
  but they are also protocol state the session must declare. §2.3 names this as
  the largest cost of this approach; on our side it shows up as the one value that
  flows view → session → wire.

So the honest statement of the split is now:

- **`preview_session.rs` owns *what* to show.** Connection, page table, image cache,
  retention, and the outbound subscription. Its output unit is *an
  `Arc<RenderImage>` for the content at index k*.
- **`typst_viewer.rs` owns *where and how big*.** List layout, scroll, zoom
  gestures, placeholder boxes, and which indices are visible. It performs no
  decoding and holds no page memory.

`preview_session.rs` is not an extractable library. It is an `Entity`, it uses
`Task`/`cx.spawn`, it reads `ProjectSettings`, it emits GPUI events, and it
produces `RenderImage`s — a GPUI type. It is "the non-UI half of a Zed feature".

## 2. The connection

One transport: the existing `ws://127.0.0.1:{port}` data plane, local and remote
alike (`design.md` §6). What changes with distance is the **encoding** — `raw`
locally, `png` when `project.is_local(cx)` is false — not the transport.

**Mode entry is a WebSocket subprotocol handshake** (§5.0), not a server flag. The
client offers

```
Sec-WebSocket-Protocol: tinymist-page-image-v1
```

on connect. A server that supports the mode echoes it; one that does not **fails
the upgrade**. That refusal is the capability signal, and it arrives before either
side sends a byte — so it must be handled as a first-class outcome, not a
transport error:

- **Distinguish it from a connection failure.** A refused upgrade means "this
  tinymist cannot do page images", which is a permanent condition for this server,
  not something a retry fixes. The 3-attempt retry in §3.7 must not apply to it.
- **Surface it.** The user needs to know their tinymist is too old, rather than
  watching an empty pane. This is the one error in the protocol that is actionable
  by the user.

Two things worth getting right at the outset, because both are hard to retrofit:

- **Bound the frame size.** A `raw` page is ~8 MB and the header is the only
  description of the buffer, so a corrupt or hostile length must not become a
  `Vec::with_capacity` of arbitrary size. Cap incoming messages at a ceiling
  derived from the largest plausible page (say 256 MB) — which is also the
  server's own raster budget — and drop the connection above it.
- **Reuse the read buffer.** At ~8 MB per image and a frame per keystroke, a fresh
  allocation per frame is real churn. Read into a reused `Vec` and hand out slices,
  or pool the buffers.

## 3. `preview_session.rs`

### 3.1 Shape

**Two keyings, deliberately.** The page table is keyed by **index** (a display
position); images are keyed by **content id** (§3). A pagebreak shift rewrites the
table and touches no image; a zoom touches neither, and only changes whether a
cached image is still at the current scale.

```rust
pub struct PreviewSession {
    project: Entity<Project>,
    source_buffer: Option<Entity<Buffer>>,
    entry_path: PathBuf,

    status: ConnectionStatus,
    table: PageTable,                          // index -> geometry + content id
    images: ImageCache,                        // content id -> decoded page

    viewport: Viewport,
    scale: f32,                                // pixels per point, from the view
    pending_view_send: Option<Task<()>>,       // debounce (§5.3, §5.6)
    outbox: mpsc::UnboundedSender<ClientMessage>,

    _connection_task: Task<()>,
    _lsp_subscriptions: Vec<lsp::Subscription>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContentId(u128);

#[derive(Default)]
pub struct PageTable {
    entries: Vec<PageTableEntry>,              // len() == total
}

pub struct PageTableEntry {
    pub size: Size<f32>,                       // intrinsic, in POINTS (§5.1)
    pub content: Option<ContentId>,
}

struct ImageCache {
    entries: HashMap<ContentId, CachedImage>,
    resident_bytes: usize,
}

struct CachedImage {
    image: Arc<RenderImage>,
    rendered_scale: f32,                       // from the image header (§5.2)
}
```

**Scale is metadata on the entry, not part of the key** (§3). The session holds at
most one rendering of a given content id — the most recent — and records what
scale it was rendered at, so a lookup can report whether it is current. Keying on
`(ContentId, scale)` would let the cache hold several scales of the same page,
which sounds like a feature and is really an unbounded one at ~8 MB apiece; the
connection only ever renders at one scale, so the extra entries could never be
served, only retained. The overwrite-on-arrival that id-only keying gives is also
what bounds this cache without an eviction rule of its own.

`rendered_scale` is a plain `f32` taken **from the image header**, never inferred.
It does not need to be a quantized newtype: it is no longer a hash key, and the
two comparisons it participates in want different things anyway — `image_at` wants
exact equality against the requested scale (both values originate from the same
`view` message, so this is a round-trip, not a float computation), while the
re-request decision in §3.6 wants a threshold, not equality.

Taking it from the header rather than assuming "whatever scale we currently want"
is what makes the in-flight case correct: an image requested before a zoom can
arrive after it, and assuming the current scale would both mislabel it `Current`
and hand the wrong value to `RenderImage::with_scale_factor`, drawing the page at
the wrong *size* rather than merely the wrong sharpness.

`PageTable` applies deltas rather than replacing itself (§5.1): `total` resizes
`entries`, truncating past the new end; per-entry `size` and `content` are each
optional and overwrite only what they carry.

`size` is in **points**, and keeping it that way matters — §5.1 makes geometry
scale-independent precisely so a zoom leaves the scroll region and every
placeholder untouched. Storing pixels here would silently reintroduce the layout
jump the table exists to prevent.

No `generation` counter. An image for a content id *is* that content at that
scale, so there is no such thing as a stale image for a given id and the
async-arrived-late hazard disappears rather than needing a version field.

### 3.2 Events out, method calls in

```rust
pub enum SessionEvent {
    StatusChanged,
    TableChanged(TableDelta),
    PagesChanged(SmallVec<[usize; 8]>),     // these indices now draw differently
}

pub struct TableDelta {
    pub total: usize,
    pub total_changed: bool,                    // -> ListState::splice  (§4.3)
    pub geometry_changed: SmallVec<[usize; 8]>, // -> remeasure_items    (§4.3)
    pub content_moved: bool,                    // -> re-anchor          (§4.3)
}
impl EventEmitter<SessionEvent> for PreviewSession {}
```

`TableDelta` is shaped around the three decisions the view must make, so the view
never re-derives a diff the session already computed.

```rust
impl PreviewSession {
    pub fn status(&self) -> &ConnectionStatus;
    pub fn table(&self) -> &PageTable;

    /// Declare what's on screen and at what resolution. Cheap and idempotent;
    /// call on every scroll and every zoom. Debouncing, throttling and wire
    /// traffic are the session's business (§5.6).
    pub fn set_viewport(&mut self, viewport: Viewport, scale: f32,
                        cx: &mut Context<Self>);

    /// The image to draw at `index`, and whether it is current. `Stale` means
    /// "correct content, wrong scale — draw it stretched" (§3.6).
    pub fn image_at(&self, index: usize) -> PageImage;
}

pub enum PageImage {
    Current(Arc<RenderImage>),
    Stale(Arc<RenderImage>),
    Failed,                                  // §5.8: will not render, don't wait
    Missing,                                 // draw the placeholder box
}
```

`image_at` is `&self` and does no work — decoding happens on arrival, not on
demand. Nothing is assembled on the render path, which is why it can be a plain
borrow rather than an `&mut` that mutates a cache while rendering.

`Failed` exists because the server **will not retry on its own**: an `error` frame
marks a content id as delivered, so nothing is re-sent until the id changes (§5.8).
Without a distinct variant a failed page is indistinguishable from one still in
flight, and the view shows a placeholder forever. It must render as a visible
error state, not as a placeholder.

**`Failed` is not terminal, and the cache must let it be overwritten.** Two things
un-stick it, and neither involves the client asking. An edit that changes the page
gives it a new id, which has no failure recorded against it. And a **scale change**
clears the server's sent-state (§5.6), so the same id is re-rendered at the new
scale — which is exactly how the one failure the server generates on purpose
recovers: a page too large to rasterize at 8× renders fine once the user zooms
back out. So an arriving `image` for an id currently marked `Failed` must replace
it like any other entry. Treating `Failed` as a permanent tombstone — skipping the
id on later frames, or short-circuiting it in `image_at` — would leave a
permanently broken page that the protocol had already fixed.

Making `Stale` a distinct variant rather than a bool keeps §5.6's transitional
state from being representable by accident: a caller must decide what to do about
it, and the view's decision (draw it, don't count it as satisfied) is not the
default of either bool value.

### 3.3 Connection: two tasks, not one

Today a single foreground task owns the socket and calls `now_or_never()` on it
to drain, which pins I/O to the main thread and entangles coalescing with
reading. Split it:

```
background task    owns the Transport. reads frames -> parses headers ->
                   DECODES AND CONVERTS (§3.6) -> sends ready images to a channel.
                   writes ClientMessage from `outbox`. Never touches App state.

foreground task    drains the channel in batches, applies to the PreviewSession
```

Putting decode/convert in the **background** task is the single most important
placement decision on this side. A ~2 Mpx page is 8 MB to convert and, over TCP,
a PNG decode of tens of milliseconds; on the foreground thread that is dropped
frames in every other Zed window. §1's resvg pass has the same requirement today;
it is more acute here only because the work is unavoidable per arriving image.

Foreground drain, where §4's coalescing rule lives:

```rust
while let Some(first) = inbox.next().await {
    let mut batch = vec![first];
    while let Ok(Some(next)) = inbox.try_next() { batch.push(next); }
    session.update(cx, |session, cx| session.apply_batch(batch, cx))?;
}
```

`apply_batch` implements the rule that makes coalescing safe. **Only one of the
two message kinds may be dropped**, and knowing which is the point:

1. Apply **every** `Pages` message, in order, unconditionally. These are
   **deltas, not snapshots** (§5.1) — the table is the accumulated result of all
   of them. Dropping one silently corrupts the table, and because a delta may be
   geometry-only or content-only, the corruption is not self-healing: a later
   message will not necessarily restate the lost field.
2. For `Image` frames, keep **only the last per content id** — in practice a
   no-op, since distinct ids are distinct content, but it costs nothing and
   covers the redundant-resend case §5.4 acknowledges.

The asymmetry is not arbitrary: images are **self-contained** (an image for `c`
fully determines what to draw), while table deltas are **accumulated state**.
Only self-contained messages are safe to drop. Worth an explicit comment, because
"coalesce the chatty channel" is exactly the instinct that would break rule 1.

Note what is *absent*: any channel carrying shared sub-resources, and with it the
§2.1 invisible-glyph bug. No message's loss can corrupt a later frame's rendering,
which is goal 4 of §4.

### 3.4 Codec

Frames are **binary**, where §1's are text. An `image` frame
is an ASCII header up to the first `\n` followed by raw bytes, so parsing works
over `&[u8]` and must not go through `str`:

```rust
enum ServerMessage {
    Pages { total: usize, full: bool, entries: Vec<TableEntryDelta> },
    Image { content: ContentId, px: Size<u32>, scale: f32,
            encoding: Encoding, bytes: Range<usize> },
    Error { content: ContentId, msg: String },   // §5.8
}

struct TableEntryDelta {
    index: usize,
    size: Option<Size<f32>>,     // points; absent on a content-only change
    content: Option<ContentId>,  // absent on a geometry-only change
}

enum ClientMessage {
    // `visible` is ordered by fraction-on-screen then index (§4.2), so it is a
    // Vec, not a Range: the order is the send priority the server honors (§5.3).
    View { visible: Vec<usize>, prefetch: Range<usize>, cached: Vec<usize>,
           scale: f32, encoding: Encoding, opaque: bool },
    WantPages(Vec<usize>),       // §5.7: page *indices*; defined, not sent initially
}
```

The `Option`s in `TableEntryDelta` are load-bearing: `None` means *unchanged*, not
*absent*, and collapsing them to defaults would clear geometry on every keystroke.

The server guarantees a `pages` message introducing a content id **precedes** any
`image` or `error` carrying it (§5.1), so an id absent from the table is a protocol
violation and the frame may be dropped rather than buffered.

`Image::bytes` is a range into the frame buffer rather than an owned `Vec`, so the
header parse doesn't copy 8 MB. **Validate `px_w × px_h × 4 == bytes.len()` for
`raw` before trusting either.** The header is the only description of the buffer's
shape, and a mismatch turns into an out-of-bounds read or a garbled image in the
conversion loop below.

`opaque` is `true` and `encoding` is fixed for the life of the connection (§5.3) —
both describe what this client *is*, so they are sent once and a later `view` that
changed them would be ignored server-side. Zed has no reason to set `opaque`
false; §3.5 depends on it being true.

`WantPages` carries **indices, not content ids** (§5.7). The client's problem is
"I need index 7 and don't have it"; a content id can also name content that no
longer exists after an edit, which is unanswerable.

`full: true` (on the first message after connect, including for a zero-page
document) replaces the table rather than merging. Merging from empty would also work, but honoring the flag makes a
reconnect with stale state safe by construction.

Parsing lives in free functions over `&[u8]` returning `Result`, so the protocol
is unit-testable without a socket, an LSP, or a GPUI app.

### 3.5 Decoding: from the wire to a `RenderImage`

Both encodings converge on GPUI's requirement: **BGRA8 with straight alpha**, in
an `image::ImageBuffer` inside a `RenderImage`. Because §3 guarantees every page
image is opaque, premultiplied and straight are the same bytes, so the conversion
is a **bare R↔B swap**:

```rust
for px in buffer.chunks_exact_mut(4) {
    px.swap(0, 2);
}
```

That is the whole of it, for both encodings — `raw` arrives premultiplied and
`png` straight, and with `a == 255` those are identical.

**Do not reach for `gpui::swap_rgba_pa_to_bgra` here.** It is the obvious-looking
helper and it is the wrong tool: it operates on one pixel at a time, and its guard
is `a > 0` rather than `a == 255`, so an opaque pixel still pays three float
divides by 1.0 — about 6M pointless divides on an A4 page at 2×. The opacity
guarantee exists partly to let this path skip it. Worth a comment at the call
site, since the function's name is an exact description of what we appear to want.

The corollary is that the opacity guarantee is now load-bearing for
**correctness**, not just speed: if a page ever did arrive with partial alpha, a
bare swap would render premultiplied values as straight and the page would look
subtly wrong rather than failing loudly. If §3's guarantee is ever relaxed, this
loop must change with it — link the two in a comment.

The result is `RenderImage::new(...).with_scale_factor(scale)`. Setting the scale
factor is what makes GPUI lay the image out at the right display size; §4.3's
placeholder geometry and this must agree or pages will jump as they load. Note
this is the *rendered* scale, which during a zoom transition is **not** the view's
current scale — that is exactly what makes a stale-scale image draw at the right
size (§3.6).

### 3.6 Retention and scale changes

The contract is declared in **indices** but the cache is keyed by **content id**,
so retention runs through the table:

```
held_indices = visible ∪ prefetch ∪ cached
held_content = { table[i].content : i ∈ held_indices }
```

- On viewport change: drop images whose id is no longer in `held_content`, then
  send `view` with the recomputed `cached`.
- Never drop an image reachable from `visible ∪ prefetch` — that is the promise,
  and honoring it is why Zed never needs `want-page` (§5.7).
- The map is **many-to-one**: distinct indices can share an id, so eviction must
  be by *absence from `held_content`*, never by per-index refcount. "Index left
  the window, drop its image" would evict an image another held index still
  displays. Note the sharing is narrower than "looks the same": `Frame` items
  carry source spans, so blank pages and content generated from one source node
  share an id, while boilerplate typed out twice does not (§5.4).

**`held` is the memory budget, and it is the only one.** At ~8 MB per page there
is no second tier to fall back on — nothing cheap to hold and re-rasterize from
locally, so eviction means a network round-trip.
Size `prefetch`/`cached` in **bytes**, from the table's point geometry times
`scale²`, not in page count — at 4× zoom a page is 16× the bytes and a count-based
budget is wrong by that factor.

The server also caps `held` at a fixed page count, keeping the visible-first order
and dropping the tail, so a byte budget that admits more pages than that cap will
not get them all served. In practice `cached` is where the excess lands and those
are pages the client already holds, so truncation costs nothing — but a very large
`visible` window would silently go unserved.

**Scale changes** (§5.6) are the expensive event, but with scale-independent ids
they need very little machinery on this side:

1. **Throttle, and ignore changes too small to see.** `set_viewport` records the
   new scale immediately but the wire send is debounced — with a *longer* window
   than scroll, since the work it triggers is a whole-document re-render. A
   pinch-zoom emits one `view`, not sixty. Same `pending_view_send` task, delay
   chosen by which field changed.

   Pair the debounce with a threshold, since a settled scale a hair from the last
   one is still a full re-render for no visible gain:

   ```rust
   if (new_scale - self.requested_scale).abs() > RERENDER_THRESHOLD { … }
   ```

   Compare against the last **requested** scale, not the last computed one —
   comparing against the last computed value lets a slow continuous zoom drift
   arbitrarily far while every individual step falls under the threshold.
2. **Serve the wrong scale on a miss.** `image_at` looks up `table[i].content`,
   finds an entry, and compares `rendered_scale` to the current scale —
   `Current` if equal, `Stale` otherwise. That is the entirety of §5.6's
   display-stale rule: no side table, no index-keyed cache, no eviction
   special-case, and no extra lookup path. A zoom simply makes every existing
   entry report `Stale` until its replacement overwrites it.

There is no third rule. Superseded scales need no eviction because they were never
retained separately — the arriving image overwrites the entry it replaces.

This is where the id choice pays off downstream. With scale folded into the id, a
zoom would remap every table entry, make every cached image unreachable, and have
the ordinary retention rule evict the whole cache before the first replacement
arrived — a full-document flash arising from the *interaction* between two
individually-correct mechanisms. Keeping scale out of the id means the interaction
never exists; keeping it out of the cache key means there is nothing extra to
bound.

### 3.7 LSP handshake

Moves from `typst_viewer.rs` largely unchanged: `find_tinymist_server`,
`register_tinymist_notifications`, `start_preview_via_lsp`, and the 3-attempt
retry. Changes while relocating:

- Drop `--server-svg --strip-svg-glyph-defs`. There is **no client-side flag** for
  the mode: it is selected by the subprotocol offered at connect (§2), and
  `doStartPreview` yields only `dataPlanePort`.
- **Send `doKillPreview` on teardown.** Today it is only sent defensively
  *before* starting, to clean up a previous leak. Register `cx.on_release` so
  closing a pane stops the tinymist-side render loop instead of leaving it
  compiling and rasterizing 8 MB pages into a dead socket. Server-side rendering
  makes this meaningfully more expensive to get wrong than it is today.

## 4. `typst_viewer.rs`

### 4.1 Shape

```rust
pub struct TypstPreviewView {
    session: Entity<PreviewSession>,
    focus_handle: FocusHandle,
    list_state: ListState,
    zoom: f32,
    _subscription: Subscription,
}
```

That is the whole of it. The view holds **no page memory and no decode state** —
it asks the session for an image per visible index at render time. Compare the
current `PreviewState::Rendering { pages: Vec<Option<Arc<RenderImage>>> }`, which
held every page bitmap in the view with no bound at all (`todo.md`'s "what about
huge documents? We don't want all page images in memory").

### 4.2 Use `gpui::list`, not a `div` column

The current render builds a `div` child per page under `overflow_y_scroll`. At
hundreds of pages that constructs hundreds of elements per frame — the
presentation-side half of the O(N) problem, and windowing the *protocol* doesn't
help if the *view* still builds every page.

`gpui::list` / `ListState` gives three things:

- **Virtualization.** Only items within the visible range plus `overdraw` are
  rendered and measured.
- **Visible range for free.** `ListState::set_scroll_handler` delivers
  `ListScrollEvent { visible_range }` — the on-screen indices, computed by the
  element that did the layout. The current code instead estimates them by dividing
  scroll offset by a guessed uniform page height, which is wrong the moment a
  document mixes page sizes — which typst supports and §3 calls out.
- **Index-anchored scroll.** `ListOffset { item_ix, offset_in_item }` is the
  anchor §5.5 asks for, given the invalidation discipline in §4.3.

Derive `overdraw` and the `prefetch` window from one another, so the view doesn't
ask for pages the list won't render or render pages it never subscribed to.

**Order `visible` by the fraction of each page on screen, ties by index (§5.3).**
`visible_range` is a bare `Range<usize>`, so used directly it puts the topmost index
first — but a viewport usually straddles a page boundary, so the topmost page is often a
sliver scrolled off the top while a lower page fills the screen. Sending the sliver first
makes the server refresh the page the user is *not* reading before the one they are.

The list does not hand out per-item visible fractions, but the view has everything to
compute them:

- `list_state.logical_scroll_top()` → `ListOffset { item_ix, offset_in_item }`: the
  first partly-visible index and how far into it the top of the viewport sits.
- the viewport height, from the list's bounds.
- each page's display height, `table[i].size.height × zoom`, which the view already
  derives for placeholders (§4.3).

Walk the visible indices from `item_ix`, accumulating display heights (less
`offset_in_item` for the first), and clip each page's `[y_start, y_end)` against
`[0, viewport_height)`. The clipped extent divided by the page's display height is the
**fraction on screen**. Sort `visible` by that fraction descending, breaking ties by
ascending index:

```rust
visible.sort_by(|a, b| {
    fraction[b].total_cmp(&fraction[a]).then(a.cmp(b))
});
```

Fraction rather than absolute pixels so a short page shown whole outranks a tall page
shown half — the fully-visible page is the one being read whatever its physical size —
and the index tie-break resolves two equally-visible pages top-to-bottom deterministically.
This is pure presentation arithmetic: the session takes an ordered `Vec<usize>`, not a
range, and treats the order as the send priority §5.3 relies on.

Note this only affects *latency and drop-order*, never correctness: at a normal
2–3-page viewport every visible page is served regardless of order, so the payoff is
that the page in front of the user is the first to refresh after a keystroke and the
last to be dropped at the 64-page cap.

### 4.3 Keeping `ListState` in sync

`ListState` is **not** told sizes; it **measures** them, caching results in a
`SumTree` as `ListItem::Measured { size }`. The module doc is explicit that those
are authoritative until invalidated:

> Clients of this API need to ensure that elements outside of the scrolled area
> do not change their height […] If your elements do change height, notify the
> list element via `ListState::splice` or `ListState::reset`.

So the page table never reaches `ListState` directly — it arrives through the
`render_item` closure, which sizes a placeholder box from `table[i].size × zoom`,
and the list measures that. An index is therefore **only re-measured when it is
actually rendered**: a page far off-screen keeps a stale height until scrolled
near. Inherent to virtualization; it never causes a jump at the viewport, but
total content height (and scrollbar thumb) is approximate until those pages come
into view.

**Three questions, three mechanisms.** Conflating them is the easy way to get
this wrong:

- *Which slots exist?* → `splice`
- *Which existing slots changed height?* → `remeasure_items`
- *Where should the viewport point afterwards?* → the §5.5 anchor

For `ListState` an index is purely a position, and the structural change is only
ever at the tail even when the content change starts anywhere. `splice` is a
general list-diff API built for lists where items really are inserted mid-list;
we never need that generality, because `TableDelta::total_changed` says only the
count moved:

```rust
if new_count > old_count {
    list_state.splice(old_count..old_count, new_count - old_count);   // append
} else if new_count < old_count {
    list_state.splice(new_count..old_count, 0);                       // truncate
}
list_state.remeasure_items(first_geometry_change..new_count);
```

Restricting `splice` to the tail sidesteps its one sharp edge:

```rust
if old_range.contains(item_ix) {
    *item_ix = old_range.start;       // <-- anchor collapses to range start
    *offset_in_item = px(0.);
} else if old_range.end <= *item_ix {
    *item_ix = *item_ix - (old_range.end - old_range.start) + spliced_count;
}
```

Against an append, `splice(200..200, 1)`, neither branch fires: the range is empty
so it contains nothing, and `old_range.end <= item_ix` needs `item_ix >= 200` when
only 0..=199 existed. `remeasure_items` likewise preserves the anchor deliberately
via `ScrollAnchor::Absolute`.

The failure mode to avoid is `splice(0..old_count, new_count)` — the tempting read
of a `TableChanged`. That range contains every index, so it always collapses the
anchor and **jumps to the top on every repagination**. It also discards every
cached measurement, and spliced items get `size_hint: None`, so content height
goes unknown and the scrollbar lurches. One residual collapse is benign:
truncating below the current position, where the page genuinely ceased to exist.

**The anchor, which the list cannot do for us.** `splice`/`remeasure_items` pin
the same *index*. §5.5 wants the same *content*, because editing above the
viewport shifts your page down without changing it. When `delta.content_moved`:

```rust
let anchor = list_state.logical_scroll_top();
let anchor_content = table[anchor.item_ix].content;
// ... apply splice / remeasure_items ...
if let Some(ix) = new_table.find_content(anchor_content, near = anchor.item_ix) {
    list_state.scroll_to(ListOffset { item_ix: ix, ..anchor });
}   // else: the index anchor the list already preserved IS the §5.5 fallback
```

The empty else-branch is the point: §5.5's two branches map onto "call
`scroll_to`" and "don't". `near` matters — §5.5 notes an id can appear at several
indices (blanks, boilerplate), so picking the first would teleport to the first
blank page on every edit.

**A scale change never reaches any of this**, because scale-independent ids (§3)
mean a zoom produces no `pages` delta at all. `TableChanged` fires on
repagination only; zoom is handled entirely by §4.4 and never reaches the anchor.
Had scale been folded into the id, a zoom would be indistinguishable from a
reflow here and would need a rule of its own.

Most edits need none of the three. Geometry comes from the table and a page's
rendered height only changes when its *point* size or the zoom changes, so:

| Situation | List notification |
| --- | --- |
| Ordinary edit, no repagination | none — swap the image |
| Page count changed | tail `splice` |
| Page point-sizes changed | `remeasure_items(first_changed..)` |
| Zoom changed | `remeasure_items(0..total)` — every box resizes |

**Gap worth knowing about:** `ListState` exposes per-item size hints only
uniformly (`with_uniform_item_height` / `reset_with_uniform_height`); there is no
public API to seed a different hint per index. We know every page's exact size and
cannot tell the list without rendering. For mixed-page-size documents, unmeasured
pages contribute wrong heights to the scroll region. Options, increasing cost:
accept it (bounded, self-correcting); seed a uniform hint from the modal page
size, exact for the overwhelmingly common single-size document; or add a
`splice_with_hints` to gpui. Start with the uniform hint.

### 4.4 Zoom

The view owns the gesture and the `zoom` factor; the session owns what to do about
it. On a zoom change the view:

1. Updates `zoom`, which immediately changes placeholder box sizes.
2. Calls `remeasure_items(0..total)` — every box resized.
3. Calls `session.set_viewport(viewport, window.scale_factor() * zoom, cx)`.

Everything else — throttling, serving the wrong scale on a miss, re-subscription — is the session's
(§3.6). The view's only obligation is to render `PageImage::Stale` the same way it
renders `Current`: at the size the table dictates, letting GPUI scale the texture.
A stale image is *correct content at the wrong resolution*, so it is drawn in the
right place at the right size and merely looks soft for a few hundred ms.

Do not render `Stale` differently (dimmed, spinner-overlaid). It is the steady
state during any zoom gesture, and drawing attention to it makes a smooth
interaction look broken.

### 4.5 Rendering a page

`render_item(i)` is nearly trivial, which is much of the payoff:

```rust
match session.read(cx).image_at(i) {
    PageImage::Current(image) | PageImage::Stale(image) =>
        img(ImageSource::Render(image)).w(w).h(h),
    PageImage::Failed => error_box(w, h),        // §5.8: nothing more is coming
    PageImage::Missing => placeholder_box(w, h),
}
```

where `w`/`h` come from `table[i].size * zoom`, **not** from the image's pixel
dimensions. Deriving display size from the image is what makes a page resize as it
loads; deriving it from the table means the placeholder and the loaded page occupy
identical space, which is the property that makes fling-scrolling a long document
feel like a PDF viewer.

There is no rasterization here, no background spawn, no cancellation, and no
priority ordering — all of which client-side rasterization would require. Decode
happens once on arrival in the transport task (§3.3).

## 5. What moves where

| Current location | Destination |
| --- | --- |
| `find_tinymist_server`, `register_tinymist_notifications`, `REGISTERED_SERVERS` | `preview_session.rs` |
| `start_preview_via_lsp`, `StartPreviewResponse` | `preview_session.rs` |
| `connect` | `preview_session.rs` (plus subprotocol negotiation, §2) |
| `connect_and_receive` / `try_connect_and_receive` retry | `preview_session.rs` |
| `receive_loop` drain | split: reader/decoder in the background task + `apply_batch` in `preview_session.rs` (§3.3) |
| `parse_page_header`, `parse_svg_message` | `preview_session.rs` codec, rewritten for binary |
| `resolve_glyph_defs`, `inject_glyph_defs`, `GLYPH_DEFS_OPEN`, `DEFS_CLOSE` | **deleted** (§2.1) |
| `SvgRenderer::render_single_frame` call | **deleted** — the server rasterizes now |
| visible-page estimation + `sort_by_key` | **deleted** — `ListScrollEvent::visible_range` |
| `PreviewState` | split: `ConnectionStatus` in `preview_session.rs`, nothing in the view |
| `TypstPreviewView`, `Render`, `Item`, `Focusable`, actions, `init` | stays |
| `layout_tests` | stays (pure presentation) |

The view loses all `anyhow`, `async-tungstenite`, `lsp`, `smol`, and `url` usage,
and the crate drops its dependency on GPUI's SVG renderer. Those are good proxies
for whether the seam is in the right place.

## 6. Testing

**Pure, no GPUI:**
- Codec over `&[u8]`: header parse, `px_w × px_h × 4 == len` validation, oversized
  frame rejection, truncated frame, and an `error` frame yielding `PageImage::Failed`
  for that id.
- `apply_batch` **non**-coalescing: `[Pages{geometry only}, Pages{content only}]`
  must leave both applied. A last-wins implementation passes every other test and
  fails this one; it is the guard on §3.3 rule 1.
- `PageTable` deltas: `total` shrink truncates; `None` preserves rather than
  clears; `full: true` replaces.
- The pagebreak-shift invariant: apply a delta remapping every index by +1 and
  assert **zero** images requested and **zero** evicted.
- Retention through the table, including the many-to-one case where a duplicate
  page keeps an image alive after one of its indices scrolls away.
- Pixel conversion: RGBA → BGRA channel swap on an opaque buffer, and a
  round-trip check that swapping twice is the identity.
- Scale staleness: with `c` cached at `1×` and current scale `2×`, `image_at`
  returns `Stale`, not `Missing`; once the `2×` image lands it returns `Current`,
  and the cache still holds exactly one entry for `c`.
- The in-flight case: set scale to `2×`, then deliver an image whose header says
  `1×`. It must be stored as `Stale` and keep `with_scale_factor(1.0)` — asserting
  the header is believed over the current request. This is the regression test for
  the wrong-*size* glitch, as distinct from the wrong-sharpness one above.
- `RERENDER_THRESHOLD`: a sequence of sub-threshold zoom steps that sums to well
  over it must eventually re-request, proving the comparison is against the last
  requested scale rather than the last computed one.

**GPUI, no network:** feed `ServerMessage`s into `apply_batch` (make it
`pub(crate)`) and assert view state — placeholder sizing, tail-splice vs.
remeasure selection, content-anchored scrolling including §5.5's duplicate-content
case, and the §3.6 scale-change sequence: assert that after a scale change and
before any new image arrives, every visible index still yields `Stale` rather than
`Missing`, and that no `TableChanged` was emitted at all. Those two are the
regression tests for the full-document flash and for zoom leaking into reflow
anchoring.

**Live:** a mock server for the handshake and reader task, including a server that
**refuses the subprotocol upgrade** — that path must surface as "this tinymist is
too old", not as a connection failure that triggers the §3.7 retry.

## 7. Phasing

**The old path is abandoned, so there is nothing to preserve.** Today's
`typst_viewer.rs` asks for `--server-svg --strip-svg-glyph-defs`, flags that exist
in no released tinymist — the patch was never landed and has been dropped. So the
current preview cannot run against any tinymist you can actually build, and an
earlier draft's opening step ("mechanical split, keeping today's protocol as-is,
no behavior change") was preserving something with no behavior to preserve.

The server side is likewise **not** phased any more: the tinymist implementation is
complete, so nothing here gates on it. That removes the incremental-shipping
constraint that shaped the old ordering, and the steps below are sequenced purely
by what makes each one debuggable.

1. **`gpui::list` migration.** Replace the `div` column and the guessed
   visible-page estimate. Do it first and alone: it is the one step with no
   protocol content, it is verifiable against the current UI, and every later step
   depends on an honest `visible` range. Doing it *with* the protocol work would
   mean debugging layout and wire format at the same time.
2. **Connect and the page table.** Subprotocol negotiation (§2), the binary codec,
   `PageTable` with delta application, size-accurate placeholders, and a scroll
   region from real geometry — but no images yet.

   This is the natural first milestone precisely because it renders *nothing*: a
   document that lays out with correctly-sized placeholder boxes proves
   negotiation, framing, header parsing, delta application, and ordering all work,
   with no decode path in the way. The server sends the table before any image and
   before the client sends a `view` (§5.0), so this is a complete, testable state
   rather than a half-built one.
3. **Images.** Decode/convert in the background task (§3.3), `RenderImage` from
   `raw`, and the deletion of the defs-stripping machinery and the resvg call — the
   single biggest deletion in the project. Needs a `view` to be sent, so it arrives
   with a minimal subscription: `visible` only, no prefetch, no retention.
4. **`view` subscription, retention, scale.** Windowing, the byte-based budget,
   content anchoring, and the §3.6 scale-change machinery. **Required, not
   optional**: without windowing, 8 MB images are worse than §1.
5. **`error` frames.** `PageImage::Failed` and a visible error state. Last because
   everything before it is correct-but-silent on a page that will not render, and
   because it is the only step that needs a deliberately pathological document to
   exercise.

**What to suspect first when something does not work.** Nothing has yet spoken this
protocol end to end — the server is unit-tested but has never served a frame, and
this is the first client. The three least-tested links, in order: subprotocol
negotiation through `hyper_tungstenite`; the server's connect-time render kick,
which is what makes the page table arrive without the client asking (and which had
an ordering bug once already); and header parsing from real wire bytes rather than
from a test constructor.

## 8. Open questions

- **Throttle window** (§5.6). `design.md` leaves the number open; on our side it
  is one constant in `set_viewport`, and it wants measuring against real
  compile+render times rather than guessing. Start ~250 ms and instrument.
- **What `scale` should actually be, and what `RERENDER_THRESHOLD` should be.**
  `window.scale_factor() * zoom` is the obvious value, but a page displayed
  smaller than natural size doesn't need full DPI, and re-rendering a 200-page
  document at 4× because one page was zoomed is a lot of server work. Whether to
  clamp the upper end, and how large the §3.6 threshold should be, together form
  the main lever on how expensive zoom feels. Both want measuring rather than
  guessing; the threshold in particular trades re-render frequency against how
  long the page stays visibly soft.
- **Memory accounting across previews.** The `held` budget is per-session, so N
  open previews are N budgets. At 8 MB per page that needs a process-wide ceiling
  — the same conclusion `todo.md` reached for the old bitmap cache, now more
  pressing because images are no longer regenerable without the network.
- **`image_cache` integration** (`todo.md`). Deferred: eviction here is driven by
  the retention contract, which a generic LRU doesn't know about. Revisit once
  §3.6 exists.
