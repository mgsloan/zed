# Typst preview: SVG streaming protocol design

Status: draft / proposal. The tinymist `--server-svg` patch has **not landed**, so
both sides of the wire are open to change and can be co-designed.

This document proposes a more efficient, more robust, less hacky, and
large-document-scalable way to stream a rendered typst document from tinymist to a
lightweight SVG-rasterizing client (Zed). It supersedes the current "whole-block
glyph-defs stripping + send every page every frame" approach.

It is a **transport/protocol** design. How the client rasterizes the SVG it
receives (resvg today) is out of scope — no glyph atlas, display list, or other
renderer changes are proposed here.

It covers two largely independent axes:

- **Glyph transport efficiency** (§§2–5): the bytes per keystroke are dominated by
  glyph defs; how to send each glyph once.
- **Large-document scaling** (§6): how "send the whole document every frame" fails
  at hundreds of pages, and the index-based windowing that fixes it.

## 1. Current architecture (baseline)

- tinymist compiles the document and, in `--server-svg` mode, emits **one full
  standalone SVG per page** via `typst_svg::svg(page)`. **Every render sends all N
  pages** — there is no viewport windowing and no page-level dedup; the only thing
  skipped is a page's glyph-defs block when unchanged.
- Each page SVG embeds a `<defs id="glyph">` block containing a `<symbol>` for
  every glyph the page uses, followed by a body of `<use href="#g…">` placements.
- To save bandwidth, `--strip-svg-glyph-defs` hashes the **entire** defs block per
  page; if the hash equals the previous frame's, it strips the block entirely.
- Zed caches the last-seen defs block per page, re-injects it into stripped frames
  by string splicing, then rasterizes the reassembled SVG with resvg.
- Zed coalesces bursts of frames, keeping only the latest SVG per page, then
  rasterizes every page it received and holds every page bitmap.

## 2. Structural facts everything should exploit

**Glyphs are content-addressed.** typst-svg's `Deduplicator` emits symbol ids as
`g<128-bit-hash-hex>` (e.g. `g67E4375AFE95DE661F4ED15A21098E39`); the same id
appears across frames and across pages. Therefore:

1. **Add-only.** A glyph, once defined, never changes. Editing only ever *adds*
   glyphs to the working set.
2. **Globally shared.** The same glyph has the same id on every page. The current
   per-page defs blocks duplicate shared glyphs across pages.
3. **Content-keyed.** The id *is* the identity. Delivery can be idempotent and
   order-independent: "here is symbol `gXXXX`" applies any number of times, in any
   order, safely.
4. **Stable across recompiles.** The id is a hash of `(font, glyph_index)`, so it
   is expected to be identical from one compile to the next; the compile-epoch tag
   (§5.1) is a fallback should that ever not hold.

The right model is a **monotonically growing, content-keyed glyph library** shared
by all pages — not a per-page, per-frame blob.

**Pages have per-page sizes and are identified by index.** typst supports multiple
page sizes in one document (`#set page(width:, height:)`, `flipped: true`), so a
page is `(index, width, height, content)` — each `Page` carries its own `size`
(Zed already renders mixed sizes). Two useful properties:

1. **Index is a sufficient identity.** Within a compile epoch, page *k* is page
   *k*; that's all the client needs to correlate metadata, content, cached
   bitmaps, and scroll position — including across edits (§6.4).
2. **Pages are content-hashable.** A page's render inputs can be hashed cheaply, so
   the server can tell whether page *k* changed between renders (§6.3).

## 3. Why the current approach is weak

### 3.1 Glyph protocol efficiency

It is *normal* for the defs block to change between frames: while typing, most new
characters introduce a new glyph, so the whole-block hash changes and the
**entire** defs block (typically 80–90% of the SVG, ~1.5 MB on a dense page) is
resent. The optimization only pays off when the glyph set is momentarily stable —
exactly *not* the active-typing case it was meant to help.

The block-level view conflates two different quantities: the defs **block** changes
often (true), but the glyph **set** grows slowly — usually one symbol per
keystroke. A per-glyph delta sends the second; the whole-block strip pays the
first. Plus shared glyphs are duplicated in every page's defs.

### 3.2 Fragility (the invisible-glyph bug)

`strip_cached_glyph_defs` is a **stateful delta**: "when I strip, the defs are
byte-identical to the last full block I sent you." It assumes the client observes
every frame in order. Zed's frame-coalescing violates that: when a burst contains
`[full-defs frame introducing glyph D]` then `[stripped frame]`, Zed keeps only the
stripped frame, never caches D, and injects stale defs forever after — D's `<use>`
dangles and renders **invisible**. Small documents trigger this readily: fast
compiles (more coalescing) and a small starting glyph set (nearly every keystroke
is a new glyph). Root cause: *stateful glyph state* entangled with a *droppable
layout frame*.

### 3.3 Representation is not SVG-native

The client side is string surgery: find `<defs id="glyph">`, find the next
`</defs>`, splice. There is no model of glyphs as first-class entities, so the
cache can only be "the last blob" — which is what makes it fragile and whole-block.

### 3.4 It does not scale to large documents

Every keystroke costs **O(N pages)** on every dimension:

- **Serialize/transport:** `typst_svg::svg(page)` for all N pages, all sent —
  megabytes/keystroke at scale (and a global reflow, §3.5, resends full defs for
  most pages too).
- **Rasterize:** Zed runs resvg on every received page; a single update rasterizes
  all N. Frame-coalescing bounds the *burst rate*, not the *per-update* O(N).
- **Memory:** every page bitmap is held at 2× scale — an A4 page ≈ 8 MB, so a few
  hundred pages is multiple GB resident.

This is fine for the small docs it targets today and falls over well before
hundreds of pages.

### 3.5 Global reflow defeats the strip

Each page's defs hold only the glyphs used *on that page*. Insert a line near the
top of a long doc and text reflows across page boundaries, changing *which* glyphs
land on each page — so every page's whole-block defs hash changes and full defs are
resent for essentially every page. A top-insert thus degrades the current
optimization to "no compression at all." A document-global glyph registry (§5.1)
removes this failure mode: glyph ids are global, so reflow only repositions
`<use>`s and never resends a glyph.

## 4. Goals

1. Wire cost proportional to *new* glyphs, not the whole glyph set.
2. Robust to Zed's frame-coalescing (and to reordering / loss in general).
3. A principled client representation (glyphs as entities, not a blob).
4. Keep the client lightweight — avoid pulling the reflexo/typst.ts rendering stack
   (which drags in the full typst compiler; the reason `--server-svg` exists).
5. **Scale to hundreds of pages**: per-keystroke work O(visible pages), bounded
   client memory, smooth scroll.

Non-goals: any change to how the client rasterizes (glyph atlas, display list,
GPU); pixel-perfect parity with the browser preview; the HTML target; source-jump
and other interactive features.

## 5. Glyph transport efficiency (two layers)

Layer A fixes the bug and most of the bandwidth waste; Layer B is a follow-on.

### 5.1 Layer A — transport: a content-keyed glyph registry

Split the stream into two message kinds instead of one entangled SVG:

```
glyphs\n<defs id="glyph"><symbol id="gAAAA">…</symbol>…</defs>
page:{index}\n<svg …>…<use href="#gAAAA" …/>… (NO defs) …</svg>
```

Server (tinymist), per connection:

```text
sent: HashSet<GlyphId>                 # per-connection, monotonic
on render(document):
    referenced = union of glyph ids used by the pages being sent this frame
    new = referenced - sent
    if new is non-empty:
        emit  "glyphs\n" + <defs> containing only symbols for `new`
        sent |= new
    for each page being sent:
        emit  "page:index\n" + page-body-svg-without-defs
```

Client (Zed):

- Keep the glyph `library` as a **bounded LRU** (process-wide and shared across
  previews, §5.3), keyed by glyph id.
- **Always** apply every `glyphs\n` message to `library` (never coalesced, never
  dropped).
- Coalesce only `page:` frames.
- To rasterize page *i*: compose `<svg …><defs id="glyph">{symbols for the ids
  this page references}</defs>{page body}</svg>` from `library` + body.

Why this is better:

- **Bandwidth** = new glyphs only; shared glyphs sent once for the whole document,
  not per page, not per frame.
- **Bug fixed by construction:** glyph state travels in additive, idempotent
  messages that are never dropped; layout frames stay freely coalescable. The
  hazardous coupling in §3.2 is gone.
- **Order/loss tolerant:** applying `gAAAA` twice is a no-op; ids are identity.

Eviction & recovery: the `library` is a **bounded LRU** — glyph outlines are the
memory cost, so old glyphs are evicted under a budget. `want-glyphs` is therefore a
**steady-state** mechanism, not just belt-and-suspenders: when composing a page
references an id no longer in the library, the client pulls it
(`want-glyphs\ngAAAA,gBBBB` → server replies with a `glyphs\n` message from its full
`id → symbol` table). The server's `sent` set is a **best-effort delivery hint**:
after a client eviction it may claim the client still has a glyph it dropped, which
only means the server won't proactively resend — the pull reconciles. (The server
may keep `sent` loosely bounded too; over-eviction there just costs an occasional
idempotent resend.)

**LRU capacity floor.** Composing a page requires *all* of its glyphs to be in the
library at once. If a single page (or the set of pages composed together) references
more distinct glyphs than the LRU holds, composition would evict glyphs the same
compose still needs — thrash, and `want-glyphs` re-fetch just loops. So the budget
is not free to be arbitrarily small: it must be floored at the working set of the
pages composed together (visible + prefetch, §6.2). Two safeguards:

- **Pin during compose:** never evict a glyph referenced by a page currently being
  assembled.
- **Floor the budget** at ≥ the max distinct-glyph count across the simultaneously
  composed pages. In practice a document's used-font glyph set is bounded (a few
  thousand), so a generous fixed cap avoids thrash while the LRU still reclaims
  across documents/sessions. This makes LRU a cross-document/idle reclamation
  mechanism, not an intra-page one.

Cache lifetime: content-addressing keeps every entry valid across recompiles (an
id's meaning can never change), so the library is never *stale* — LRU eviction is
purely a memory-budget decision, never a correctness one, and routine growth is
handled continuously by the LRU. **No wholesale-resync signal is needed.** The two
situations that would seem to call for one are both already covered:

- **Reconnect / server restart** — a new connection's server `sent` set starts
  empty, so it re-announces glyphs the client may already hold. The merge is
  idempotent, so this costs a little redundant transport and nothing else.
- **Compile-epoch rollover** — document closed and reopened. Glyph ids are
  document-independent, so a rollover cannot invalidate a glyph.

A compile-epoch tag on `glyphs`/`page` still lets either side notice a rollover,
which matters for *page* state (index identity is only stable within an epoch,
§6.4). It carries no implication for the glyph library.

### 5.2 Layer B — a principled client representation

- `library: BTreeMap<GlyphId, SymbolMarkup>` is the single source of truth.
- Compose the per-page `<defs>` from the exact ids a page uses (one pass over the
  body's `href="#g…"` set — cheap).
- No "find the last `</defs>` and splice"; the only string work is concatenation
  from typed data we own.

About as SVG-native as the rasterizer path allows: resvg needs a self-contained
`<svg>` (usvg has no persistent cross-render symbol table), but authoring it is now
model-driven, not byte-scavenging.

### 5.3 Sharing the glyph library across previews

The glyph id is `hash128(&(font, glyph_id))` — it depends only on the **font and
glyph index, not the document**. So the same glyph has the same id and the same
`<symbol>` markup in *every* preview that uses that font (and previews
overwhelmingly share the default fonts). That makes the library naturally
**process-wide**, shared by all open typst previews:

- One `library` for the whole app dedups glyph outlines across documents. The merge
  is idempotent, so it is safe by construction — two connections announcing the
  same id cannot conflict.
- Fetching a glyph for document A makes it available to document B for free, so a
  newly-opened preview that shares fonts starts mostly warm.

Caveats:

- The server's `sent` set is **per-connection**, so sharing does *not* dedup
  transport across documents — document B's server still sends glyph X even if the
  shared client library already has it (the client just no-ops the merge).
  Advertising client holdings to every server to dedup transport is almost
  certainly not worth it; the win is client **memory**, for free.
- Lifetime is global: closing one preview must not drop glyphs another still shows.
  Memory management is a **global** LRU over the shared store (with `want-glyphs`
  re-fetch on a miss), reference-counted across previews — never a per-preview
  clear. The LRU capacity floor (§5.1) applies to the union of all previews'
  composed pages.

## 6. Large documents: page metadata + index subscription

The glyph layers fix per-page cost but not the O(N)-pages-per-keystroke problem
(§3.4). Scaling needs the client to deal only with pages near the viewport. The
design principle:

> The **server owns content and intrinsic page sizes.** The **viewer owns
> presentation** — the gap between pages, zoom, scroll offset, DPI, borders.
> Presentation must never leak to the server.

This is why viewport **coordinates** are the wrong currency: a document-space rect
would force the server to reconstruct the viewer's gaps/zoom to map it to pages.
Page **indices** are the natural interface — the viewer already computes which
indices intersect its viewport, so it names them.

### 6.1 Two channels: metadata and content

1. **Layout metadata (cheap, all pages).** `index → (width, height)` for the whole
   document, plus `total`. The viewer needs this to build the scroll region and
   decide which indices are visible *before* it has any content. It is small
   (N × a few numbers). **It is sent on connect and thereafter only when it
   changes** — i.e. when page count or any page size changes. Most edits don't
   alter page geometry, so it is rarely resent; a burst of typing that doesn't
   repaginate sends no `layout` messages at all.

   ```
   layout\n{ total, pages: [ {i, w, h}, … ] }      # server → client, on-change only
   ```

2. **Page content (SVG, on demand, by index).** Sent only for indices the viewer is
   subscribed to, and — combined with page-content hashing (§6.3) — only when that
   page actually changed.

   ```
   page:{index}\n<svg …>… (defs-free; glyphs via the §5.1 registry) …</svg>
   ```

The metadata channel is what enables size-accurate placeholders and kills layout
jump, which is the foundation for smooth scroll.

### 6.2 Index subscription as a retention contract

The viewer maintains a **standing subscription** rather than one-shot requests,
carrying up to three index sets:

```
view\n{ visible: [...], prefetch: [...], cached?: [...] }   # client → server, on scroll
```

- `visible` — on-screen indices (serve first; also drives scroll-sync /
  current-page).
- `prefetch` — a margin around `visible`, **biased toward the scroll direction** (a
  downward fling prefetches ahead, not behind).
- `cached` (optional) — *additional* indices the client still holds beyond
  `prefetch`. A client with memory headroom keeps a larger page cache and declares
  it, so scrolling back to a recently-seen page needs no resend when it hasn't
  changed.

The subscription doubles as a **retention contract**: the union
`held = visible ∪ prefetch ∪ cached` is exactly the set of pages the client
promises to keep. That is what lets the server know *what the client has* without
guessing — the hard part of skipping unchanged pages (§6.3). The client may evict
anything **outside** `held`; the server tracks nothing outside `held`; so eviction
and server-forgetting stay in lockstep at the same boundary, and a page is simply
resent when it re-enters `held`.

- **Placeholders from metadata:** an unfetched page draws as a correctly-sized box
  (optionally a stale/low-res thumbnail); scrolling never reflows, content pops in
  on arrival — standard PDF-viewer behavior (pdf.js, native viewers).
- **Memory budget = the `held` set.** Retention and prefetch are the same knob: the
  client sizes `prefetch`/`cached` to its bitmap budget (a page bitmap is ~8 MB at
  2×). This is the single lever that bounds page-bitmap memory.
- **Debounce flings:** settle to the resting viewport before emitting a `view`
  update, so a fling doesn't churn pages in and out of `held`; keep the prefetch
  margin generous enough that it lands on already-held pages.

`view` fits the existing binary `key,value` control-message channel tinymist
already uses (`partial-rendering,true`).

### 6.3 Sending only the pages that changed

Given `held` from §6.2, the server sends page `k` when `k ∈ held` **and** its
content differs from what it last sent this connection (or `k` just entered
`held`). Two questions:

*How does the server know a page changed?* Hash the page's **render inputs,
pre-render** — not the emitted SVG. `typst_svg::svg(page)` is a function of the
page's `frame` (laid-out content), its `bleed`, and its `fill` (background), plus a
constant `SvgOptions`. `Page` derives `Hash` and covers all of these, so `hash(page)`
is a **safe** change key: an unchanged hash guarantees identical SVG. Two nuances
from the typst types:

- **Don't hash `Frame` alone.** `Frame` is only the content geometry; it excludes
  `fill` and `bleed`. A `#set page(fill: …)` change leaves the frame identical, so a
  frame-only key would miss it and show a stale page — a correctness bug.
- **`Page` is slightly over-sensitive.** It also hashes `numbering`/`supplement`/
  `number`, which don't affect the SVG. A pure renumber — inserting an early page
  shifts later pages' logical `number` while their frames are byte-identical — would
  change the `Page` hash and needlessly resend an identical page. If that matters
  (large docs with early inserts), hash the render-affecting subset `(frame, bleed,
  fill)` instead: precise, still catches fill/bleed. Default to hashing `Page` for
  simplicity; tighten to the subset if renumber false-positives show up.

Hashing the input (not SVG bytes) lets tinymist **skip `typst_svg::svg(page)` for
unchanged pages** — saving render, not just transport. A change to `SvgOptions`
(e.g. toggling invert-colors) invalidates all pages; treat it as a global resync /
epoch bump.

*How does the server know the client still has the old page?* The retention contract
(§6.2). The server keeps a per-connection `sent_version: Map<Index, Hash>` scoped to
`held`: send when `k` is new-to-`held` or its hash changed; drop `sent_version[k]`
when `k` leaves `held`. No client-sent hashes, no eviction messages, no manifest
round-trip — the `view` declaration carries the missing information.

**Recovery — `want-page`.** A `want-page\nk` pull (server resends page `k`) mirrors
`want-glyphs`: the safety valve if a client is forced to evict inside `held` under
memory pressure, or wants a page it never subscribed to. **Planned on the tinymist
side** so the protocol is complete and robust for any client; **Zed does not send it
initially** — Zed honors its declared `held` set, so it never needs to pull. It can
be added later with no protocol change.

### 6.4 Reflow anchoring

When metadata changes under an edit (repagination shifts page count/sizes), the
viewer must not visually jump. **Page index is a sufficient anchor**: keep the
top-visible page index pinned across the metadata update and recompute scroll offset
from the new sizes. Nothing more is needed — no content fingerprints, no coordinate
remapping — because index is a stable identity within a compile epoch and the viewer
owns the offset math. (If a compile epoch rolls over — full reopen — indices are no
longer comparable, so the viewer drops its page state and starts from the new
`layout`. The glyph library is unaffected, §5.1.)

### 6.5 Precedent and honest limits

- tinymist *has* a "partial rendering" feature, but it is **client-side**: the
  server just forwards a `partial-rendering,true` flag and the typst.ts client culls
  rendering to its viewport (`IncrSvgDocClient::render_in_window(rect)`). There is
  **no server-side viewport gating** and no client→server viewport message that
  limits what the server produces — the reflexo path can stream the whole
  (incremental) doc cheaply and cull on the client. The `view` subscription here is
  therefore genuinely new protocol, though it reuses the existing control channel and
  scroll-position vocabulary.
- **Windowing does not reduce typst compile time.** Typst lays out the whole
  document regardless (page *k*'s position depends on everything before it).
  Windowing saves serialize-to-SVG, transport, rasterization, and memory — not
  compilation.

## 7. Recommendation & phasing

Two orthogonal tracks that compose:

**Track 1 — glyph transport efficiency**

1. **Layer A** — split `glyphs\n` (additive, never-dropped) from `page:\n`
   (coalescable, defs-free). *Fixes the invisible-glyph bug (§3.2) and the
   whole-block/per-page/reflow waste (§3.1, §3.5).* Small change both sides.
2. **Layer B** — structured client `library`, per-page id parse, `want-glyphs`
   recovery, LRU with the capacity floor (§5.1), compile-epoch tagging.

**Track 2 — large-document scaling**

3. **Metadata channel** (§6.1) — the `layout` message (geometry; sent on-change
   only). The client builds the scroll region, computes visibility, and draws
   size-accurate placeholders.
4. **Index subscription + retention contract + change detection** (§6.2–6.4) — the
   `view` subscription (`visible`/`prefetch`/`cached`) doubling as the retention
   contract, per-connection `sent_version` scoped to `held`, cheap pre-render
   page-hash change detection, and index-based reflow anchoring. `want-page` is
   planned server-side, deferred on the Zed side. Turns per-keystroke cost from
   O(doc) into O(`held`) and bounds memory.

Order by need: a small-doc deployment can ship Track 1 alone; large-doc support
requires Track 2. Layer A is the natural first PR; the metadata channel (step 3) is
small and independently useful (placeholders, no layout jump) and can precede the
full subscription.

## 8. Interaction with frame coalescing

Coalescing is worth keeping (it bounds rasterize work under fast typing). The design
makes it safe by moving all *stateful* data (the glyph library) into messages
applied unconditionally, leaving only *idempotent* `page:` frames to be dropped. A
dropped `page:` frame just means "an intermediate layout we skipped"; the next one
is complete on its own given the library. This is the core reason to split the
stream.

## 9. Alternatives considered

- **Keep whole-block strip, fix only Zed caching** (cache defs from every drained
  frame, not just the survivor). Fixes the bug, but keeps whole-block granularity,
  per-page duplication, and the reflow failure — no bandwidth win. Cheapest stopgap.
- **Per-frame defs byte-diff.** More complex and less robust than content-keyed ids;
  the ids already give the clean key.
- **Viewport in document coordinates.** Rejected (§6): couples the server to the
  viewer's gap/zoom/scroll layout; indices don't.
- **Adopt reflexo's incremental format.** `IncrSvgDocServer` already produces an
  incremental scene, but it is DOM-diff oriented (needs the typst.ts client), and
  `reflexo`/`reflexo-vec2svg` depend **unconditionally** on the `typst` crate,
  dragging the full compiler onto the client (~84 net-new crates, ~660 MB of rlibs
  measured against Zed's tree). Rejected: `--server-svg` exists precisely to keep the
  client to an SVG rasterizer.

## 10. Open questions

- **LRU sizing** (§5.1): the concrete budget and the capacity floor for the shared
  cross-preview store — sized to the union of all previews' composed (visible +
  prefetch) pages, with per-compose pinning to prevent thrash.
- **Page-change key** (§6.3): default to hashing `Page`; decide whether the
  renumber false-positive is common enough to warrant the `(frame, bleed, fill)`
  subset. Confirm typst exposes a cheap `Page`/`Frame` hash on this path (it derives
  `Hash`).
- **`SvgOptions` changes** (invert-colors, bleed): handle as a global epoch bump
  that invalidates all cached pages; confirm nothing else varies the per-page SVG.
