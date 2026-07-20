//! The page table and the image cache: what to show, and what we have.
//!
//! **Two keyings, deliberately.** The table is keyed by **index** (a display
//! position); images are keyed by **content id**. A pagebreak shift rewrites the
//! table and touches no image; a zoom touches neither, and only changes whether
//! a cached image is still at the current scale.

use collections::HashMap;
use gpui::{RenderImage, Size, size};
use smallvec::SmallVec;
use std::sync::Arc;

use crate::protocol::{ContentId, TableEntryDelta};

/// One page's geometry and identity.
#[derive(Clone, Debug, PartialEq)]
pub struct PageTableEntry {
    /// Intrinsic size in **points**, not pixels.
    ///
    /// Keeping it scale-independent is what lets a zoom leave the scroll region
    /// and every placeholder untouched. Storing pixels here would silently
    /// reintroduce the layout jump the table exists to prevent.
    pub size: Size<f32>,
    /// `None` before the server has told us what is at this index.
    pub content: Option<ContentId>,
}

impl Default for PageTableEntry {
    fn default() -> Self {
        // A4 in points, so an entry we have not heard about yet still lays out
        // plausibly rather than collapsing to zero height.
        Self {
            size: size(595.0, 842.0),
            content: None,
        }
    }
}

/// What the view needs to know after a table update, computed once here so the
/// view never re-derives a diff.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableDelta {
    pub total: usize,
    /// The page count moved, so list slots must be spliced.
    pub total_changed: bool,
    /// These indices changed point-size, so they must be re-measured.
    pub geometry_changed: SmallVec<[usize; 8]>,
    /// Some index now maps to different content, so the view may need to
    /// re-anchor on the content it was looking at.
    pub content_moved: bool,
}

/// Index -> geometry + content id, applied incrementally.
#[derive(Default)]
pub struct PageTable {
    entries: Vec<PageTableEntry>,
    /// Per index, the content id we last had an image for.
    ///
    /// Deliberately *not* cleared when an index's content changes: it is what
    /// the index keeps showing while the replacement image is in flight. See
    /// [`lookup_for_index`].
    displayed: Vec<Option<ContentId>>,
}

impl PageTable {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, index: usize) -> Option<&PageTableEntry> {
        self.entries.get(index)
    }

    pub fn content_at(&self, index: usize) -> Option<ContentId> {
        self.entries.get(index)?.content
    }

    /// Applies one `pages` message.
    ///
    /// `full` replaces rather than merges. Merging from empty would also work,
    /// but honoring the flag makes a reconnect with stale state safe by
    /// construction.
    pub fn apply(&mut self, total: usize, full: bool, entries: &[TableEntryDelta]) -> TableDelta {
        if full {
            self.entries.clear();
            self.displayed.clear();
        }

        let total_changed = self.entries.len() != total;
        self.entries.resize(total, PageTableEntry::default());
        self.displayed.resize(total, None);

        let mut delta = TableDelta {
            total,
            total_changed,
            ..Default::default()
        };

        for entry in entries {
            let Some(slot) = self.entries.get_mut(entry.index) else {
                // Racing a shrink is normal, not an error.
                continue;
            };

            // `None` means unchanged; only overwrite what the delta carries.
            if let (Some(w), Some(h)) = (entry.width, entry.height) {
                let new_size = size(w, h);
                if slot.size != new_size {
                    slot.size = new_size;
                    delta.geometry_changed.push(entry.index);
                }
            }
            if let Some(content) = entry.content
                && slot.content != Some(content)
            {
                slot.content = Some(content);
                delta.content_moved = true;
            }
        }

        delta
    }

    /// The content id last drawn at this index, if any.
    pub fn displayed_at(&self, index: usize) -> Option<ContentId> {
        self.displayed.get(index).copied().flatten()
    }

    /// Records that `index` is now showing `content`.
    pub fn record_displayed(&mut self, index: usize, content: ContentId) {
        if let Some(slot) = self.displayed.get_mut(index) {
            *slot = Some(content);
        }
    }

    /// Every id an index is currently relying on — the one the table says it
    /// should show, plus the one it is actually showing until that arrives.
    pub fn ids_in_use(&self, indices: impl IntoIterator<Item = usize>) -> Vec<ContentId> {
        let mut ids = Vec::new();
        for index in indices {
            ids.extend(self.content_at(index));
            ids.extend(self.displayed_at(index));
        }
        ids
    }

    /// The index nearest `near` whose content is `content`.
    ///
    /// An id can appear at several indices (blank pages), so picking the first
    /// match would teleport to the first blank page on every edit.
    pub fn find_content(&self, content: ContentId, near: usize) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.content == Some(content))
            .min_by_key(|(index, _)| index.abs_diff(near))
            .map(|(index, _)| index)
    }
}

/// A decoded page, plus the scale it was rendered at.
struct CachedImage {
    image: Arc<RenderImage>,
    /// Taken from the image header, never inferred. This is what makes the
    /// in-flight case correct: an image requested before a zoom can arrive
    /// after it.
    rendered_scale: f32,
    bytes: usize,
}

/// What we can draw for a given index.
pub enum PageImage {
    /// Correct content at the current scale.
    Current(Arc<RenderImage>),
    /// Correct content at the wrong scale — draw it stretched while the
    /// replacement renders.
    Stale(Arc<RenderImage>),
    /// The server said this content will not render.
    Failed,
    /// Nothing yet; draw a placeholder box.
    Missing,
}

/// Content id -> decoded page.
///
/// **Scale is metadata on the entry, not part of the key.** Keying on
/// `(ContentId, scale)` would let the cache hold several scales of the same
/// page, which sounds like a feature and is really an unbounded one at ~8 MB
/// apiece: the connection only ever renders at one scale, so the extra entries
/// could never be served, only retained. Overwrite-on-arrival is also what
/// bounds this cache without an eviction rule of its own.
#[derive(Default)]
pub struct ImageCache {
    entries: HashMap<ContentId, CachedImage>,
    failed: collections::HashSet<ContentId>,
    resident_bytes: usize,
}

impl ImageCache {
    pub fn insert(&mut self, content: ContentId, image: Arc<RenderImage>, rendered_scale: f32) {
        let bytes = image_bytes(&image);
        if let Some(previous) = self.entries.remove(&content) {
            self.resident_bytes -= previous.bytes;
        }
        // An arriving image supersedes a recorded failure. A failure is not a
        // tombstone: a scale change re-renders the same id server-side, which is
        // exactly how a page too large to rasterize recovers when zoomed out.
        self.failed.remove(&content);
        self.resident_bytes += bytes;
        self.entries.insert(
            content,
            CachedImage {
                image,
                rendered_scale,
                bytes,
            },
        );
    }

    pub fn mark_failed(&mut self, content: ContentId) {
        if let Some(previous) = self.entries.remove(&content) {
            self.resident_bytes -= previous.bytes;
        }
        self.failed.insert(content);
    }

    pub fn lookup(&self, content: Option<ContentId>, current_scale: f32) -> PageImage {
        let Some(content) = content else {
            return PageImage::Missing;
        };
        if let Some(entry) = self.entries.get(&content) {
            return if entry.rendered_scale == current_scale {
                PageImage::Current(entry.image.clone())
            } else {
                PageImage::Stale(entry.image.clone())
            };
        }
        if self.failed.contains(&content) {
            return PageImage::Failed;
        }
        PageImage::Missing
    }

    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Drops everything not reachable from the held set.
    ///
    /// Eviction is by *absence from `held`*, never by per-index refcount: the
    /// index-to-content map is many-to-one, so "index left the window, drop its
    /// image" would evict an image another held index still displays.
    pub fn retain_held(&mut self, held: &collections::HashSet<ContentId>) {
        self.entries.retain(|content, entry| {
            let keep = held.contains(content);
            if !keep {
                self.resident_bytes -= entry.bytes;
            }
            keep
        });
        self.failed.retain(|content| held.contains(content));
    }
}

/// What to draw at `index`, falling back to the previous content while a
/// replacement is in flight.
///
/// The fallback is what removes the white flash after a keystroke. A `pages`
/// delta parses instantly while the image behind it needs a few milliseconds to
/// decode, so the two land in different batches: for that window the table
/// points at a content id nothing has yet, and drawing `Missing` paints a white
/// box over a page whose content barely changed. Showing the previous render for
/// those few milliseconds is what every PDF viewer does.
pub fn lookup_for_index(
    table: &PageTable,
    images: &ImageCache,
    index: usize,
    scale: f32,
) -> PageImage {
    let current = table.content_at(index);
    match images.lookup(current, scale) {
        // Only `Missing` falls back. `Failed` is a real answer, and `Stale`
        // already has the right content at the wrong scale.
        PageImage::Missing => {
            let previous = table.displayed_at(index);
            match previous {
                Some(previous) if Some(previous) != current => {
                    // Wrong *content*, briefly — flagged Stale so nothing treats
                    // it as satisfied.
                    match images.lookup(Some(previous), scale) {
                        PageImage::Current(image) | PageImage::Stale(image) => {
                            PageImage::Stale(image)
                        }
                        _ => PageImage::Missing,
                    }
                }
                _ => PageImage::Missing,
            }
        }
        other => other,
    }
}

fn image_bytes(image: &RenderImage) -> usize {
    let size = image.size(0);
    (size.width.0 as usize) * (size.height.0 as usize) * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> ContentId {
        format!("{n:032x}").parse().unwrap()
    }

    fn delta(index: usize, wh: Option<(f32, f32)>, content: Option<u128>) -> TableEntryDelta {
        TableEntryDelta {
            index,
            width: wh.map(|(w, _)| w),
            height: wh.map(|(_, h)| h),
            content: content.map(id),
        }
    }

    #[test]
    fn full_snapshot_populates_geometry_and_content() {
        let mut table = PageTable::default();
        let result = table.apply(
            2,
            true,
            &[
                delta(0, Some((595.0, 842.0)), Some(1)),
                delta(1, Some((595.0, 842.0)), Some(2)),
            ],
        );

        assert_eq!(result.total, 2);
        assert!(result.total_changed);
        assert_eq!(table.len(), 2);
        assert_eq!(table.content_at(1), Some(id(2)));
    }

    #[test]
    fn content_only_delta_preserves_geometry() {
        // The regression that would clear every page's size on each keystroke.
        let mut table = PageTable::default();
        table.apply(1, true, &[delta(0, Some((595.0, 842.0)), Some(1))]);

        let result = table.apply(1, false, &[delta(0, None, Some(9))]);
        assert_eq!(table.get(0).unwrap().size, size(595.0, 842.0));
        assert_eq!(table.content_at(0), Some(id(9)));
        assert!(result.content_moved);
        assert!(result.geometry_changed.is_empty());
        assert!(!result.total_changed);
    }

    #[test]
    fn geometry_change_is_reported_for_remeasure() {
        let mut table = PageTable::default();
        table.apply(1, true, &[delta(0, Some((595.0, 842.0)), Some(1))]);

        let result = table.apply(1, false, &[delta(0, Some((200.0, 300.0)), None)]);
        assert_eq!(result.geometry_changed.as_slice(), &[0]);
        assert!(!result.content_moved);
    }

    #[test]
    fn shrink_truncates() {
        let mut table = PageTable::default();
        table.apply(
            3,
            true,
            &[
                delta(0, Some((1.0, 1.0)), Some(1)),
                delta(1, Some((1.0, 1.0)), Some(2)),
                delta(2, Some((1.0, 1.0)), Some(3)),
            ],
        );

        let result = table.apply(1, false, &[]);
        assert_eq!(table.len(), 1);
        assert!(result.total_changed);
        assert_eq!(table.content_at(1), None);
    }

    #[test]
    fn find_content_prefers_the_nearest_match() {
        // Duplicate ids are real (blank pages), so the first match would
        // teleport the viewport to the first blank page on every edit.
        let mut table = PageTable::default();
        table.apply(
            5,
            true,
            &[
                delta(0, Some((1.0, 1.0)), Some(7)),
                delta(1, Some((1.0, 1.0)), Some(1)),
                delta(2, Some((1.0, 1.0)), Some(2)),
                delta(3, Some((1.0, 1.0)), Some(7)),
                delta(4, Some((1.0, 1.0)), Some(3)),
            ],
        );

        assert_eq!(table.find_content(id(7), 3), Some(3));
        assert_eq!(table.find_content(id(7), 0), Some(0));
        assert_eq!(table.find_content(id(99), 0), None);
    }

    fn test_image() -> Arc<RenderImage> {
        use image::{Frame, ImageBuffer, Rgba};
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(1, 1, vec![0, 0, 0, 255]).unwrap();
        Arc::new(RenderImage::new(smallvec::SmallVec::from_const([Frame::new(
            buffer,
        )])))
    }

    /// The white-flash regression.
    ///
    /// A keystroke's `pages` delta arrives a few milliseconds before the image
    /// behind it finishes decoding. In that window the table points at a content
    /// id nothing has, and without a fallback the page paints white.
    #[test]
    fn keeps_showing_the_previous_render_while_the_next_decodes() {
        let mut table = PageTable::default();
        let mut images = ImageCache::default();

        table.apply(1, true, &[delta(0, Some((595.0, 842.0)), Some(1))]);
        images.insert(id(1), test_image(), 2.0);
        table.record_displayed(0, id(1));
        assert!(matches!(
            lookup_for_index(&table, &images, 0, 2.0),
            PageImage::Current(_)
        ));

        // Keystroke: the table moves to a new id whose image has not decoded.
        table.apply(1, false, &[delta(0, None, Some(2))]);
        assert!(
            matches!(
                lookup_for_index(&table, &images, 0, 2.0),
                PageImage::Stale(_)
            ),
            "must keep drawing the old page, not flash white"
        );

        // The replacement lands.
        images.insert(id(2), test_image(), 2.0);
        table.record_displayed(0, id(2));
        assert!(matches!(
            lookup_for_index(&table, &images, 0, 2.0),
            PageImage::Current(_)
        ));
    }

    #[test]
    fn a_page_never_drawn_is_missing_not_stale() {
        // The fallback must not invent content for a page that has never had an
        // image: that is a genuine placeholder.
        let mut table = PageTable::default();
        let images = ImageCache::default();
        table.apply(1, true, &[delta(0, Some((595.0, 842.0)), Some(1))]);

        assert!(matches!(
            lookup_for_index(&table, &images, 0, 2.0),
            PageImage::Missing
        ));
    }

    #[test]
    fn ids_in_use_covers_the_fallback_so_it_is_not_evicted() {
        // Retention must keep the image an index is *actually showing*, or the
        // fallback has nothing to fall back to.
        let mut table = PageTable::default();
        table.apply(1, true, &[delta(0, Some((595.0, 842.0)), Some(1))]);
        table.record_displayed(0, id(1));
        table.apply(1, false, &[delta(0, None, Some(2))]);

        let ids = table.ids_in_use([0]);
        assert!(ids.contains(&id(1)), "the displayed image must be retained");
        assert!(ids.contains(&id(2)), "the incoming image must be retained");
    }

    #[test]
    fn a_pagebreak_shift_moves_content_without_touching_geometry() {
        let mut table = PageTable::default();
        table.apply(
            2,
            true,
            &[
                delta(0, Some((595.0, 842.0)), Some(1)),
                delta(1, Some((595.0, 842.0)), Some(2)),
            ],
        );

        // Insert a page at the front: ids move, geometry does not.
        let result = table.apply(
            3,
            false,
            &[
                delta(0, None, Some(7)),
                delta(1, None, Some(1)),
                delta(2, Some((595.0, 842.0)), Some(2)),
            ],
        );

        assert!(result.content_moved);
        assert!(result.total_changed);
        assert_eq!(table.content_at(1), Some(id(1)));

        // The point of content addressing: pages that merely moved must not be
        // reported as needing a re-measure. A newly appended index may or may
        // not appear here depending on whether its size differs from the
        // placeholder default, and either is fine — the tail splice already
        // makes the list measure it.
        assert!(
            !result.geometry_changed.contains(&0) && !result.geometry_changed.contains(&1),
            "a shift must not remeasure pages the list already sized: {:?}",
            result.geometry_changed
        );
    }
}
