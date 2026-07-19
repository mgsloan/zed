# Typst preview: SVG streaming protocol design

Status: draft / proposal. The tinymist `--server-svg` patch has **not landed**, so
both sides of the wire are open to change and can be co-designed.

This document proposes a more efficient, more robust, less hacky, and
large-document-scalable way to stream a rendered typst document from tinymist to a
lightweight SVG-rasterizing client (Zed). It supersedes the current "whole-block
glyph-defs stripping + send every page every frame" approach.

It is a **protocol** design. How the client rasterizes the SVG it receives (resvg
today) is out of scope — no glyph atlas, display list, or other renderer changes are
proposed here.

It covers three largely independent axes:

- **Glyph transport efficiency** (§§2–5): the bytes per keystroke are dominated by
  glyph defs; how to send each glyph once.
- **Large-document scaling** (§6): how "send the whole document every frame" fails
  at hundreds of pages, and the index-based windowing that fixes it.
- **Wire transport** (§7): a Unix domain socket locally, TCP when the language
  server is remote.

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
   is expected to be identical from one compile to the next.

The right model is a **monotonically growing, content-keyed glyph library** shared
by all pages — not a per-page, per-frame blob.

**Pages are content-addressable, and the index is only a position.** typst supports
multiple page sizes in one document (`#set page(width:, height:)`, `flipped: true`),
so a page is `(width, height, content)` displayed at some index — each `Page`
carries its own `size` (Zed already renders mixed sizes). Two useful properties:

1. **Content is the identity; the index is a display position.** A page's render
   inputs hash cheaply, giving a content id that is *stable when the content moves*.
   That matters because content moving between indices is routine — insert a
   paragraph early and every later page shifts down one. Keying page bodies by
   content id makes such a shift a cheap remap instead of a resend of the whole
   document tail (§6.3).
2. **The hash must cover exactly what is drawn.** `typst_svg::svg(page)` renders
   from `frame`, `bleed` and `fill`; hashing those three is invariant precisely
   when the rendered SVG is invariant (§6.3).

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
page:{c}\n<svg …>…<use href="#gAAAA" …/>… (NO defs) …</svg>
```

(`{c}` is the page's content id, §6.3. Layer A only cares that glyph defs are
separated from page bodies; how bodies are addressed is §6's concern.)

Server (tinymist), per connection:

```text
sent: HashSet<GlyphId>                 # per-connection, monotonic
on render(document):
    referenced = union of glyph ids used by the page bodies being sent
    new = referenced - sent
    if new is non-empty:
        emit  "glyphs\n" + <defs> containing only symbols for `new`
        sent |= new
    for each body being sent:
        emit  "page:{c}\n" + page-body-svg-without-defs
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
handled continuously by the LRU. **No wholesale-resync signal (`reset`, epoch tag)
is needed**, for either glyphs or pages:

- **Reconnect / server restart** — the server's per-connection state starts empty,
  so it re-announces what the client may already hold. Every merge is idempotent
  (glyphs and page bodies are both content-keyed), so this costs a little redundant
  transport and nothing else.
- **Document closed and reopened** — a new connection with fresh state on both
  sides. Glyph ids are document-independent and page ids are content-derived, so
  nothing carried over can be invalidated.
- **A `SvgOptions` change** (invert-colors, bleed) invalidates rendered output
  without changing any content hash — but the server can simply clear its own
  sent-state and resend the held pages; the client replaces them on receipt. No
  client-visible signal required.

The condition that would reintroduce the need: sharing one `GlyphRegistry` across
connections to dedup transport, which would destroy the per-connection freshness
these all rely on.

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

### 6.1 Two channels: the page table and page bodies

**1. `pages` — the page table (incremental).** One index-keyed table carrying both
the geometry the viewer needs to lay out and the content id it needs to know *what*
to show:

```
pages\n{ total, full: true, pages: [ {i, w, h, c}, … ] }   # snapshot, on connect
pages\n{ total, pages: [ {i, c}, … ] }                     # delta, afterwards
```

- **`total` is in every message.** Page count changes ride along; on a shrink the
  client drops entries past `total`. No separate resize signal.
- **Entries carry only the fields that changed.** `w`/`h` and `c` are all optional,
  keyed by `i`. A keystroke sends `{i, c}` with no geometry; a page resize sends
  `w`/`h` with no `c`.
- **`full: true`** only on the first message after connect (the client has nothing);
  everything after is a delta.

Geometry and mapping are combined deliberately. Sent as whole snapshots they could
not be: geometry changes rarely but content ids change on nearly every keystroke, so
a full table per keystroke would be O(N) — reintroducing exactly the cost this
section exists to remove. Incrementality is what makes one table viable, and it
keeps geometry and mapping consistent by construction (they are one snapshot of the
document, never two views that can disagree).

**2. `page` — a page body, keyed by content id.** Sent once per distinct content,
*not* per index:

```
page:{c}\n<svg …>… (defs-free; glyphs via the §5.1 registry) …</svg>
```

The client caches bodies (and their rasterized bitmaps) by `c` and consults the
table to decide which `c` to display at which index.

The table is what enables size-accurate placeholders and kills layout jump, which is
the foundation for smooth scroll.

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
`held = visible ∪ prefetch ∪ cached` is exactly the set of pages the client promises
to keep. That is what lets the server know *what the client has* without guessing —
the hard part of not resending a body it already sent (§6.3). The client may evict
anything **outside** `held`; the server tracks nothing outside `held`; so eviction
and server-forgetting stay in lockstep at the same boundary, and a body is simply
resent when its content re-enters `held`. (The declaration is in indices; the server
maps them through the `pages` table to the content ids it must retain, §6.3.)

- **Placeholders from the table:** an unfetched page draws as a correctly-sized box
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

### 6.3 Content ids: sending each page body once, wherever it lands

The server sends a body for content id `c` when some held index maps to `c` and it
has not already sent `c` to this client. Three questions:

*What is the content id?* A hash of the page's **render inputs, pre-render** — not
the emitted SVG. `typst_svg::svg(page)` renders from the page's `frame` (laid-out
content), `bleed`, and `fill` (background), plus a constant `SvgOptions`. So the id
is `hash(frame, bleed, fill)`. Two consequences from the typst types:

- **Don't hash `Frame` alone.** `Frame` is only the content geometry; it excludes
  `fill` and `bleed`. A `#set page(fill: …)` change leaves the frame identical, so a
  frame-only id would miss it and show a stale page — a correctness bug.
- **Don't hash the whole `Page` either.** `Page` also covers `numbering`/
  `supplement`/`number`, none of which are drawn. `number` is the *logical* page
  number, which changes on exactly the shift we want to be free — so `hash(Page)`
  is not shift-invariant and would defeat content addressing. (If the number is
  actually printed in a header, that text lives *in the frame*, so the id changes
  and the page is correctly resent.)

Hashing `(frame, bleed, fill)` is therefore invariant precisely when the rendered
SVG is invariant — no more, no less. Hashing the input rather than SVG bytes also
lets tinymist **skip `typst_svg::svg(page)` entirely for content it has already
sent**, saving render, not just transport.

*Why content ids rather than per-index versions?* Because content moving between
indices is routine. Insert a paragraph near the top of a long document and every
later page shifts down one. With per-index versioning, index 6 now holds what index
5 held, so its version mismatches and it is resent — and so is the entire tail of
the document, even though the client already has every one of those pages. With
content ids the ids are unchanged; only the mapping moves, so the server sends a
small `pages` delta and **zero bodies**. Identical pages (blank pages, repeated
boilerplate) also dedup for free. The costs by event:

| event | `pages` delta | bodies sent |
| --- | --- | --- |
| keystroke on one page | 1 entry (`c`) | 1 |
| **pagebreak shift** | remap entries (`c` only) | **0** |
| page size change | entries with `w`/`h` | 0 |

This is the same reuse reflexo gets from its per-page `Fingerprint` and
`data-reuse-from`, without the DOM dependency: our "reuse" is a client-side cache
lookup by id rather than a reference into a live DOM.

*How does the server know the client still has a body?* The retention contract
(§6.2). The client declares held *indices*; the server maps them through the current
table to the content ids the client holds, and keeps a per-connection
`sent_content: HashSet<ContentId>` scoped to that set — dropping an id once no held
index maps to it. During a shift the two sides can briefly disagree about which ids
are held, which degrades to a redundant (idempotent) resend, never a stale page.

A change to `SvgOptions` (e.g. toggling invert-colors) changes rendered output
without changing any content id; the server clears its own `sent_content` and
resends the held bodies, and the client replaces them on receipt.

**Recovery — `want-page`.** A `want-page\n{c}` pull (server resends that body)
mirrors `want-glyphs`: the safety valve if a client is forced to evict a body it
declared held, or wants content it never subscribed to. **Planned on the tinymist
side** so the protocol is complete and robust for any client; **Zed does not send it
initially** — Zed honors its declared `held` set, so it never needs to pull. It can
be added later with no protocol change.

### 6.4 Reflow anchoring

When the table changes under an edit (repagination shifts page count and sizes), the
viewer must not visually jump. Anchor on **content, falling back to index**.

Before applying a `pages` update, capture the anchor: the content id `c` of the page
covering the top of the viewport, its index, and the scroll offset within that page.
After applying:

1. **Content anchor (preferred).** If `c` still appears in the table, scroll so that
   index sits at the same position, preserving the within-page offset. The view
   follows the content across the repagination. If `c` appears at several indices
   (duplicate pages — blanks, repeated boilerplate), pick the one nearest the
   previous index.
2. **Index fallback.** If `c` is gone from the table, pin the previous *index*
   instead (clamped to `total - 1`), again preserving the within-page offset.

The two cases are well matched to what causes them, which is why this is worth the
few extra lines over index-only anchoring:

- Editing *above* the viewport — the common case — shifts your page down without
  changing its content. Index anchoring would jump the view by however many pages
  shifted; content anchoring holds it still.
- The anchor's content id only disappears when the page you are *looking at* changed
  (you typed on it) or was deleted. There the page is still meaningfully "page *k*",
  so falling back to the index is exactly right.

Both branches are pure viewer-side math over the table; neither needs anything more
from the protocol, since content ids are already there for §6.3.

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

## 7. Transport: a local socket, TCP when remote

Everything above is a sequence of framed messages (`pages`, `glyphs`, `page:{c}`,
`view`, `want-*`) and says nothing about how the bytes move. Today the data plane is
`ws://127.0.0.1:{port}`, which is an artifact of tinymist's original client rather
than a requirement: typst.ts runs in a browser or webview, and a browser can only
speak WebSocket. A native rasterizing client has better options.

### 7.1 Local: Unix domain socket

`AF_UNIX` is cross-platform — Windows has supported it since 10/1803 — and Zed
already wraps both families in `crates/net` (`net::async_net::{UnixListener,
UnixStream}`), in production use by `askpass`, `context_server` and
`remote_server`. Over loopback TCP it buys:

- **Lower latency and higher throughput.** No TCP/IP stack traversal: no checksums,
  no Nagle, no ephemeral port allocation. This matters most for the bulk frames —
  the first glyph payload and page bodies.
- **Access control by filesystem permissions.** The current design opens a
  localhost TCP port that *any* local process can connect to and read the document
  from. A socket path gets ordinary file permissions instead. This is arguably the
  strongest argument, independent of performance.
- No port exhaustion, no firewall prompts.

Server side this is a bind option alongside the existing `--data-plane-host`, e.g.
`--data-plane-socket=<path>`, with the path returned from `doStartPreview` next to
today's `dataPlanePort`.

**Framing is not free here.** WebSocket delivers discrete messages; a Unix socket
delivers a byte stream, so the socket transport needs explicit framing — a 4-byte
little-endian length prefix per message is sufficient. The message *payloads* are
byte-identical to the WebSocket ones (`pages\n{…}`, `page:{c}\n<svg …>`), so this is
purely an envelope, and the protocol layers above are unchanged.

### 7.2 Remote: TCP

A Unix socket cannot cross hosts. When the language server runs on another machine
(Zed's SSH remote development), the data plane stays TCP/WebSocket. The client
chooses: if the server is local *and* `doStartPreview` returned a socket path, use
the socket; otherwise use the port. Zed already knows whether a project is remote,
so no negotiation handshake is required.

This also means the remote case keeps the bandwidth characteristics that motivated
the glyph registry and windowing in the first place — over a real network those
optimizations stop being nice-to-haves.

### 7.3 The browser client is unaffected

This *adds* a transport rather than replacing one; tinymist's own preview keeps
WebSocket. That bounds the upstream ask to a bind option plus one extra field in the
`doStartPreview` response — not a redesign of the data plane.

## 8. Recommendation & phasing

Two orthogonal tracks that compose:

**Track 1 — glyph transport efficiency**

1. **Layer A** — split `glyphs\n` (additive, never-dropped) from `page:\n`
   (coalescable, defs-free). *Fixes the invisible-glyph bug (§3.2) and the
   whole-block/per-page/reflow waste (§3.1, §3.5).* Small change both sides.
2. **Layer B** — structured client `library`, per-page id parse, `want-glyphs`
   recovery, LRU with the capacity floor (§5.1).

**Track 2 — large-document scaling**

3. **The `pages` table** (§6.1) — incremental index-keyed geometry + content ids.
   The client builds the scroll region, computes visibility, and draws
   size-accurate placeholders.
4. **Content-addressed bodies + subscription + retention** (§6.2–6.4) — `page:{c}`
   bodies keyed by `hash(frame, bleed, fill)`, the `view` subscription
   (`visible`/`prefetch`/`cached`) doubling as the retention contract, a
   per-connection `sent_content` set, and content-anchored reflow with index
   fallback. `want-page` is planned server-side, deferred on the Zed side. Turns
   per-keystroke cost from O(doc) into O(`held`), makes pagebreak shifts nearly
   free, and bounds memory.

Order by need: a small-doc deployment can ship Track 1 alone; large-doc support
requires Track 2. Layer A is the natural first PR; the `pages` table (step 3) is
small and independently useful (placeholders, no layout jump) and can precede the
full subscription.

## 9. Interaction with frame coalescing

Coalescing is worth keeping (it bounds rasterize work under fast typing). The design
makes it safe by moving all *stateful* data (the glyph library) into messages
applied unconditionally, leaving only *idempotent* `page:` frames to be dropped. A
dropped `page:` frame just means "an intermediate layout we skipped"; the next one
is complete on its own given the library. This is the core reason to split the
stream.

## 10. Alternatives considered

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
- **Send rasterized bitmaps instead of SVG.** Would delete the entire glyph
  machinery (a page bitmap has no shared sub-resources), and `typst-render` already
  memoizes glyph rasterization globally via comemo — keyed by
  `(font, glyph, subpixel, ppem)` and returning an alpha `Bitmap` tinted afterwards
  — so the server would get cross-page, cross-render glyph reuse for free. Rejected
  because it bakes **display resolution into the protocol**: the server must know
  DPI × zoom, so zoom becomes a round-trip, per-client rasterizations multiply, and
  the "server owns content, viewer owns presentation" boundary (§6) collapses.
  Raster also grows with the square of scale where the SVG body is scale-free.
- **Multiplex the data plane over the existing LSP connection** (custom
  `tinymist/...` notifications). Attractive because it needs no new transport and
  would work under remote development for free. Rejected for now: JSON-RPC forces
  string/base64 encoding of payloads, and large frames head-of-line block
  latency-sensitive LSP traffic (completions, diagnostics) on the same pipe. §7
  keeps TCP for the remote case instead.

## 11. Open questions

- **LRU sizing** (§5.1): the concrete budget and the capacity floor for the shared
  cross-preview store — sized to the union of all previews' composed (visible +
  prefetch) pages, with per-compose pinning to prevent thrash.
- **Content id composition** (§6.3): `(frame, bleed, fill)` is derived from what
  `typst_svg::svg` reads today. Confirm nothing else varies the rendered SVG, and
  that hashing the three components is as cheap as hashing `Page` (all derive
  `Hash`; `Page` is not usable because its `number` field breaks shift-invariance).
- **Held-set bookkeeping across a shift** (§6.3): the server maps held *indices* to
  content ids through the current table, so during a repagination the two sides can
  briefly disagree about which ids are held. This degrades to a redundant resend;
  confirm there is no case where it instead drops a body the client needs.
- **Socket path lifecycle** (§7.1): where the socket file lives, how a stale one
  from a crashed server is detected and cleaned up, and whether the path needs to be
  distinct per preview task. Windows `AF_UNIX` also has its own path-length and
  semantics quirks worth checking against `crates/net`'s shim.
- **Locality detection** (§7.2): Zed knows whether a project is remote, but confirm
  that is the right signal — e.g. a locally-running server against a remote
  filesystem, or a containerized language server that is "local" but cannot share a
  filesystem namespace for the socket.
