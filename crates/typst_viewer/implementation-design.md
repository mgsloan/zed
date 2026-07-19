# Typst preview: Zed-side implementation design

Status: draft / proposal. Companion to [`design.md`](./design.md), which specifies
the **wire protocol** between tinymist and Zed. This document covers only the
**Zed side**: how the crate is structured to implement that protocol, what state
lives where, and in what order to build it.

Section references like §5.1 point at `design.md`.

## 1. The split

Today `typst_viewer.rs` is ~1000 lines holding everything: LSP handshake,
WebSocket connect, frame coalescing, glyph-defs string surgery, rasterization,
and the GPUI view. The protocol work in `design.md` roughly doubles the
communication logic (glyph registry, the incremental page table, content-addressed
bodies, view subscription, recovery pulls), so the file needs a seam before that
lands.

The seam proposed here:

```
crates/typst_viewer/src/
  typst_viewer.rs    crate root, actions, TypstPreviewView: presentation only
  client.rs          TypstClient: LSP handshake, socket, protocol, document state
  glyph_library.rs   process-global content-keyed glyph store (§5.3)
```

The dividing line is exactly the one `design.md` §6 draws for the protocol:

> The server owns content and intrinsic page sizes. The viewer owns presentation.

- **`client.rs` owns content.** Everything derived from the wire: connection
  status, the page table (geometry + content ids), content-keyed SVG bodies, the
  glyph working set, retention, and the outbound `view` subscription. Its output
  unit is *a self-contained SVG for the content at index k* — never a bitmap,
  never a pixel.
- **`typst_viewer.rs` owns presentation.** Zoom, DPI, page gap, scroll offset,
  placeholders, rasterization scale, bitmap memory, and which indices are
  visible. It consumes composed SVGs and produces `RenderImage`s.

`client.rs` is **not** an extractable library. It is an `Entity`, it takes
`&mut App`, it uses `Task`/`cx.spawn`, it reads `ProjectSettings`, it emits GPUI
events. It is "the non-UI half of a Zed feature", not "a typst preview SDK".

`glyph_library.rs` is separate from `client.rs` because its **lifetime is
different**: one library is shared by every open preview and outlives any single
connection (§5.3). Folding a process-global into a per-document entity's module
would misrepresent that. If it stays under ~150 lines it could reasonably be a
section of `client.rs` instead — a judgment call, not a load-bearing decision.

## 2. `glyph_library.rs`

A GPUI `Global`. Content-addressed, so entries are never stale (§5.1) and
eviction is purely a memory decision.

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct GlyphId(SharedString);      // the `gAAAA…` id, as it appears in markup

pub struct GlyphLibrary {
    symbols: lru::LruCache<GlyphId, SharedString>,   // id -> `<symbol …>…</symbol>`
    pinned: HashMap<GlyphId, usize>,                 // refcount, see below
    budget_bytes: usize,
    resident_bytes: usize,
}

impl Global for GlyphLibrary {}
```

API:

- `merge(&mut self, defs: &str)` — parse a `glyphs\n` payload's `<symbol>`s and
  insert. Idempotent by construction; an id already present is a no-op touch.
- `compose_defs(&mut self, ids: &BTreeSet<GlyphId>) -> Result<String, Vec<GlyphId>>`
  — returns the `<defs id="glyph">…</defs>` block, or `Err(missing_ids)` so the
  caller can issue `want-glyphs`.
- `pin(&mut self, ids) -> PinGuard` / `Drop for PinGuard` — the §5.1 "pin during
  compose" safeguard, generalized to "pin while any preview is displaying a page
  that uses these". Refcounted across previews, which is also the answer to
  §5.3's "closing one preview must not drop glyphs another still shows": pins are
  held by live page entries, not by connections.

**Capacity floor (§5.1).** `budget_bytes` must never drop below the union of all
previews' composed working sets or eviction thrashes. Two mechanisms, both
needed:

1. Pins are hard — `compose_defs` and any displayed page's ids are unevictable,
   so the floor is enforced *dynamically* regardless of the configured budget.
2. The configured budget is a soft target for reclaiming *unpinned* entries. Log
   (rate-limited) when pinned bytes exceed the budget; that is the signal the
   number is set too low, and it is the metric that answers §10's open sizing
   question with real data rather than a guess.

Start at 64 MB of unpinned outlines. Typical documents hold a few thousand
glyphs, so this should essentially never evict in practice — LRU here is an
idle/cross-document reclamation mechanism, exactly as §5.1 argues.

**Nothing ever clears the library** (§5.1). Content-addressing means an entry can
never be wrong, so there is no correctness reason to drop one; a per-document
clear would also drop glyphs another preview is showing. Eviction is exclusively
the LRU's memory-budget decision, and divergence from any server is reconciled by
`want-glyphs`. Reconnects and document reopens need no special handling here.

**Sharing the store must not become sharing the protocol state.** §5.1 names the
one thing that would reintroduce a need for resync: using this shared library to
dedup *transport* across connections — telling server B "another preview already
fetched glyph X, don't send it." That would couple the per-connection `sent` sets
to a process-global cache whose eviction they cannot see, which is exactly the
stateful-delta hazard of §3.2 in a new place. The library is a **memory**
optimization only; every connection's `sent` set stays its own, and a redundant
glyph announcement is the price. Worth a comment on the type, because
"we already have this glyph, why are we receiving it again" is a natural thing
for a future reader to try to optimize away.

## 3. `client.rs`

### 3.1 Shape

**Two keyings, deliberately.** The `pages` table is keyed by **index** (a display
position); page bodies are keyed by **content id** (the identity). Keeping these
distinct is the whole point of §6.3 — a pagebreak shift rewrites the table and
touches no body.

```rust
pub struct TypstClient {
    project: Entity<Project>,
    source_buffer: Option<Entity<Buffer>>,
    entry_path: PathBuf,

    status: ConnectionStatus,
    table: PageTable,                       // index -> geometry + content id
    bodies: HashMap<ContentId, PageBody>,   // content id -> SVG body

    viewport: Viewport,                     // last set by the view
    pending_view_send: Option<Task<()>>,    // debounce (§6.2)
    outbox: mpsc::UnboundedSender<ClientMessage>,

    _connection_task: Task<()>,
    _lsp_subscriptions: Vec<lsp::Subscription>,
}

pub enum ConnectionStatus {
    Connecting,
    Connected,
    Disconnected { reason: SharedString },
    Error { message: SharedString },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContentId(u128);                 // hash(frame, bleed, fill)

#[derive(Default)]
pub struct PageTable {
    entries: Vec<PageTableEntry>,           // len() == total
}

pub struct PageTableEntry {
    pub size: Size<f32>,                    // intrinsic, in typst pt
    pub content: Option<ContentId>,         // None until the table names one
}

struct PageBody {
    svg: SharedString,                      // defs-free `<svg>…</svg>`
    glyph_ids: BTreeSet<GlyphId>,           // parsed once on arrival
    _pins: PinGuard,
}
```

`PageTable` applies deltas rather than replacing itself (§6.1): `total` resizes
`entries`, truncating past the new end; per-entry `w`/`h` and `c` are each optional
and overwrite only what they carry.

No `generation` field. With bodies keyed by content id, a body for `c` *is* the
content — there is no such thing as a stale body for a given id, so the
async-rasterization-finished-late hazard disappears rather than needing a version
counter. The one exception is a `SvgOptions` change, where the server resends
bodies under unchanged ids (§6.3); the client overwrites the body and emits
`PagesChanged`, and the view must drop any bitmap it cached for that id. That is
the only path by which a body mutates, and it is worth a comment at the write site.

`Viewport` is what the view declares; the client turns it into the wire message:

```rust
pub struct Viewport { pub visible: Range<usize>, pub prefetch: Range<usize> }
```

Ranges rather than `Vec<usize>` — the view always computes contiguous windows, and
ranges make the retention arithmetic (`held`, entering/leaving) trivial. The wire
format stays whatever `design.md` §6.2 settles on; serializing a range to a list
is a codec detail.

### 3.2 Events out, method calls in

The view subscribes; the client never knows the view exists.

```rust
pub enum ClientEvent {
    StatusChanged,
    TableChanged(TableDelta),
    PagesChanged(SmallVec<[usize; 8]>),     // these indices now draw differently
}

pub struct TableDelta {
    pub total: usize,
    pub total_changed: bool,                // -> ListState::splice  (§4.3)
    pub geometry_changed: SmallVec<[usize; 8]>, // -> remeasure_items (§4.3)
    pub content_moved: bool,                // -> re-anchor          (§4.3)
}
impl EventEmitter<ClientEvent> for TypstClient {}
```

`TableDelta` is shaped around the three things the view must decide, so the view
never re-derives a diff the client already computed. The distinction matters: a
keystroke changes content ids without touching geometry, and §6.1 keeps those in
separate optional fields precisely so this stays cheap.

`PagesChanged` covers both ways an index can start drawing differently — the table
remapped it to another content id, or a body arrived for the id it already names.
The view does not need to care which.

Events carry **indices, not payloads**. The view pulls what it wants through
`compose_page`, at its own pace and priority order. This keeps a 1000-page table
update from allocating 1000 bodies into an event, and lets the view skip composing
pages that scrolled away before it got to them.

Inbound (view → client) is plain method calls on `Entity<TypstClient>`:

```rust
impl TypstClient {
    pub fn new(project, source_buffer, cx: &mut Context<Self>) -> Self;
    pub fn status(&self) -> &ConnectionStatus;
    pub fn table(&self) -> &PageTable;

    /// Declare what's on screen. Cheap and idempotent; call on every scroll.
    /// Debouncing and wire traffic are the client's business.
    pub fn set_viewport(&mut self, viewport: Viewport, cx: &mut Context<Self>);

    /// Self-contained SVG for the content currently at `index`, or None if the
    /// body hasn't arrived or its glyphs are missing. On a glyph miss this
    /// enqueues `want-glyphs` and returns None; the view keeps its placeholder
    /// and gets a `PagesChanged` when it resolves.
    pub fn compose_page(&mut self, index: usize, cx: &mut Context<Self>)
        -> Option<ComposedPage>;
}

pub struct ComposedPage { pub svg: Arc<[u8]>, pub content: ContentId }
```

`compose_page` takes an **index** because that is what the view has, and returns
the `ContentId` because that is what the view should cache the resulting bitmap
under (§4.5). This is the one place the two keyings meet, and the signature is
deliberately the translation point.

### 3.3 Connection: two tasks, not one

Today a single foreground task owns the socket and calls `now_or_never()` on it
to drain. That works but pins socket I/O to the main thread and entangles
coalescing with reading. Split it:

```
background task            owns the split WebSocket.
                           reads frames -> parses to ServerMessage -> unbounded channel
                           writes ClientMessage from `outbox`
                           never touches App state

foreground task            drains the channel in batches, applies to TypstClient
```

Foreground drain, which is where §8's coalescing rule lives:

```rust
while let Some(first) = inbox.next().await {
    let mut batch = vec![first];
    while let Ok(Some(next)) = inbox.try_next() {
        batch.push(next);
    }
    client.update(cx, |client, cx| client.apply_batch(batch, cx))?;
}
```

`apply_batch` implements the rule that makes coalescing safe. **Exactly one of the
three message kinds may be dropped**, and knowing which is the entire point:

1. Apply **every** `Glyphs` message, in order, unconditionally. Additive and
   idempotent; dropping one is the §3.2 invisible-glyph bug.
2. Apply **every** `Pages` message, in order, unconditionally. These are
   **deltas, not snapshots** (§6.1) — each carries only the fields that changed,
   so the table is the accumulated result of all of them. Dropping one silently
   corrupts the table, and because a delta may be geometry-only or content-only,
   the corruption is not self-healing: a subsequent message will not necessarily
   restate the field that was lost.
3. For `Page` bodies, keep **only the last per content id**, then apply. Log the
   drops.

Step 2 is a change from an earlier draft, which treated the layout channel as a
full snapshot where last-wins was safe. Under §6.1's incremental table it is not,
and the two look identical at the call site — a `Pages` message dropped from a
batch produces a table that is merely *wrong*, with no error anywhere. Worth an
explicit comment in the code, since "coalesce the chatty channel" is exactly the
instinct that would break it.

The asymmetry is not arbitrary: bodies are **idempotent and self-contained** (a
body for `c` fully determines what to draw), while glyphs and table deltas are
**accumulated state**. Only self-contained messages are safe to drop. That is the
same principle §8 uses to justify coalescing at all, applied one level finer than
before.

### 3.4 Codec

Parsing borrows from the frame; ownership is taken only at insert.

```rust
enum ServerMessage {
    Glyphs { defs: SharedString },
    Page   { content: ContentId, svg: SharedString },
    Pages  { total: usize, full: bool, entries: Vec<TableEntryDelta> },
}

struct TableEntryDelta {
    index: usize,
    size: Option<Size<f32>>,        // absent on a content-only change
    content: Option<ContentId>,     // absent on a geometry-only change
}

enum ClientMessage {
    Current,
    View { visible: Range<usize>, prefetch: Range<usize>, cached: Vec<usize> },
    WantGlyphs(Vec<GlyphId>),
    WantPage(ContentId),   // §6.3: defined, not sent initially
}
```

The `Option`s in `TableEntryDelta` are load-bearing, not convenience: `None` means
*unchanged*, not *absent*, and collapsing them to defaults would clear geometry on
every keystroke. Worth a type-level comment.

`full: true` (only on the first message after connect) tells the client to replace
the table rather than merge into it. Merging would also work from an empty table,
but honoring the flag makes a reconnect-with-stale-state safe by construction.

`WantPage` is defined and never constructed by Zed at first, matching §6.3's
"planned server-side, deferred on the Zed side". Keying it by `ContentId` rather
than index follows the body channel — the client wants specific *content*, and
the index it wanted it for may have moved by the time the reply lands.

Parsing lives in free functions over `&str` returning `Result`, so the protocol
is unit-testable without a socket, an LSP, or a GPUI app. This is the main
testability win of the split (§6).

### 3.5 Retention and the `cached` set

The client's body store is the authority on what Zed holds (§6.2). The contract is
declared in **indices** but the store is keyed by **content id**, so retention
runs through the table:

```
held_indices  = visible ∪ prefetch ∪ cached
held_content  = { table[i].content : i ∈ held_indices }
```

- On viewport change: drop bodies whose id is no longer in `held_content`
  (releasing their `PinGuard`s, and with them the glyphs), then send `view` with
  the recomputed `cached`.
- The client **must not** drop a body reachable from `visible ∪ prefetch` — that
  is the promise the contract makes, and honoring it is why Zed never needs
  `want-page`.
- The map is **many-to-one**: duplicate pages (blanks, repeated boilerplate) share
  a content id, so eviction must be by *absence from `held_content`*, not by
  per-index refcount decrement. A naive "index left the window, drop its body"
  would evict a body another held index still displays.

The indirection is also where §6.3's acknowledged raciness lands on our side: the
server maps our declared indices through *its* current table, we map through
*ours*, and during a repagination those can briefly differ. `design.md` argues
this degrades to a redundant idempotent resend. On the Zed side the corresponding
requirement is that an unsolicited body for an id we do not currently hold must be
**accepted and stored**, not treated as a protocol error — it is about to become
held. Rejecting it is the one local decision that could turn the benign race into
a missing page.

Debounce (§6.2): `set_viewport` updates state immediately (so `compose_page` is
correct right away) but schedules the wire send ~100 ms out, replacing any
pending send. A fling therefore produces one `view` message, not fifty.

SVG bodies are the cheap tier — text, maybe a few hundred KB per page — so the
client can afford a `cached` window meaningfully larger than the view's bitmap
window. That asymmetry is the point of having two tiers (§4.5).

### 3.6 LSP handshake

Moves from `typst_viewer.rs` essentially unchanged: `find_tinymist_server`,
`register_tinymist_notifications`, `start_preview_via_lsp`, `connect`, and the
3-attempt retry. Two changes worth making while relocating:

- Drop `--strip-svg-glyph-defs` from the argument list once Layer A lands; the
  glyph registry supersedes it and the two must not both be active.
- **Send `doKillPreview` on teardown.** Today it is only sent defensively *before*
  starting, to clean up a previous leak. `TypstClient` should register
  `cx.on_release` (or the view should, via `observe_release`) to kill its own
  preview task when the entity drops, so closing a pane stops the tinymist-side
  render loop instead of leaving it compiling into a dead socket.

## 4. `typst_viewer.rs`

### 4.1 Shape

```rust
pub struct TypstPreviewView {
    client: Entity<TypstClient>,
    focus_handle: FocusHandle,
    list_state: ListState,
    bitmaps: BitmapCache,                       // ContentId -> RenderImage
    zoom: f32,
    rasterizing: HashMap<ContentId, Task<()>>,
    _subscription: Subscription,
}

struct BitmapCache {
    entries: lru::LruCache<(ContentId, ScaleKey), Arc<RenderImage>>,
    budget_bytes: usize,
}
```

**The bitmap cache is keyed by `ContentId`, not by index.** This is the single
most consequential thing the view inherits from §6.3: when a pagebreak shifts 200
pages down by one, every content id is unchanged, so *every bitmap is still valid
and simply displays at a new index*. Keyed by index, all 200 would be invalidated
and re-rasterized — the expensive half of §3.4, reintroduced in the view after the
protocol went to real trouble to avoid it.

Rendering index `i` is therefore a double lookup: `table[i].content`, then
`bitmaps[(content, scale)]`. A miss draws the placeholder and schedules work.

`ScaleKey` is the quantized rasterization scale (`window.scale_factor() * zoom`),
so a zoom change doesn't collide with the existing entry. `rasterizing` is keyed
by content id too — two indices showing the same content must not race to
rasterize it twice.

### 4.2 Use `gpui::list`, not a `div` column

The current render builds a `div` child per page and relies on
`overflow_y_scroll`. At hundreds of pages that constructs hundreds of elements
per frame, which is the presentation-side half of §3.4's O(N) problem — windowing
the *protocol* doesn't help if the *view* still builds every page.

`gpui::list` / `ListState` solves three problems at once:

- **Virtualization.** Only items within the visible range plus `overdraw` are
  rendered and measured.
- **Visible range for free.** `ListState::set_scroll_handler` delivers
  `ListScrollEvent { visible_range }` — exactly the `visible` set §6.2 wants,
  computed by the element that actually did the layout rather than re-derived
  from a scroll offset. (The current code estimates the visible page by dividing
  scroll offset by a guessed uniform page height — wrong the moment a document
  mixes page sizes, which typst supports and `design.md` §2 calls out.)
- **Index-anchored scroll (§6.4).** `ListOffset { item_ix, offset_in_item }` *is*
  the index anchor the design asks for, so reflow anchoring needs no prefix-sum
  arithmetic in this crate — provided invalidation is done as §4.3 describes.

The `overdraw` parameter and the `prefetch` window should be derived from one
another so the view doesn't ask the client for pages the list won't render, or
render pages it never subscribed to.

### 4.3 How page sizes reach the list

`ListState` is **not** told sizes; it **measures** them. Heights are discovered by
rendering an item and measuring the result, then cached in a `SumTree` as
`ListItem::Measured { size }`. The module doc is explicit that cached
measurements are authoritative until invalidated:

> Clients of this API need to ensure that elements outside of the scrolled area
> do not change their height […] If your elements do change height, notify the
> list element via `ListState::splice` or `ListState::reset`.

So the page table never reaches `ListState` directly. It reaches it through the
`render_item` closure, on a round trip:

```
client: TableChanged(delta)
  -> view reads the client's updated PageTable
  -> view invalidates the affected index range on ListState
  -> next paint: list re-renders items in visible+overdraw
  -> render_item builds a placeholder div sized from the NEW geometry
  -> list measures it, SumTree updated
```

The consequence: **an index is only re-measured when it is actually rendered.**
A page far outside the viewport keeps its stale height until you scroll near it.
That is inherent to virtualization and never causes a jump *at the viewport*, but
it does mean total content height — and therefore scrollbar thumb size — is
approximate for a document whose off-screen pages repaginated, converging as
those pages come into view.

**Three separate questions, three separate mechanisms.** Keeping them apart is
what makes this simple, and conflating them is the easy way to get it wrong:

- *Which slots exist?* → `splice`
- *Which existing slots changed height?* → `remeasure_items`
- *Where should the viewport point afterwards?* → the §6.4 anchor, below

`ListState` is index-keyed, and for **it** an index is purely a position: index 47
means "the 47th row", and the structural change is only ever at the tail even when
the content change starts anywhere. That is compatible with §2's "content is the
identity; the index is a display position" — the list mirrors *positions*, while
the bitmap cache (§4.1) keys on *identity*. Two keyings, two jobs.

`splice(old_range, count)` is a general list-diff API — built for lists like a
chat log where items really are inserted mid-list. We never need its generality,
because `TableDelta::total_changed` tells us only the count moved:

```rust
// count changed — pure tail edit
if new_count > old_count {
    list_state.splice(old_count..old_count, new_count - old_count);   // append
} else if new_count < old_count {
    list_state.splice(new_count..old_count, 0);                       // truncate
}

// geometry of surviving pages changed — from delta.geometry_changed
list_state.remeasure_items(first_changed_index..new_count);
```

Restricting `splice` to the tail sidesteps its one sharp edge. The anchor fixup is:

```rust
if old_range.contains(item_ix) {
    *item_ix = old_range.start;       // <-- anchor collapses to range start
    *offset_in_item = px(0.);
} else if old_range.end <= *item_ix {
    *item_ix = *item_ix - (old_range.end - old_range.start) + spliced_count;
}
```

Against an append, `splice(200..200, 1)`, neither branch can fire: the range is
empty so it contains nothing, and `old_range.end <= item_ix` needs
`item_ix >= 200` when only 0..=199 existed. The anchor is untouched.
`remeasure_items` likewise preserves it deliberately, via `ScrollAnchor::Absolute`.
So **no manual re-pinning is needed**, and the naive-looking approach is correct
so long as the splice range is derived from the *count* delta and not from "which
pages changed".

The failure mode to avoid is `splice(0..old_count, new_count)` — "the whole
document changed", which is the tempting read of a `TableChanged`. That range
contains every index, so it always hits the
collapse branch and **jumps the viewport to the top on every repagination** —
precisely the failure §6.4 exists to prevent. It also discards every cached
measurement, and spliced items get `size_hint: None`, so total content height goes
unknown and the scrollbar thumb lurches until enough pages re-render.

One residual collapse case survives and is benign: truncating below the current
scroll position (you were viewing page 200 of 201 and the document is now 200
pages). That page genuinely ceased to exist, so clamping is the only sensible
behavior.

**The anchor, which the list cannot do for us.** `splice`/`remeasure_items` keep
the anchor pinned to the same *index*. §6.4 now wants it pinned to the same
*content*, because editing above the viewport shifts your page down without
changing what is on it — index anchoring would slide the view by however many
pages shifted. So when `delta.content_moved` is set:

```rust
// before applying the delta
let anchor = list_state.logical_scroll_top();
let anchor_content = table[anchor.item_ix].content;

// after applying splice / remeasure_items
if let Some(new_ix) = new_table.find_content(anchor_content, near = anchor.item_ix) {
    list_state.scroll_to(ListOffset { item_ix: new_ix, ..anchor });
}   // else: fall through — the index anchor the list already preserved is correct
```

Note the shape of the fallback. §6.4's two branches map onto "call `scroll_to`" and
"don't" — if the anchor's content is gone, the index-pinned position the list has
*already* maintained is exactly the desired fallback, so the else-branch is empty.
The `clamped to total - 1` in §6.4 is handled by `scroll_to`, which clamps
`item_ix` to the item count.

`near` matters: §6.4 notes a content id can appear at several indices (blank pages,
repeated boilerplate), so `find_content` must pick the occurrence nearest the
previous index rather than the first. A document with many blank pages would
otherwise teleport to the first blank on every edit.

This is a real re-pin, unlike the one an earlier draft proposed — that one existed
to undo damage from a badly-chosen splice range; this one moves the viewport
somewhere the list could not have known to put it.

Most edits need **none** of the three. Page height comes from table geometry
rendered as a fixed-size box, so if a page's size didn't change, its rendered
height didn't change and the list needs no notification — just re-rasterize and
swap the image. Since §6.1's deltas carry `w`/`h` only when geometry actually
changes, the common typing case touches `ListState` not at all:

| Situation | List notification |
| --- | --- |
| Ordinary edit, no repagination | none — re-rasterize only |
| Page count changed | tail `splice` |
| Surviving page sizes changed | `remeasure_items(first_changed..)` |

**Gap worth knowing about:** `ListState` exposes per-item size *hints* only
uniformly (`with_uniform_item_height` / `reset_with_uniform_height`); there is no
public API to seed a *different* hint per index. We know every page's exact size
from the table and cannot tell the list about it without rendering. For a
document with mixed page sizes, unmeasured pages therefore contribute a wrong
height to the scroll region. Options, in increasing cost: accept it (the error is
bounded and self-corrects); seed a uniform hint from the modal page size, which
is exact for the overwhelmingly common single-size document; or add a
`splice_with_hints`-style API to `gpui`. Start with the uniform hint.

### 4.4 Rasterization

On `PagesChanged` for an index within (or near) the visible range:

1. Look up `table[index].content`. If `bitmaps` already holds it at the current
   scale, **there is nothing to do** — this is the pagebreak-shift fast path, and
   it should be the first branch so a 200-page shift costs 200 hash lookups.
2. Otherwise `client.update(cx, |c, cx| c.compose_page(index, cx))` →
   `ComposedPage`, or `None` (body or glyphs pending) → leave the placeholder.
3. Spawn a background rasterization at `window.scale_factor() * zoom`, keyed in
   `rasterizing` by `ComposedPage::content` so scrolling away **cancels** it by
   drop, and so two indices showing identical content don't race. The current code
   can do neither, and will rasterize pages the user has already scrolled past.
4. On completion, insert under `(content, scale)`. No staleness check is needed —
   a body for a content id never changes meaning, so a late result is still
   correct, merely possibly unwanted. (The `SvgOptions` resend of §3.1 is the lone
   exception; it busts the cache entry explicitly at the write site.)

Cap concurrent rasterizations (start with 2). Unbounded fan-out on a
repagination that dirties every visible page would saturate the background
executor and starve everything else in Zed.

Order by distance from the visible range. This preserves the intent of the
existing "visible page first" sort (`todo.md` item 9) but on a sound basis: the
visible range comes from `ListScrollEvent`, not from an estimate that assumes
uniform page heights.

### 4.5 Bitmap memory

The tighter of the two tiers, and the one that actually threatens the process:
~8 MB per A4 page at 2× (§3.4). Bitmaps are evicted by LRU under a **byte**
budget, not a page count — zoom multiplies bytes quadratically, so a 4× zoom is
16× the memory per page and a count-based budget would be wrong by that factor.

Eviction is by content id, driven by the list's rendered range plus a margin
mapped through the table. Because the client still holds the SVG body for the
wider `held` window, scrolling back re-rasterizes locally with **no** network
round trip — the two-tier cache paying off. And because the key is content rather
than index, a pagebreak shift costs nothing at all: not a re-fetch, not even a
re-rasterization.

Placeholders draw at the exact size from the table, so nothing reflows when
content arrives (§6.2) — the property that makes fling-scrolling a long document
feel like a PDF viewer instead of a jumping mess.

Whether to route bitmaps through Zed's `image_cache` (`todo.md`) is left open:
the eviction policy here is driven by the list's visible range mapped through the
table, which a generic LRU doesn't know about. Revisit once that policy exists.

## 5. What moves where

| Current location | Destination |
| --- | --- |
| `find_tinymist_server`, `register_tinymist_notifications`, `REGISTERED_SERVERS` | `client.rs` |
| `start_preview_via_lsp`, `StartPreviewResponse`, `connect` | `client.rs` |
| `connect_and_receive` / `try_connect_and_receive` retry | `client.rs` |
| `receive_loop` drain | `client.rs`, split into background reader + `apply_batch` |
| `parse_page_header`, `parse_svg_message` | `client.rs` codec |
| `resolve_glyph_defs`, `inject_glyph_defs`, `GLYPH_DEFS_OPEN`, `DEFS_CLOSE` | **deleted** — superseded by `glyph_library.rs` |
| visible-page estimation + `sort_by_key` | **deleted** — replaced by `ListScrollEvent::visible_range` |
| `PreviewState` | split: `ConnectionStatus` in `client.rs`, `BitmapCache` in the view |
| `TypstPreviewView`, `Render`, `Item`, `Focusable`, actions, `init` | stays in `typst_viewer.rs` |
| `layout_tests` | stays (pure presentation) |

Net effect on the view: it loses all `anyhow`, `async-tungstenite`, `lsp`,
`smol`, and `url` usage. That is a good proxy for whether the seam is in the
right place.

## 6. Testing

The split's main payoff is that most of the hard logic becomes testable without a
window.

**Pure, no GPUI:**
- Codec round-trips; malformed headers; unknown message kinds ignored, not fatal.
- `apply_batch` coalescing: a batch of `[Glyphs(D), Page(c,v1), Page(c,v2)]`
  applies D **and** keeps only v2 — the regression test for §3.2, currently
  untestable because the drain is welded to a live socket.
- `apply_batch` **non**-coalescing: a batch of `[Pages{geometry only},
  Pages{content only}]` must leave both applied. A last-wins implementation passes
  every other test and fails this one; it is the guard on §3.3 step 2.
- `PageTable` delta application: `total` shrink truncates; `None` fields preserve
  rather than clear; `full: true` replaces.
- The pagebreak-shift invariant, which is the whole point of the content-id model:
  apply a delta that remaps every index by +1 and assert **zero** bodies were
  requested and **zero** content ids evicted.
- `GlyphLibrary::compose_defs` missing-id reporting; pinned entries survive
  eviction pressure.
- Retention arithmetic through the table: which *content ids* enter/leave
  `held_content` across viewport moves, including the many-to-one case where a
  duplicate page keeps a body alive after one of its indices scrolls away.

**GPUI, no network:** drive `TypstClient` by feeding `ServerMessage`s directly
into `apply_batch` (make it `pub(crate)`), then assert view state. Covers
placeholder sizing, tail-splice vs. remeasure selection, and content-anchored
scrolling across a repagination — including the §6.4 duplicate-content case, where
the anchor must land on the *nearest* occurrence rather than the first.

**Live:** keep one mock-WebSocket test for the handshake and the reader task; it
is the only part the above doesn't reach.

## 7. Phasing

Mapped onto `design.md` §7's tracks. Steps 0 and 1 are worth doing regardless of
which protocol tracks ship.

0. **Mechanical split.** Move the code above into `client.rs` behind the
   `Entity` + events interface, keeping today's protocol exactly as-is. No
   behavior change; reviewable as a pure refactor. Do this first — every later
   step is smaller against a split file.
1. **`gpui::list` migration.** Replaces the div column and the visible-page
   estimate. Independently valuable (fixes mixed-page-size visibility, adds
   virtualization) and unblocks the honest `visible` set that Track 2 needs.
2. **Track 1 Layer A** (§5.1) — `glyph_library.rs`, split `glyphs\n` / `page:\n`
   handling, `apply_batch`. Delete `resolve_glyph_defs`/`inject_glyph_defs` and
   the `--strip-svg-glyph-defs` flag. Fixes the invisible-glyph bug.
3. **Track 1 Layer B** (§5.2) — per-page glyph-id parsing, `want-glyphs`
   recovery, LRU with pinning.
4. **Track 2, the `pages` table** (§6.1) — `PageTable` with delta application,
   correctly-sized placeholders, scroll region from real geometry. Small and
   independently useful.
5. **Track 2, content-addressed bodies + subscription** (§6.2–6.4) — bodies and
   bitmaps keyed by `ContentId`, `Viewport`, debounced `view` sends,
   retention/eviction through the table, byte-based bitmap budget, and
   content-anchored reflow.

Steps 0–1 are Zed-only and can land before any tinymist change. Steps 2–5 each
require the corresponding server side, so they gate on the fork.

## 8. Open questions

- **`cached` budget.** §3.5 assumes SVG bodies are cheap enough to hold a wide
  window, but a dense page's defs-free body is still substantial. Measure on a
  real large document before picking a number.
- **Zoom.** Absent from `design.md` (it is pure presentation) but it changes
  rasterization scale and therefore bitmap memory quadratically, and it puts a
  second dimension in the bitmap key (§4.1). Whether to keep multiple scales per
  content id or evict on zoom change is unresolved; keeping them makes pinch-zoom
  smooth and costs memory.
- **`ContentId` width.** `design.md` §6.3 derives it from `hash(frame, bleed,
  fill)` but does not fix a width. It must be wide enough that a collision is not
  a concern, since a collision renders the *wrong page* with no detectable error —
  the same reasoning that makes glyph ids 128-bit. Adopt whatever tinymist emits,
  and store it as an opaque fixed-width value rather than parsing it to `u64`.
- **Body-mutation path** (§3.1). The `SvgOptions` resend is the only case where a
  body changes under a fixed content id, and it invalidates a view-side bitmap
  the client cannot see. Confirm the client→view signal for it is `PagesChanged`
  on every affected index, and that the view treats that as cache-busting rather
  than as a no-op when the content id compares equal.
- **`image_cache` integration** (§4.5) — deferred until the list-driven eviction
  policy is written and its shape is clear.
