use crate::outline::{OutlineItem, query_outline_items};
use crate::treesitter_util::{
    expand_range_to_line_boundaries, range_intersection, range_is_superset_of, range_size,
};
use crate::zed_code::Language;
use clap::Args;
use std::ops::Range;
use tree_sitter::{Tree, TreeCursor};

#[derive(Args, Debug, Clone)]
pub struct ExcerptOptions {
    /// Limit for the number of bytes in the window around the cursor.
    #[arg(long, default_value_t = 4096)]
    pub window_max_bytes: usize,
    /// Target ratio of bytes before the cursor vs after the cursor
    #[arg(long, default_value_t = 0.75)]
    pub before_cursor_bytes_ratio: f32,
}

impl Default for ExcerptOptions {
    fn default() -> Self {
        Self {
            window_max_bytes: 4096,
            before_cursor_bytes_ratio: 0.5,
        }
    }
}

#[derive(Clone)]
pub struct ExcerptRanges {
    pub excerpt_range: Range<usize>,
    pub parent_signature_ranges: Vec<Range<usize>>,
}

impl ExcerptRanges {
    pub fn size(&self) -> usize {
        self.excerpt_range.len()
            + self
                .parent_signature_ranges
                .iter()
                .map(|r| r.len())
                .sum::<usize>()
    }
}

pub struct ExcerptRangesInput<'a> {
    pub language: &'a Language,
    pub tree: &'a Tree,
    pub source: &'a str,
    pub cursor_offset: usize,
    pub options: &'a ExcerptOptions,
}

impl ExcerptRangesInput<'_> {
    /// An excerpt around the cursor is selected by finding a sequence of AST nodes that contains the
    /// line the cursor is on, upto a byte limit for the window. The excerpt also includes the signatures
    /// of parent outline items.
    ///
    /// If there are no AST nodes or none fit, falls back on line-based expansion.
    ///
    /// Both selecting a sequence of AST nodes and line-based expansion are configured by a target ratio
    /// of bytes before/after the cursor.
    ///
    /// Returns `None` if the line around the cursor doesn't fit.
    pub fn select(&self) -> Option<ExcerptRanges> {
        let source_length = self.source.len();
        if source_length <= self.options.window_max_bytes {
            log::debug!(
                "using entire file for excerpt since source length ({}) <= window max bytes ({})",
                source_length,
                self.options.window_max_bytes
            );
            return Some(ExcerptRanges {
                excerpt_range: 0..self.source.len(),
                parent_signature_ranges: Vec::new(),
            });
        }

        let query_range =
            expand_range_to_line_boundaries(self.source, self.cursor_offset..self.cursor_offset);
        if query_range.end - query_range.start > self.options.window_max_bytes {
            return None;
        }

        // TODO: Not efficient to query all outline items.
        let all_outline_items: Vec<OutlineItem> =
            query_outline_items(self.language, self.tree, self.source);
        let containing_items: Vec<&OutlineItem> = all_outline_items
            .iter()
            .filter(|item| range_is_superset_of(&item.item_range, &query_range))
            .collect();

        // TODO: If neighboring nodes are all large, this can be quite a small selection, should
        // have a fallback in that case. One option is line-based, but a better option would be to
        // descend into neighboring nodes.
        if let Some(excerpt_ranges) = self.select_via_ast(query_range.clone(), &containing_items) {
            return Some(excerpt_ranges);
        }

        log::debug!("falling back on line-based selection");
        if let Some(excerpt_ranges) = self.select_via_lines(query_range.clone(), &containing_items)
        {
            log::debug!("excerpt selection used line-based fallback");
            return Some(excerpt_ranges);
        }

        log::error!("bug: select_via_lines failed but its expected preconditions were checked.");
        None
    }

    fn select_via_ast(
        &self,
        query_range: Range<usize>,
        containing_items: &[&OutlineItem],
    ) -> Option<ExcerptRanges> {
        let mut cursor = self.tree.walk();

        // Find the largest node that contains `query_range` while being smaller than the
        // window size.
        loop {
            let node_range = cursor.node().byte_range();
            // TODO: not efficient
            let line_range = expand_range_to_line_boundaries(self.source, node_range.clone());
            if range_is_superset_of(&line_range, &query_range) {
                let excerpt_size = range_size(line_range.clone());
                let signatures_size = self.signatures_size(containing_items, line_range.clone());
                if excerpt_size + signatures_size <= self.options.window_max_bytes {
                    log::debug!(
                        "found small enough node containing cursor line, \
                        excerpt size: {}, signatures size: {}",
                        excerpt_size,
                        signatures_size
                    );
                    // TODO: not efficient to recompute signatures when this doesn't expand.
                    return Some(self.expand_to_siblings(
                        &mut cursor,
                        containing_items,
                        signatures_size,
                    ));
                } else {
                    log::debug!(
                        "encountered node that is too large containing cursor line, \
                        excerpt size: {}, signatures size: {}",
                        excerpt_size,
                        signatures_size
                    );
                }
            } else {
                // TODO: Should still be able to handle this case via AST nodes. For example, this
                // can happen if the cursor is between two methods in a large class file.
                return None;
            }

            if cursor
                .goto_first_child_for_byte(query_range.start)
                .is_none()
            {
                return None;
            }
        }
    }

    fn expand_to_siblings(
        &self,
        cursor: &mut TreeCursor,
        containing_items: &[&OutlineItem],
        signatures_size: usize,
    ) -> ExcerptRanges {
        let mut excerpt_range =
            expand_range_to_line_boundaries(self.source, cursor.node().byte_range());
        let mut forward_cursor = cursor.clone();
        let backward_cursor = cursor;
        let mut forward_done = !forward_cursor.goto_next_sibling();
        let mut backward_done = !backward_cursor.goto_previous_sibling();
        loop {
            if backward_done && forward_done {
                break;
            }

            let forward_range = if !forward_done {
                Some(expand_range_to_line_boundaries(
                    self.source,
                    excerpt_range.start..forward_cursor.node().end_byte(),
                ))
            } else {
                None
            };

            let backward_range = if !backward_done {
                Some(expand_range_to_line_boundaries(
                    self.source,
                    backward_cursor.node().start_byte()..excerpt_range.end,
                ))
            } else {
                None
            };

            let go_forward = match (forward_range, backward_range) {
                (Some(forward_range), Some(backward_range)) => {
                    if let Some(go_forward) = self.is_better_excerpt_range(
                        forward_range.clone(),
                        backward_range.clone(),
                        signatures_size,
                    ) {
                        if go_forward {
                            excerpt_range = forward_range;
                        } else {
                            excerpt_range = backward_range;
                        }
                        go_forward
                    } else {
                        break;
                    }
                }
                (Some(forward_range), None) => {
                    if self.excerpt_range_qualifies(forward_range.clone(), signatures_size) {
                        log::debug!(
                            "expanding excerpt forward since there is nothing to expand backwards"
                        );
                        excerpt_range = forward_range;
                        true
                    } else {
                        log::debug!(
                            "halting excerpt expansion since forward direction does not fit"
                        );
                        break;
                    }
                }
                (None, Some(backward_range)) => {
                    if self.excerpt_range_qualifies(backward_range.clone(), signatures_size) {
                        log::debug!(
                            "expanding excerpt backward since there is nothing to expand forwards"
                        );
                        excerpt_range = backward_range;
                        false
                    } else {
                        log::debug!(
                            "halting excerpt expansion since backward direction does not fit"
                        );
                        break;
                    }
                }
                (None, None) => {
                    log::debug!(
                        "halting excerpt expansion since there is nothing in either direction"
                    );
                    break;
                }
            };

            if go_forward {
                forward_done = !forward_cursor.goto_next_sibling();
            } else {
                backward_done = !backward_cursor.goto_previous_sibling();
            }
        }

        // TODO: Not efficient - this should always be the same signature ranges as computed earlier
        // for size.
        let parent_signature_ranges =
            self.signature_offset_ranges(containing_items, excerpt_range.clone());
        ExcerptRanges {
            excerpt_range,
            parent_signature_ranges,
        }
    }

    /// TODO: this is quite inefficient and LLM generated code. Will be rewritten, probably similar
    /// to zeta/src/input_excerpt.rs. Could possibly do some smarter search as well.
    fn select_via_lines(
        &self,
        query_range: Range<usize>,
        containing_items: &[&OutlineItem],
    ) -> Option<ExcerptRanges> {
        let mut excerpt_range = query_range.clone();
        let excerpt_size = range_size(excerpt_range.clone());
        let signatures_size = self.signatures_size(containing_items, excerpt_range.clone());

        // Early return if the initial range is already too large
        if !self.excerpt_range_qualifies(excerpt_range.clone(), signatures_size) {
            log::debug!(
                "range for line expansion is already too large. \
                excerpt_size: {}, signatures_size: {}",
                excerpt_size,
                signatures_size
            );
            return None;
        }

        // Expand line by line until we reach the size limit or optimal ratio
        loop {
            // Try expanding forward (after cursor)
            let expanded_forward = {
                let line_end =
                    crate::treesitter_util::line_end_from_offset(self.source, excerpt_range.end);
                if line_end < self.source.len() {
                    let next_line_end = self.source[line_end..]
                        .find('\n')
                        .map(|pos| line_end + pos + 1)
                        .unwrap_or(self.source.len());
                    Some(excerpt_range.start..next_line_end)
                } else {
                    None
                }
            };

            // Try expanding backward (before cursor)
            let expanded_backward = {
                let line_start = crate::treesitter_util::line_start_from_offset(
                    self.source,
                    excerpt_range.start,
                );
                if line_start > 0 {
                    let prev_line_start = self.source[..line_start.saturating_sub(1)]
                        .rfind('\n')
                        .map(|pos| pos + 1)
                        .unwrap_or(0);
                    Some(prev_line_start..excerpt_range.end)
                } else {
                    None
                }
            };

            let signatures_size = self.signatures_size(containing_items, excerpt_range.clone());
            // Use is_better_excerpt_range to choose the best expansion
            let next_range = match (expanded_forward, expanded_backward) {
                (Some(forward), Some(backward)) => {
                    if let Some(go_forward) = self.is_better_excerpt_range(
                        forward.clone(),
                        backward.clone(),
                        signatures_size,
                    ) {
                        if go_forward {
                            Some(forward)
                        } else {
                            Some(backward)
                        }
                    } else {
                        None // Neither range fits
                    }
                }
                (Some(forward), None) => {
                    if self.excerpt_range_qualifies(forward.clone(), signatures_size) {
                        Some(forward)
                    } else {
                        None
                    }
                }
                (None, Some(backward)) => {
                    if self.excerpt_range_qualifies(backward.clone(), signatures_size) {
                        Some(backward)
                    } else {
                        None
                    }
                }
                (None, None) => None,
            };

            match next_range {
                Some(new_range) => {
                    excerpt_range = new_range;
                }
                None => break, // Can't expand further
            }
        }

        let parent_signature_ranges =
            self.signature_offset_ranges(containing_items, excerpt_range.clone());
        Some(ExcerptRanges {
            excerpt_range,
            parent_signature_ranges,
        })
    }

    fn signature_offset_ranges(
        &self,
        containing_items: &[&OutlineItem],
        excerpt_range: Range<usize>,
    ) -> Vec<Range<usize>> {
        containing_items
            .iter()
            .filter(|item| range_is_superset_of(&item.item_range, &excerpt_range))
            .map(|item| {
                // TODO: Not efficient to find line ranges here.
                expand_range_to_line_boundaries(self.source, item.signature_range.clone())
            })
            .collect()
    }

    fn signatures_size(
        &self,
        containing_items: &[&OutlineItem],
        excerpt_range: Range<usize>,
    ) -> usize {
        self.signature_offset_ranges(containing_items, excerpt_range.clone())
            .into_iter()
            .map(|offset_range| {
                if let Some(intersection) = range_intersection(&offset_range, &excerpt_range) {
                    range_size(offset_range) - range_size(intersection)
                } else {
                    range_size(offset_range)
                }
            })
            .sum()
    }

    /// Returns None if neither range fits in window_max_bytes. Returns `true` if only `forward`
    /// fits or if it is closer to the target ratio, and otherwise returns `false`.
    fn is_better_excerpt_range(
        &self,
        forward: Range<usize>,
        backward: Range<usize>,
        signatures_size: usize,
    ) -> Option<bool> {
        match (
            self.excerpt_range_ratio(forward, signatures_size),
            self.excerpt_range_ratio(backward, signatures_size),
        ) {
            (Some(forward_ratio), Some(backward_ratio)) => {
                let forward_delta = (forward_ratio - self.options.before_cursor_bytes_ratio).abs();
                let backward_delta =
                    (backward_ratio - self.options.before_cursor_bytes_ratio).abs();
                let forward_is_better = forward_delta <= backward_delta;
                if forward_is_better {
                    log::debug!(
                        "expanding excerpt forward since {} is closer than {} to {}",
                        forward_ratio,
                        backward_ratio,
                        self.options.before_cursor_bytes_ratio
                    );
                } else {
                    log::debug!(
                        "expanding excerpt backward since {} is closer than {} to {}",
                        backward_ratio,
                        forward_ratio,
                        self.options.before_cursor_bytes_ratio
                    );
                }
                Some(forward_is_better)
            }
            (Some(_), None) => {
                log::debug!(
                    "expanding excerpt forward since going backwards exceeds window_max_bytes"
                );
                Some(true)
            }
            (None, Some(_)) => {
                log::debug!(
                    "expanding excerpt backward since going forwards exceeds window_max_bytes"
                );
                Some(false)
            }
            (None, None) => {
                log::debug!("halting excerpt expansion since neither direction fits");
                None
            }
        }
    }

    /// Returns the ratio of bytes before the cursor over bytes within the range. Returns None if
    /// the range is larger than `window_max_bytes`.
    fn excerpt_range_ratio(&self, range: Range<usize>, signatures_size: usize) -> Option<f32> {
        if self.excerpt_range_qualifies(range.clone(), signatures_size) {
            let range_size = range_size(range.clone());
            let bytes_before_cursor = self.cursor_offset - range.start;
            Some(bytes_before_cursor as f32 / range_size as f32)
        } else {
            None
        }
    }

    fn excerpt_range_qualifies(&self, range: Range<usize>, signatures_size: usize) -> bool {
        let range_size = range_size(range.clone());
        range_size + signatures_size <= self.options.window_max_bytes
    }
}
