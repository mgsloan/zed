use once_cell::sync::Lazy;
use std::{ops::Range, sync::Mutex};

use tree_sitter::{QueryCursor, QueryMatch, StreamingIterator, Tree};

use crate::{
    identifier_index::IdentifierIndex,
    treesitter_util::line_len,
    zed_code::{Language, OutlineConfig},
};

static NEXT_OUTLINE_ID: Lazy<Mutex<OutlineId>> = Lazy::new(|| Mutex::new(OutlineId(0)));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OutlineId(u32);

impl OutlineId {
    pub fn new_unique() -> Self {
        let mut next_id = NEXT_OUTLINE_ID.lock().unwrap();
        let id = *next_id;
        next_id.0 += 1;
        id
    }
}

pub enum OutlineQueryResult {
    Item(OutlineItem),
    Annotation(#[allow(dead_code)] Range<usize>),
}

#[derive(Debug, Clone)]
pub struct OutlineItem {
    pub id: OutlineId,
    pub parents: Vec<OutlineId>,
    pub name_range: Range<usize>,
    pub item_range: Range<usize>,
    pub signature_range: Range<usize>,
    pub concise_text: String,
}

impl OutlineItem {
    pub fn name<'a>(&self, source: &'a str) -> &'a str {
        &source[self.name_range.clone()]
    }

    pub fn item<'a>(&self, source: &'a str) -> &'a str {
        &source[self.item_range.clone()]
    }

    pub fn signature<'a>(&self, source: &'a str) -> &'a str {
        &source[self.signature_range.clone()]
    }

    pub fn path_string(&self, index: &IdentifierIndex) -> String {
        if self.parents.is_empty() {
            format!("{}", self.concise_text)
        } else {
            format!(
                "{} • {}",
                self.parents
                    .iter()
                    .map(|parent_id| index
                        .outline_id_to_item
                        .get(parent_id)
                        .unwrap()
                        .concise_text
                        .clone())
                    .collect::<Vec<_>>()
                    .join(" • "),
                self.concise_text,
            )
        }
    }
}

impl OutlineQueryResult {
    pub fn from_match(
        source: &str,
        query_match: &QueryMatch,
        config: &OutlineConfig,
    ) -> Option<Self> {
        let mut annotation_range = None;
        let mut name_range = None;
        let mut item_range = None;
        let mut signature_start = None;
        let mut signature_end = None;
        let mut concise_text = String::new();
        let mut last_concise_text_end = None;

        for capture in query_match.captures {
            let mut included_in_signature = false;
            let mut included_in_concise_text = false;
            if Some(capture.index) == config.annotation_capture_ix {
                annotation_range = Some(capture.node.byte_range());
            } else if capture.index == config.name_capture_ix {
                name_range = Some(capture.node.byte_range());
                included_in_signature = true;
                included_in_concise_text = true;
            } else if Some(capture.index) == config.context_capture_ix
                || Some(capture.index) == config.extra_context_capture_ix
            {
                included_in_signature = true;
                included_in_concise_text = true;
            } else if Some(capture.index) == config.signature_capture_ix {
                included_in_signature = true;
            } else if capture.index == config.item_capture_ix {
                item_range = Some(capture.node.byte_range());
            }

            if included_in_signature {
                if signature_start.is_none() {
                    signature_start = Some(capture.node.start_byte());
                }
                signature_end = Some(capture.node.end_byte());
            }
            if included_in_concise_text {
                let mut range = capture.node.start_byte()..capture.node.end_byte();
                let start = capture.node.start_position();
                // avoid including newlines (logic copied from Zed)
                if capture.node.end_position().row > start.row {
                    range.end =
                        range.start + line_len(source, start.row as u32) as usize - start.column;
                }
                if range.start < range.end {
                    if let Some(last_concise_text_end) = last_concise_text_end
                        && range.start > last_concise_text_end
                    {
                        concise_text.push(' ');
                    }
                    last_concise_text_end = Some(range.end);
                    concise_text.push_str(&source[range]);
                }
            }
        }

        if let Some(annotation_range) = annotation_range {
            Some(OutlineQueryResult::Annotation(annotation_range))
        } else {
            Some(OutlineQueryResult::Item(OutlineItem {
                id: OutlineId::new_unique(),
                // populated in query_outline_items
                parents: Vec::new(),
                name_range: name_range?,
                item_range: item_range?,
                signature_range: signature_start
                    .zip(signature_end)
                    .map(|(start, end)| start..end)?,
                concise_text,
            }))
        }
    }
}

pub fn query_outline_items(language: &Language, tree: &Tree, source: &str) -> Vec<OutlineItem> {
    let Some(outline_config) = language.outline_config.as_ref() else {
        return Vec::new();
    };

    let mut query_cursor = QueryCursor::new();
    let mut matches =
        query_cursor.matches(&outline_config.query, tree.root_node(), source.as_bytes());

    let mut outline_items = Vec::new();
    while let Some(query_match) = matches.next() {
        let Some(outline_query_result) =
            OutlineQueryResult::from_match(source, query_match, &outline_config)
        else {
            log::error!("Outline query parse failed expectations: {:?}", query_match);
            continue;
        };
        if let OutlineQueryResult::Item(outline_item) = outline_query_result {
            outline_items.push(outline_item)
        }
    }

    outline_items.sort_by_key(|item| item.item_range.start);

    let mut parent_stack: Vec<(OutlineId, Range<usize>)> = Vec::new();
    for i in 0..outline_items.len() {
        let current_item_range = outline_items[i].item_range.clone();
        let current_item_id = outline_items[i].id;

        while let Some(&(_, ref parent_range)) = parent_stack.last() {
            if current_item_range.start >= parent_range.end {
                parent_stack.pop();
            } else {
                break;
            }
        }

        outline_items[i].parents = parent_stack.iter().map(|(id, _)| *id).collect();
        parent_stack.push((current_item_id, current_item_range));
    }

    outline_items
}
