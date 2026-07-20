# Typst preview: page image streaming protocol design

Status: **implemented server-side, unimplemented client-side.** tinymist speaks this
protocol on the `preview-page-images` branch; no client does yet, so nothing has
exercised it end to end. The earlier `--server-svg` patch this replaces was never
landed and has been abandoned.

The wire is still open to change — the only implementation is unreleased and behind a
flag — but a change now costs a server edit rather than a co-design conversation.

This document proposes streaming **rendered page images** from tinymist to Zed. Each
refresh carries a complete page image; there are no image diffs (§8).

It covers three largely independent axes:

- **Why page images** (§2): what streaming finished rasters buys and what it costs.
- **The protocol** (§5): a page table, page images addressed by content, and a
  viewport subscription that bounds work and memory.
- **Wire transport** (§6): the existing WebSocket data plane, with the encoding
  chosen by locality.

## 1. Current architecture (baseline)

- tinymist compiles the document and, in `--server-svg` mode, emits **one full
  standalone SVG per page** via `typst_svg::svg(page)`. **Every render sends all N
  pages** — no viewport windowing, no page-level dedup.
- Each page SVG embeds a `<defs id="glyph">` block with a `<symbol>` per glyph used,
  followed by `<use href="#g…">` placements. `--strip-svg-glyph-defs` hashes the
  whole defs block and strips it when unchanged.
- Zed caches the last-seen defs block per page, re-injects it into stripped frames by
  string splicing, then rasterizes the reassembled SVG with **resvg** (~82 ms per
  page, dominating the ~26 ms incremental compile).
- Zed coalesces bursts of frames, keeping the latest SVG per page, then rasterizes
  every page it received and holds every page bitmap, at 2× scale, with no bound —
  an A4 page is ~8 MB, so a few hundred pages is multiple GB resident.

Workable for the small documents it targets today; it falls over well before hundreds
of pages, on transport, rasterization and memory alike.

## 2. Why page images

### 2.1 What it deletes

A page image is **self-contained**: it references nothing delivered in another
message. That single property removes all of §1's shared-state machinery.

- **The defs-stripping dance** — the server hashing each page's glyph block to decide
  whether to strip it, the client caching the last block per page, and the string
  splicing that re-inserts it.
- **The bug that dance causes.** Stripping is a *stateful delta* — "the defs are
  byte-identical to the last full block I sent you" — which assumes the client
  observes every frame in order. Frame coalescing breaks that assumption: when a
  burst contains a full-defs frame introducing a new glyph followed by a stripped
  frame, only the stripped frame survives, the new glyph is never cached, and its
  `<use>` dangles and renders **invisible** from then on. Small documents trigger it
  readily — fast compiles mean more coalescing, and a small starting glyph set means
  nearly every keystroke introduces a glyph. With no cross-message state, this class
  of failure cannot occur (goal 4, §4).
- **Client-side rasterization.** The ~82 ms resvg pass per page becomes a
  pixel-format conversion (§5.2) and a texture upload.

Note that inlining each page's glyphs instead would fix the bug but not the cost —
it is the ~1.5 MB of defs on a dense page that motivated stripping to begin with.
Removing the shared state and removing the per-frame glyph payload are the same move
only because a raster carries neither.

### 2.2 What it buys

- **Fidelity.** Today we re-rasterize typst's vector output with a *different*
  engine than typst's own; antialiasing, hinting, gradients and clipping can differ.
  Server-side `typst-render` is by construction identical to what typst itself
  produces.
- **Glyph reuse for free, server-side.** `typst-render` already memoizes glyph
  rasterization globally via comemo:

  ```rust
  #[comemo::memoize]
  fn rasterize(font: &FontInstance, id: GlyphId, x: u32, y: u32, size: u32)
      -> Option<Arc<Bitmap>>
  ```

  The key is `(font, glyph, subpixel x/y, ppem)` and the value is an alpha `Bitmap`
  tinted afterwards — so colour changes don't invalidate it, and reuse spans pages,
  renders, and documents in that process. A glyph atlas we neither build nor
  maintain, living on the side of the wire that has the fonts.

### 2.3 What it costs

- **Resolution enters the protocol.** The server must know pixels-per-point to
  rasterize, so the client's scale becomes part of the subscription (§5.3) and a
  zoom or DPI change means a server round-trip and re-render (§5.6). This is the
  largest cost, and the one a resolution-independent format would not incur.
- **Payload grows with the square of scale.** An A4 page at 2× is ~2.0 Mpx ⇒ **8 MB**
  raw, against a few hundred KB for the equivalent vector description. Windowing
  (§5.3) stops being an optimization and becomes a requirement.
- **The server rasterizes**, on top of compiling — mitigated but not erased by the
  glyph memoization above.

## 3. Structural facts the design relies on

**Rendering.** `typst_render::render(page, &RenderOptions { pixel_per_pt,
render_bleed }) -> sk::Pixmap`. Output size is
`round(pixel_per_pt × (frame.size + bleed))`, clamped to ≥ 1 px.

**Native pixel format.** `sk::Pixmap` is **RGBA8, premultiplied alpha, tightly
packed** (no row padding: `data()` is exactly `w × h × 4`). This is what goes on the
wire (§5.2).

**Page images are opaque when the client asks for it.** `render` fills the background
from `page.fill_or_white()`, which resolves `Auto` to white, so a *typical* document is
fully opaque. There are two holes, not one:

- `#set page(fill: none)`, where `fill_or_white()` returns `None`, the fill is skipped
  entirely, and the canvas keeps `Pixmap::new`'s transparent zero-init.
- **A fill that is itself translucent** — `#set page(fill: rgb(255,0,0,50%))`, or a
  gradient or pattern with alpha. The fill happens, but leaves `a < 255`.

So full opacity is not a property of `render` at any setting; it is a **post-condition the
server establishes** by compositing onto opaque white. `view` therefore carries
`opaque: bool`, default `true`:

- **`opaque: true`** — the server guarantees `a == 255` on every delivered image. It skips
  the compositing pass when the page's fill is statically known to be an opaque solid,
  which is essentially every real page, so the guarantee is close to free.
- **`opaque: false`** — the server ships what `render` produced, alpha intact, and the
  client owes the un-premultiply.

The default is what the rest of this document assumes, and it is worth what it buys:
guaranteed `a == 255` makes the client's conversion a bare channel swap with no
un-premultiplication (§5.2) — no divide, no branch, no per-page variation in cost. The
alternative preserves a transparency that has nowhere to show through, since the viewer
draws pages on its own backdrop exactly as a PDF viewer does, so a transparent page
composites onto that backdrop and looks identical to a white one in every case except a
deliberately coloured viewer theme.

It is an option rather than a mandate because that argument is contingent on how the
viewer draws, not on anything about the format. A checkerboard or transparency mode, or a
client compositing onto something other than white, flips it — and one boolean is a much
cheaper way to hold that open than a wire format that has to be renegotiated (§8,
"Preserving page transparency"). `opaque` belongs to the same family as `scale`,
`encoding` (§5.3): it invalidates what has been *delivered* without
changing what any page *is*.

Export is unaffected — this concerns the preview path only.

**Pages are content-addressable; the index is only a position.** A page's render
inputs hash cheaply, giving an id that is *stable when content moves*. That matters
because content moving between indices is routine — insert a paragraph early and
every later page shifts down one.

**The id covers the page's content, not the rendering of it.** `render` reads
`frame`, `bleed` and `fill` from the page, plus the `RenderOptions`. The id covers
the first three only:

```
content id = hash(frame, bleed, fill)
```

- **Not `Frame` alone** — it excludes `fill` and `bleed`, so a `#set page(fill: …)`
  change would leave the id unchanged and show a stale page.
- **Not the whole `Page`** — it also covers `numbering`/`supplement`/`number`, none
  of which are drawn. `number` is the *logical* page number, which changes on exactly
  the shift we want to be free, so hashing `Page` would defeat content addressing.
  (If the number is actually printed, that text lives in the `frame`, so the id
  changes and the page is correctly re-sent.)
- **Not the `RenderOptions`.** `pixel_per_pt` is a per-connection setting (§5.3), not a
  property of the page. (`render_bleed` is always `false` here — see §5.3.)

**Why scale stays out of the id.** It is tempting to fold `pixel_per_pt` in on the
grounds that the id then names the exact bitmap on the wire. Keeping it out is
better on three counts:

1. **Zoom and reflow become orthogonal.** With scale in the id, a zoom changes every
   id in the document and is indistinguishable on the wire from a repagination that
   moved everything — so it produces a full `pages` remap, and the viewer's reflow
   anchoring (§5.5) has to special-case an event where nothing actually moved. With
   scale out, a zoom sends **no `pages` delta at all** and anchoring never sees it.
2. **Displaying a stale scale falls out for free.** A cache keyed by id still
   answers "do I have this page?" after a zoom — with the previous scale's pixels,
   which is exactly what §5.6 wants to draw while the replacement renders. With
   scale in the id, those entries become unreachable from the table the instant it
   remaps, so keeping them alive requires a separate, separately-bounded side cache.
3. **No ambiguity is introduced.** The concern would be a client unable to tell
   which scale an arriving image was rendered at — but the `image` header already
   carries `px_w`/`px_h` (§5.2), so every frame is self-describing.

The cost is that a content id no longer identifies bytes by itself: the same id names
different pixels at different scales. **Scale is therefore metadata on a cache entry,
not part of its key.** The *client* holds at most one rendering of a given id — the most
recent — and records which scale it was rendered at, so a lookup can report whether it
is current. A connection has exactly one scale at a time, so nothing needs to hold two
simultaneously; a newly-arrived image simply overwrites its predecessor, which is also
what bounds client memory without an eviction rule of its own.

**The server holds no rendered pixels at all.** It has the compiled document, so
re-rendering a page it needs is always available at the cost of one `render` call; there
is nothing a cache of finished rasters would buy it that is worth ~8 MB per entry per
connection. Its entire per-connection state is the `sent_content` set of §5.4 — a set of
128-bit ids, kilobytes at worst — which is unaffected by scale, and which a scale change
clears (§5.6).

## 4. Goals

1. Per-keystroke work proportional to what is on screen, not to document length.
2. Bounded client memory, at a scale where a single page is ~8 MB.
3. Content that merely *moves* is not re-sent.
4. A protocol with no shared sub-resource state, so frame coalescing can never
   desynchronise the two sides.
5. Keep the client light — no typst compiler, no vector renderer.

Non-goals for now: image diffs (each refresh is a full page image, §8); render bleed
(§5.3); encodings
beyond the two in §5.2; shared-memory transport; the HTML target; source-jump and
other interactive features.

## 5. The protocol

### 5.0 Mode entry — WebSocket subprotocol negotiation

tinymist's existing preview protocol, an incremental vector format, shares this data
plane, and both are reachable on one preview task. A connection must therefore establish
which protocol it speaks — and it must do so **before either side sends anything**, or the
page table cannot ride on connect (§5.1) and the server risks pushing page-image frames at
a client that only understands the vector format.

WebSocket already has the mechanism. The client requests the subprotocol at connect:

```
Sec-WebSocket-Protocol: tinymist-page-image-v1
```

The server echoes it if the mode is available, and otherwise **fails the upgrade**. A
client that requests no subprotocol gets exactly today's behaviour, so every shipped
client is unaffected by the mode existing.

Three properties fall out, none of which needed a design of its own:

- **Capability discovery is free and immediate.** A server that lacks the feature, or has
  it disabled, refuses the upgrade — the client learns at connect, rather than sending a
  subscription into silence indistinguishable from a slow compile.
- **The mode is known before the first byte**, so `pages` can be sent on connect and
  nothing needs bootstrapping. An earlier draft entered the mode on the first `view`
  message, which deadlocked: `view` names page *indices*, and indices only exist once
  `pages` has arrived.
- **It stays per-connection.** One preview task can serve a page-image client and a
  browser or native vector client simultaneously, which a task-wide flag would not allow.
  The server still needs a flag to gate *availability*; it does not need to decide per task
  which kind of client may attach.

The version is in the subprotocol name, so a future incompatible revision is
`tinymist-page-image-v2` and negotiation handles the mismatch with no in-band versioning.

### 5.1 `pages` — the page table (incremental)

One index-keyed table carrying the geometry needed to lay out and the content id
needed to know *what* to show:

```
pages\n{ total, full: true, pages: [ {i, w, h, c}, … ] }   # snapshot, on connect
pages\n{ total, pages: [ {i, c}, … ] }                     # delta, afterwards
```

- `w`/`h` are in **points**, not pixels — resolution-independent, so the client can
  size its scroll region and draw placeholders regardless of the current scale. Since
  ids are scale-independent too (§3), a zoom change touches **no part of this table**
  and sends no delta at all.
- **`total` is in every message.** Page count changes ride along; on a shrink the
  client drops entries past `total`.
- **Entries carry only changed fields**, keyed by `i`. A keystroke sends `{i, c}`
  with no geometry.
- **`full: true`** only on the first message after connect.

**`pages` precedes `image`.** The server MUST send the `pages` message introducing a
content id before any `image` carrying it. Both travel one ordered channel per connection,
so this costs nothing to guarantee and saves the client a buffer for images it cannot yet
place. A client that receives an id absent from its table MAY drop the image.

Geometry and mapping are combined deliberately. As whole snapshots they could not be:
geometry changes rarely but content ids change on nearly every keystroke, so a full
table per keystroke would be O(N). Incrementality is what makes one table viable, and
it keeps geometry and mapping consistent by construction.

### 5.2 `image` — a page image, keyed by content id

Sent once per distinct content, *not* per index:

```
image:{c}:{px_w}:{px_h}:{scale}:{encoding}\n<bytes>
```

- **`{c}`** is the content id (§3), written as **exactly 32 lowercase hex digits** —
  fixed width, so the header splits unambiguously and an id can never contain the `:`
  separator. **`px_w`/`px_h`** are decimal, and are the actual pixel
  dimensions, which the client needs for the buffer and which may differ from
  `w × scale` by the rounding in `render`.
- **`{scale}`** is the `pixel_per_pt` this image was actually rendered at. It is
  redundant in the steady state — it equals the `scale` the client last asked for
  in `view` — and is carried precisely for the case where it does *not*: images
  requested before a scale change are still in flight when the new scale takes
  effect. Without it the client must either assume the current scale, which
  mislabels those in-flight images and draws them at the wrong size, or recover it
  by dividing `px_w` by the page's point width, which the `round()` above makes
  approximate. One field removes the ambiguity, and it is what lets the client
  decide whether an image is current (§5.6) by comparison rather than inference.

  Because that comparison is an **equality test on a float**, the representation has to
  round-trip exactly: the server echoes the `f64` it parsed from the client's `view`,
  formatted as the shortest decimal that reads back to the same bits, and the client
  compares against the value it sent rather than against a re-parsed one. Note a client
  sending JSON `2` and one sending `2.0` produce the same `f64`, so only the formatting
  of the echo matters, not the client's spelling.
- **`{encoding}`** is one of exactly two values, chosen by the client (§5.3):

| encoding | bytes | intended for |
| --- | --- | --- |
| `raw` | `sk::Pixmap` verbatim — **RGBA8, premultiplied, tightly packed**, exactly `px_w × px_h × 4` | local |
| `png` | PNG, RGBA8 | remote (TCP) |

**`raw`** is tinymist's native buffer: beyond the compositing `opaque` may require (§3),
the server performs no conversion — it hands over what `typst_render::render` produced.
Client-side, GPUI wants **BGRA8 with straight alpha**, and under the default
`opaque: true` premultiplied and straight are the same bytes — so the conversion is a
**bare R↔B swap per pixel**, with no un-premultiplication anywhere. (Under
`opaque: false` the client owes a real un-premultiply; everything below about skipping it
applies only to the default.) Zed does one pass to build the `ImageBuffer` behind a
`RenderImage`. At ~1–3 ms per page this is bandwidth-bound and negligible beside the
~82 ms resvg pass it replaces (§1).

Note this makes GPUI's own `swap_rgba_pa_to_bgra` the wrong tool: it takes the divide
branch on every pixel and, because its guard is `a > 0` rather than `a == 255`, an
opaque pixel still pays three float divides by 1.0. The opacity guarantee is what lets
the client skip it entirely — and, conversely, `swap_rgba_pa_to_bgra` is exactly the right
tool if a client ever sets `opaque: false`.

**`png`** exists because raw images are not viable over a network (§6). PNG is
defined as **non-premultiplied**, which is a non-issue under the default for the same
reason: opaque pixels are identical either way, so the server encodes directly and the
client decodes to straight RGBA8 and again only swaps channels. It stays correct under
`opaque: false` for free, since a PNG encoder un-premultiplies as part of encoding — that
work is simply no longer a no-op. The cost is on the server:
PNG encode of a ~2 Mpx page runs to tens of milliseconds, the same order as the
rasterization it accompanies. That is a poor trade locally, which is exactly why the
encoding is selectable rather than fixed — and why a faster codec is the obvious later
improvement (§8).

Sending the renderer's native buffer for the local case keeps tinymist free of any one
client's texture conventions: BGRA-straight is a *GPUI* requirement, and putting it on
the wire would be the client-specific coupling the rest of this design avoids.

### 5.3 `view` — subscription, scale, and retention contract

The viewer maintains a standing subscription rather than issuing one-shot requests:

```
view\n{ visible: [...], prefetch: [...], cached?: [...], scale: 2.0,
        encoding: "raw", opaque?: true }
```

- `visible` — on-screen indices, served first.
- `prefetch` — a margin around `visible`, **biased toward the scroll direction**.
- `cached` (optional) — *additional* indices the client still holds beyond
  `prefetch`, so scrolling back needs no resend when content hasn't changed.
- `scale` — **pixels per point**, feeding `RenderOptions::pixel_per_pt`. Changing it
  (zoom, DPI, moving windows between monitors) invalidates every *delivered* image
  without changing any content id: the server clears its sent-state and re-renders the
  held pages at the new scale, and the `pages` table is untouched. This is the most
  expensive event in the protocol; §5.6 covers how both sides keep it from showing.
- `encoding` — `raw` or `png` (§5.2). The client picks by transport: `raw` over a
  loopback connection, `png` when the server is remote. It is the client's choice rather than derived from the
  transport server-side, so a client can opt for `png` locally if it would rather
  spend server CPU than memory, without the server second-guessing it.
- `opaque` (optional, default `true`) — whether the server composites each page onto
  opaque white before encoding (§3). Zed leaves it unset. A client that can actually
  *display* transparency — a checkerboard mode, or a backdrop that is not white — sets it
  false and takes on the un-premultiply.

`scale`, `encoding` and `opaque` all behave the same way, and none of them enter the
content id: each invalidates what has been **delivered** without changing what any page
**is**. Changing one clears the server's sent-state so held pages are re-sent, and none
produces a `pages` delta.

**`encoding` and `opaque` are fixed for the life of the connection** — set in the first
`view`, and ignored if a later one differs. Both are properties of what the client *is*
rather than what it is currently showing, and freezing them keeps them out of the `image`
header: a client can never be confused about which encoding or alpha convention an
in-flight image used. `scale` is the one that genuinely changes at runtime, which is why
it alone rides in the header (§5.2) and gets §5.6.

**`render_bleed` is always `false`.** typst's `RenderOptions` has it, but exposing it here
would break the geometry contract silently: `w`/`h` are the frame size *excluding* bleed
while `render` *includes* it, so a client would have to switch to deriving layout from
`px_w`/`px_h` ÷ `scale`. That is a real design question and gets its own treatment if bleed
is ever wanted in a preview; it is not a field to quietly add.

**A failed compile produces no traffic.** There is no `pages` delta and no `image`; the
client keeps displaying the last table and images it received, exactly as tinymist's
existing preview behaves. A document that does not compile is the normal state during
live editing, not an error condition, so it needs no representation on the wire.

**Each `view` wholly replaces the previous subscription** — it is not a delta. The server
may abandon in-flight work for indices no longer held.

The server caps `held` at **64 pages**, preserving the visible-first order and
dropping the tail. The number is part of the contract rather than an implementation
detail: a client sizing its window has to know whether the window will actually be
served, and "some cap exists" is not something it can size against. The contract is written by the client, and nothing else stops
a `cached: [0..999]` on a thousand-page document from rasterizing the whole thing on
every compile — at ~8 MB a page that is the difference between a bounded preview and
an out-of-memory abort. Truncation normally costs nothing, since the excess lands in
`cached`, which names pages the client already holds.

The subscription doubles as a **retention contract**: `held = visible ∪ prefetch ∪
cached` is exactly the set of pages the client promises to keep. That is what lets
the server know *what the client has* without guessing. The client may evict anything
**outside** `held`; the server tracks nothing outside `held`; so eviction and server
forgetting stay in lockstep at the same boundary.

- **Placeholders from the table:** an unfetched page draws as a correctly-sized box;
  scrolling never reflows and content pops in on arrival — standard PDF-viewer
  behaviour.
- **Memory budget = the `held` set.** Retention and prefetch are the same knob, and
  at ~8 MB per page at 2× it is the only thing bounding client memory.
- **Size `held` by bytes, not by page count.** Cost per page is `≈ w × h × scale² × 4`, so
  it grows with the *square* of scale: the same A4 page is ~2 MB at 1×, ~8 MB at 2×, ~32 MB
  at 4×. A fixed prefetch margin of *k* pages therefore means a memory budget that
  quadruples on a zoom-in — exactly when the user is looking at fewer pages at once and
  needs the margin least. Deriving `prefetch`/`cached` from a byte budget instead makes the
  margin shrink automatically as scale rises, which is both the correct memory behaviour
  and the correct prefetch behaviour. It also bounds the burst the server is asked to
  render on a zoom, since a scale change re-renders everything held (§5.6).
- **Debounce flings:** settle to the resting viewport before emitting a `view`
  update, so a fling doesn't churn pages in and out of `held`.

### 5.4 Sending each image once, wherever it lands

The server sends an image for content id `c` when some held index maps to `c` and it
has not already sent `c` to this client. It keeps a per-connection
`sent_content: HashSet<ContentId>`, scoped to the ids the held indices currently map
to, dropping an id once no held index maps to it.

Because ids are content-derived, **content that merely moves is not re-sent**. Insert
a paragraph near the top of a long document and every later page shifts down one:
with per-index versioning the whole document tail would be re-sent even though the
client already has every one of those images; with content ids only the mapping
moves. Blank pages, and content generated from a single source node, dedup for free.

Note the limit: `Frame` items carry source spans, so boilerplate *typed out twice*
produces two ids despite identical pixels. Dedup is over render inputs, not over
appearance.

| event | `pages` delta | images sent |
| --- | --- | --- |
| keystroke on one page | 1 entry (`c`) | 1 |
| **pagebreak shift** | remap entries (`c` only) | **0** |
| zoom / DPI change | **none** | all held pages |

Hashing the render inputs rather than the output also means tinymist can **skip
`typst_render::render` entirely** for content it has already sent — saving the
rasterization, not just the transfer.

During a repagination the two sides can briefly disagree about which ids are held
(the client declares indices; the server maps them through the current table). That
degrades to a redundant, idempotent resend — never a stale page.

### 5.5 Reflow anchoring

When the table changes under an edit, the viewer must not visually jump. Anchor on
**content, falling back to index**. Before applying a `pages` update, capture the
content id `c` of the page covering the top of the viewport, its index, and the
scroll offset within it. After applying:

1. **Content anchor (preferred).** If `c` still appears in the table, scroll so that
   index sits at the same position. The view follows the content across the
   repagination. If `c` appears at several indices (duplicate pages), pick the one
   nearest the previous index.
2. **Index fallback.** If `c` is gone, pin the previous *index* (clamped to
   `total - 1`), preserving the within-page offset.

The branches match what causes them: editing *above* the viewport shifts your page
without changing it, so content anchoring holds the view still; the anchor's id only
disappears when the page you are looking at changed or was deleted, where the page is
still meaningfully "page *k*" and the index is right.

### 5.6 Absorbing a scale change

A zoom change invalidates every image both sides hold, without changing a single
content id or table entry. Three rules keep that from being visible or wasteful.

**Throttle before re-rendering.** A pinch-zoom or a window drag across monitors emits
a continuous stream of scale values; each one, taken literally, is a full-document
re-render. The client must settle on a resting scale before putting it in a `view`
message — the same debounce `visible`/`prefetch` already get for flings, with a longer
window, since the work triggered is far larger. Only the settled value goes on the
wire; intermediate values are absorbed entirely by the next rule.

**Display images at the wrong scale until replacements arrive.** Because ids are
scale-independent (§3), an image the client already holds is still *reachable* after a
scale change — same id, same table entry, merely rendered at the old `pixel_per_pt`.
The client keeps drawing it at the new display size while the replacement renders. The
geometry is unchanged (`w`/`h` in points, §5.1), so it lands in exactly the right place
at the right size and is merely soft or oversharp for a few hundred milliseconds.

This is what makes zoom feel immediate despite a round-trip behind it, and it is a
consequence of the id choice rather than a mechanism added on top: the client keys its
cache on the id alone and stores each entry's rendered scale as reported in the image
header (§5.2), so a lookup finds the page and merely reports that it is not current.
No side table, no separate bound, no eviction special-case — the entries stay under
the ordinary retention rule (§5.3) because nothing about the table moved. It also
makes the throttle above free: during a pinch, every intermediate scale is served from
existing pixels.

Note the ordering hazard this avoids. An image requested at the old scale can arrive
*after* the client has moved to the new one; because the header states the scale it
was rendered at, such an image is correctly stored as not-current and drawn at its own
scale, rather than being mistaken for the replacement and drawn at the wrong size.

**Clear the sent-state on the server.** Everything the server has *delivered* is at the
old scale and must be delivered again, so `sent_content` (§5.4) is cleared when the scale
in `view` changes and the held pages re-render on the next pass.

There is no server-side raster cache to invalidate alongside it — see §3: the server
keeps no rendered pixels, only the document and the id set. That is what makes this rule
a one-line clear rather than an eviction problem, and it is why the id-only keying needs
no scale qualifier. The glyph-level memoization of §2.2 is separate and comemo-managed,
keyed partly by ppem; it is not cleared here and its old-ppem entries age out on their
own.

### 5.7 Recovery — `want-page`

```
want-page\n{ i: [7, 8] }
```

Asks the server to render and send those indices regardless of what it believes the client
already has: the safety valve if a client is forced to evict something it declared held.
**Planned on the tinymist side** so the protocol is robust for any client; **Zed does not
send it initially**, since it honors its declared `held` set.

**It addresses pages by index, not content id**, for three reasons. It is directly
satisfiable — the server maps index through the current table and renders, needing no
reverse id→index map that nothing else in the design wants. It matches what the client
knows: the client's problem is "I need index 7 and don't have it", while content ids are
the *server's* dedup key and making the client address by them leaks an internal concern.
And it cannot go stale — an id-addressed request can name content that no longer exists
after an edit, and is then unanswerable, whereas an index always resolves against the
current document and yields what the client actually wants: whatever is at index 7 *now*.

Out-of-range indices are ignored; the client is racing a shrink, which is normal.

### 5.8 `error` — a page that will not render

```
error\n{ c, msg }
```

Sent when rendering or encoding a page fails. Without it a client cannot distinguish
"still rendering" from "never coming", and shows a placeholder forever.

**There is no retry flag, because retry is implicit in content addressing.** An error is
about a content id, and that id is a function of the page's render inputs — so if the
document changes such that the page might now succeed, its id changes, and it arrives at
the server as fresh content that is rendered normally. Conversely, while the id is
unchanged, re-rendering would fail identically, and retrying would be pure waste.

So the server treats a delivered error exactly as it treats a delivered image: **the id
goes into `sent_content`** (§5.4). The send-once property covers errors for free, a
pathological page costs one failed render rather than one per keystroke, and no retry
queue or backoff exists on either side. A client that wants to force a retry anyway uses
`want-page`, which bypasses `sent_content` by construction.

The client marks the id as failed in its cache and draws a distinguishable error
placeholder — sized from the table, like any other unfetched page.

## 6. Transport

The protocol above is a sequence of framed messages and says nothing about how bytes
move. The data plane is `ws://127.0.0.1:{port}` — an artifact of tinymist's original
client, since typst.ts runs in a browser and a browser can only speak WebSocket — and
this design keeps it, local and remote alike.

A Unix domain socket was considered for the local case and **rejected** (§8): the
throughput argument does not survive scrutiny, and the access-control argument, which is
the real one, is better addressed directly.

What does change with distance is the **encoding**, not the transport. `raw` is right
locally and unusable remotely, so the client picks by locality (§5.3):

- **Local** — `raw`. 8 MB per image over loopback is two memcpys; compressing it would
  cost more than sending it.
- **Remote** (Zed's SSH remote development) — `png`. Raw images over a network link are
  not viable. This is what makes remote previews usable at all, and it is where the §2.3
  costs bite hardest: PNG encode lands on the server per page, and the payload still
  grows with the square of scale across a link that has none to spare.

Zed already knows whether a project is remote, so the choice needs no handshake.

**The browser client is unaffected.** This adds no transport and removes none; tinymist's
own preview keeps working exactly as it does. That keeps the upstream ask to the protocol
itself.

## 7. Phasing

1. **Subprotocol negotiation + server-side rendering + `image`/`error` frames.**
   Negotiate `tinymist-page-image-v1` at connect (§5.0), then render with
   `typst_render::render` and send native `Pixmap` bytes keyed by content id; Zed swaps
   channels and uploads. Deletes §1's defs-stripping machinery and the client rasterizer
   in one step. Negotiation comes first because it is what lets the mode exist at all
   without disturbing the vector clients — every later phase assumes it.
2. **The `pages` table.** Incremental geometry + content mapping; the client builds
   its scroll region and draws size-accurate placeholders.
3. **`view` subscription, retention, and scale.** Windowing, the `sent_content` set,
   content anchoring with index fallback. Turns per-keystroke cost from O(doc) into
   O(`held`) and bounds memory — required, not optional, at 8 MB per page.
4. **Remote support.** `png` encoding, selected by the client when the language server
   is remote.

Steps 1–3 are all needed before this is usable on a document of any size: without
windowing, full-page images at 8 MB apiece are worse than what §1 does today.

## 8. Alternatives considered

- **Streaming SVG with a content-keyed glyph registry.** Keep the format
  resolution-independent and fix §1's fragility directly: instead of stripping the
  defs block and re-injecting it, send each glyph once as an additive, idempotent
  message keyed by its content hash, and send defs-free page bodies that reference
  glyphs by id. Because those messages are never coalesced away and applying one
  twice is a no-op, the invisible-glyph failure of §2.1 cannot happen, and per-frame
  cost drops to the glyphs a keystroke actually introduces.

  Genuinely attractive on two axes this design pays for: zoom and DPI stay entirely
  client-side with no round-trip (§2.3's largest cost), and page bodies run a few
  hundred KB against 8 MB for a raster.

  Rejected on the weight of what it requires. A per-connection set of delivered ids
  on the server; a client-side glyph library with an LRU and a capacity floor to stop
  one page's working set from thrashing it; a back-channel to re-fetch anything
  evicted; and, to avoid duplicating outlines across previews, a process-wide shared
  library with reference-counted lifetime. That is a substantial subsystem on both
  sides of the wire, existing solely because the format embeds shared sub-resources.
  It also keeps the client rasterization cost of §1 and renders typst's output with a
  different engine than typst's own (§2.2). The trade is protocol complexity and
  client CPU against resolution coupling and payload size; §5.3's windowing and
  §5.6's zoom handling are the price of choosing the latter.
- **Image diffs (dirty rects or tile hashing).** Tile hashing is appealing — the same
  content-addressing idea one level down, deduping whitespace for free — and would
  cut the cost of localized edits. Deferred: reflow destroys the locality assumption
  (an early insert dirties most of a page), and diffing requires the server to keep
  the previous raster per page, per client, per scale (~8 MB each). **Each refresh is
  a full page image.**
- **Encodings beyond `raw` and `png`.** PNG was chosen for the remote path on
  availability, not merit: its encode cost on a ~2 Mpx page is tens of milliseconds,
  the same order as the rasterization it accompanies. **QOI** (roughly an order of
  magnitude faster to encode, still lossless) or **LZ4/zstd-1** over the raw buffer
  are better matched to this latency budget and are the obvious upgrade. A further
  4× is available before any compression by exploiting that a typical page is
  neutral black-on-white — an 8-bit coverage channel, with RGBA as the fallback for
  pages with colour. Deferred because two encodings already cover local and remote;
  adding a third is a one-line extension to §5.2's table. Note compression is a
  *loss* locally: LZ4 on 8 MB costs more than the two memcpys of sending it raw over
  loopback.
- **A Unix domain socket for the local data plane.** `AF_UNIX` is cross-platform
  (Windows since 10/1803) and Zed already wraps both families in `crates/net`. Two
  arguments were made for it, and neither holds up. The throughput one — no TCP/IP stack
  traversal, no checksums, no Nagle — is thin: mainstream stacks skip checksums on
  loopback, Nagle is one `TCP_NODELAY` away, and the shared-memory bullet below puts the
  entire cost of socket copies at ~2 ms, which caps what any transport swap can win. The
  access-control one is real — a localhost port is readable by any local process, a socket
  path gets filesystem permissions — but it is a property of the *existing* data plane,
  affecting tinymist's shipped preview just as much, so it belongs in a security fix for
  that, not as a rider on this design. Against those, a socket costs a second transport to
  maintain, explicit length-prefix framing (a byte stream has no message boundaries), and
  socket-path lifecycle and Windows path handling. Revisit if profiling ever shows the
  transport registering at all.
- **Shared memory (memfd/`SCM_RIGHTS`, `CreateFileMapping`+`DuplicateHandle`).**
  Rejected for now. It saves only the ~2 ms of socket copies, needs three separate
  platform implementations (Windows has AF_UNIX but no fd passing), and requires a
  buffer lifetime protocol — ring buffers with explicit release, or a fresh mapping
  per frame — whose failure mode is torn frames. The prize that would justify it,
  zero-copy all the way to the GPU, is blocked anyway: GPUI's `RenderImage` takes an
  owned `ImageBuffer`, so the mapping would be memcpy'd out regardless. Revisit only
  if profiling shows socket copies register **and** GPUI grows a borrowed-buffer
  upload path.
- **Transparency by default.** Rejected; §3 has the argument. The *capability* is
  retained as `opaque: false` in `view`, so a checkerboard or transparency mode is a
  client-side change rather than a protocol renegotiation.
- **Richer pixel-format negotiation.** §5.2 deliberately offers only two encodings
  and no *pixel format* choice within `raw` — it is always the renderer's native
  RGBA8 premultiplied, and the client converts. Letting a client request, say,
  BGRA-straight or a coverage-only channel is a natural extension if a second client
  with different texture conventions appears, but it buys nothing today and would put
  one client's conventions on the wire.
- **Multiplexing the data plane over the existing LSP connection.** Would need no new
  transport and would work under remote development for free. Rejected: JSON-RPC
  forces base64 for binary, and 8 MB frames would head-of-line block
  latency-sensitive LSP traffic on the same pipe.

## 9. Open questions

- **Scale change tuning** (§5.6): the shape is settled — throttle, display stale, clear
  `sent_content` — but the throttle window is not. Too short and a pinch
  queues re-renders faster than they complete; too long and the image stays soft after
  the gesture ends. Wants measuring against real compile+render times.
- **Remote cost** (§6): `png` makes remote workable, but its encode lands on the
  server per page at roughly the cost of the rasterization itself, and the payload
  still grows with scale². Whether that is acceptable, or whether a faster codec
  (§8) is needed before remote previews ship, wants measuring on a real link.
- ~~**Server memory**~~ — settled (§3): the server caches no rendered pixels, so there
  is nothing to bound and no shared-cache keying problem when several clients connect at
  different scales. Its per-connection state is the `sent_content` id set. What remains
  to measure is the *recompute* side of that trade: re-rendering a held page on a scale
  change costs one `render` per page. (Content-id hashing is *not* a concern — typst's
  `Frame` holds its items behind an `Arc<LazyHash<…>>`, so hashing one reads a cached
  128-bit value rather than walking page content, and the cache is shared across
  incremental recompiles.)

- **Locality detection** (§6): "is the project remote" is the obvious signal for choosing
  `raw` vs `png`, but a containerized language server could be local by that measure while
  behaving like a remote one for bandwidth.
