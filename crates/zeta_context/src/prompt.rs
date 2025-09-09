use priority_queue::PriorityQueue;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::Path,
    sync::Arc,
};
use strum::IntoEnumIterator;

use crate::{
    excerpt::ExcerptRanges,
    identifier_index::IdentifierIndex,
    identifier_references::{ScoredSnippet, SnippetStyle},
    outline::{OutlineId, OutlineItem},
    treesitter_util::expand_range_to_line_boundaries,
};

pub const CURSOR_MARKER: &'static str = "<|user_cursor_is_here|>";
/// NOTE: Differs from zed version of constant is it includes a newline.
pub const EDITABLE_REGION_START_MARKER: &'static str = "<|editable_region_start|>\n";
/// NOTE: Differs from zed version of constant is it includes a newline.
pub const EDITABLE_REGION_END_MARKER: &'static str = "<|editable_region_end|>\n";

// TODO: Cleanup this type.
pub struct PromptPlanner {
    snippets: HashMap<OutlineId, PlannedSnippet>,
    included_parents: HashSet<OutlineId>,
    budget_used: usize,
}

pub struct PlannedPrompt {
    snippets: Vec<PlannedSnippet>,
    excerpt_file: Arc<Path>,
    excerpt_ranges: ExcerptRanges,
    cursor_offset: usize,
    root_path: std::path::PathBuf,
}

#[derive(Clone, Debug)]
pub struct PlannedSnippet {
    scored_snippet: ScoredSnippet,
    style: SnippetStyle,
    budget_used: usize,
}

/// Returns the byte range of the signature for the parent outline item expanded to line boundaries.
fn parent_signature(
    identifier_index: &IdentifierIndex,
    outline_item: &OutlineItem,
) -> Range<usize> {
    // Find the source file containing this outline item
    let source = identifier_index
        .path_to_items
        .iter()
        .find_map(|(path, items)| {
            if items.iter().any(|item| item.id == outline_item.id) {
                identifier_index.path_to_source.get(path)
            } else {
                None
            }
        })
        .unwrap();

    let base_range = outline_item.signature_range.clone();

    expand_range_to_line_boundaries(source, base_range)
}

impl PromptPlanner {
    /// Greedy one-pass knapsack algorithm to populate the prompt plan. Does the following:
    ///
    /// Initializes a priority queue by populating it with each snippet, finding the SnippetStyle
    /// that minimizes `score_density = score / snippet.range(style).len()`. PriorityQueue uses
    /// (OutlineId, SnippetStyle) for keys uses the score_density as the priority.
    ///
    /// In a loop:
    ///
    /// 1. Takes the highest score_density from the queue.
    ///
    /// 2. Compute how much additional budget is needed.
    ///
    /// - If it's already included, compute the difference in budget. An error should be logged if
    /// the new amount of budget_used by the snippet is less than before.
    ///
    /// - If any of its parents are not already included, these also count towards the budget.
    ///
    /// 3. If the budget won't be exceeded, include it in the plan. Otherwise, omit it and continue
    /// the loop.
    ///
    /// - When a `Signature` item is added to the plan, a `Definition` item should be inserted into
    /// the queue. The priority for this item should be the difference in snippet score divided by
    /// the difference in snippet size.
    ///
    /// TODO: Implement an early halting condition. One option might be to have another priority
    /// queue where the score is the size, and update it accordingly. Another option might be to
    /// have some simpler heuristic like bailing after N failed insertions, or based on how much
    /// budget is left.
    pub fn populate(
        identifier_index: &IdentifierIndex,
        snippets: Vec<ScoredSnippet>,
        excerpt_file: Arc<Path>,
        excerpt_ranges: ExcerptRanges,
        cursor_offset: usize,
        byte_budget: usize,
        root_path: &Path,
    ) -> PlannedPrompt {
        let mut planner = PromptPlanner {
            snippets: HashMap::new(),
            included_parents: HashSet::new(),
            budget_used: excerpt_ranges.size(),
        };

        // Initialize priority queue with all snippet/style combinations
        //
        // TODO: consider switching priority queue implementation - this one uses more memory to
        // provide fast priority changes which is not currently used.
        let mut queue: PriorityQueue<(OutlineId, SnippetStyle), ordered_float::OrderedFloat<f32>> =
            PriorityQueue::new();

        for snippet in &snippets {
            for style in SnippetStyle::iter() {
                let score_density = snippet.score_density(identifier_index, style);
                queue.push(
                    (snippet.definition.id, style),
                    ordered_float::OrderedFloat(score_density),
                );
            }
        }

        // Knapsack selection loop
        while let Some(((outline_id, style), _score_density)) = queue.pop() {
            let snippet = snippets
                .iter()
                .find(|s| s.definition.id == outline_id)
                .unwrap();

            let current_snippet_range = snippet.line_range(identifier_index, style);
            let mut additional_budget = current_snippet_range.len();

            // Check if already included and compute budget difference
            if let Some(existing) = planner.snippets.get(&outline_id) {
                let existing_budget = existing.budget_used;
                if additional_budget <= existing_budget {
                    // Skip if new style doesn't improve budget usage
                    continue;
                }
                additional_budget -= existing_budget;
            }

            // Add budget for any missing parent signatures
            for &parent_id in &snippet.definition.parents {
                if !planner.included_parents.contains(&parent_id) {
                    if let Some(parent_item) = identifier_index.outline_id_to_item.get(&parent_id) {
                        let parent_range = parent_signature(identifier_index, parent_item);
                        additional_budget += parent_range.len();
                    }
                }
            }

            // Check if we can afford this addition
            if planner.budget_used + additional_budget > byte_budget {
                continue;
            }

            // Include the snippet
            planner.budget_used += additional_budget;

            // Mark parents as included
            for &parent_id in &snippet.definition.parents {
                planner.included_parents.insert(parent_id);
            }

            let planned_snippet = PlannedSnippet {
                scored_snippet: snippet.clone(),
                style,
                budget_used: current_snippet_range.len(),
            };

            planner.snippets.insert(outline_id, planned_snippet);

            // When a Signature item is consumed, insert the Definition item into the queue
            if style == SnippetStyle::Signature {
                let signature_range = current_snippet_range;
                let full_range = snippet.line_range(identifier_index, SnippetStyle::Definition);
                let signature_score = snippet.score(SnippetStyle::Signature);
                let full_score = snippet.score(SnippetStyle::Definition);

                let score_diff = full_score - signature_score;
                let size_diff = full_range.len() - signature_range.len();

                if size_diff > 0 {
                    let upgrade_density = score_diff / (size_diff as f32);
                    queue.push(
                        (outline_id, SnippetStyle::Definition),
                        ordered_float::OrderedFloat(upgrade_density),
                    );
                }
            }
        }

        let snippets = planner.snippets.into_values().collect();
        PlannedPrompt {
            snippets,
            excerpt_file,
            excerpt_ranges,
            cursor_offset,
            root_path: root_path.to_path_buf(),
        }
    }
}

impl PlannedPrompt {
    /// Renders the planned context to a string.
    ///
    /// Each file starts with "```FILE_PATH\n` and ends with triple backticks, with a newline after
    /// each file. When outputting nonconsecutive lines, output a line with "...". When outputting
    /// chunks which are less than 4 bytes apart, merge their ranges instead of delimiting with
    /// "...".
    pub fn to_prompt_string(&self, identifier_index: &IdentifierIndex) -> String {
        use std::collections::BTreeMap;

        // Group snippets by file
        let mut file_snippets: BTreeMap<&Arc<std::path::Path>, Vec<&PlannedSnippet>> =
            BTreeMap::new();

        for snippet in &self.snippets {
            file_snippets
                .entry(&snippet.scored_snippet.definition_file)
                .or_default()
                .push(snippet);
        }

        // Reorder so that file with cursor comes last
        let mut reordered_file_snippets = Vec::new();
        let mut excerpt_file_snippets = None;
        for (file_path, snippets) in file_snippets {
            if file_path == &self.excerpt_file {
                excerpt_file_snippets = Some(snippets);
            } else {
                reordered_file_snippets.push((file_path, snippets));
            }
        }
        reordered_file_snippets.push((&self.excerpt_file, excerpt_file_snippets.unwrap_or(vec![])));

        let mut output = String::new();

        for (file_path, mut snippets) in reordered_file_snippets {
            // Sort snippets by their start position
            snippets.sort_by_key(|s| s.scored_snippet.line_range(identifier_index, s.style).start);

            // Use root-relative path by stripping the root path prefix
            let display_path = if let Ok(relative_path) = file_path.strip_prefix(&self.root_path) {
                relative_path.display().to_string()
            } else {
                file_path.display().to_string()
            };

            let source = identifier_index.path_to_source.get(file_path).unwrap();

            let mut ranges_to_output = Vec::new();

            // Collect all ranges including parent signatures
            for snippet in &snippets {
                ranges_to_output.push(SourceRangeWithInsertions::within_source(
                    snippet
                        .scored_snippet
                        .line_range(identifier_index, snippet.style),
                    &source,
                ));

                // Add parent signatures
                for &parent_id in &snippet.scored_snippet.definition.parents {
                    if let Some(parent_item) = identifier_index.outline_id_to_item.get(&parent_id) {
                        ranges_to_output.push(SourceRangeWithInsertions::within_source(
                            parent_signature(identifier_index, parent_item),
                            &source,
                        ));
                    }
                }
            }

            if file_path == &self.excerpt_file {
                let mut insertions = Vec::new();
                insertions.push((
                    self.excerpt_ranges.excerpt_range.start,
                    EDITABLE_REGION_START_MARKER,
                ));
                insertions.push((self.cursor_offset, CURSOR_MARKER));
                let end_insertions = vec![EDITABLE_REGION_END_MARKER];
                ranges_to_output.push(SourceRangeWithInsertions {
                    range: self.excerpt_ranges.excerpt_range.clone(),
                    insertions,
                    end_insertions,
                });
                ranges_to_output.extend(
                    self.excerpt_ranges
                        .parent_signature_ranges
                        .iter()
                        .map(|range| {
                            SourceRangeWithInsertions::within_source(range.clone(), &source)
                        }),
                );
            }

            if ranges_to_output.is_empty() {
                continue;
            }

            const MERGE_THRESHOLD: usize = 4;
            output.push_str(&format!("```{}\n", display_path));
            add_ellipsis_separated_ranges(&mut output, source, ranges_to_output, MERGE_THRESHOLD);
            output.push_str("```\n\n");
        }

        output
    }
}

struct SourceRangeWithInsertions<'a> {
    range: Range<usize>,
    insertions: Vec<(usize, &'a str)>,
    end_insertions: Vec<&'a str>,
}

impl SourceRangeWithInsertions<'_> {
    fn within_source(range: Range<usize>, _source_file: &str) -> Self {
        Self {
            range,
            insertions: Vec::new(),
            end_insertions: Vec::new(),
        }
    }
}

fn add_ellipsis_separated_ranges(
    output: &mut String,
    source: &str,
    mut source_ranges: Vec<SourceRangeWithInsertions<'_>>,
    merge_threshold: usize,
) {
    source_ranges.sort_by_key(|r| r.range.start);

    let mut merged_ranges: Vec<SourceRangeWithInsertions> = Vec::new();
    for source_range in source_ranges {
        if let Some(last) = merged_ranges.last_mut()
            && source_range.range.start <= last.range.end + merge_threshold
        {
            let SourceRangeWithInsertions {
                range,
                insertions,
                end_insertions,
            } = source_range;
            let new_end = last.range.end.max(range.end);
            if last.range.end < source.len() && last.range.end < new_end {
                let end_insertion_offset = last.range.end + 1;
                // ensure that it's ok to increment the end of the range by 1
                assert!(&source[last.range.end..end_insertion_offset] == "\n");
                for insertion in &last.end_insertions {
                    last.insertions.push((end_insertion_offset, insertion));
                }
                last.end_insertions = end_insertions;
            } else {
                last.end_insertions.extend(end_insertions);
            }
            last.insertions.extend(insertions);
            last.range.end = new_end;
        } else {
            merged_ranges.push(source_range);
        }
    }

    let last_merged_range_ix = merged_ranges.len() - 1;
    for (
        i,
        SourceRangeWithInsertions {
            range,
            insertions,
            end_insertions,
        },
    ) in merged_ranges.into_iter().enumerate()
    {
        if range.start > 0 {
            output.push_str("…\n");
        }

        let mut last_pos = range.start;
        for (offset, insertion) in insertions {
            assert!(offset <= range.end);
            output.push_str(&source[last_pos..offset]);
            output.push_str(insertion);
            last_pos = offset;
        }
        output.push_str(&source[last_pos..range.end]);

        if !source[range.clone()].ends_with('\n') {
            output.push('\n');
        }

        for insertion in end_insertions {
            output.push_str(insertion);
        }

        if i == last_merged_range_ix && range.end < source.len() {
            output.push_str("…\n");
        }
    }
}
