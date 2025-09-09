use anyhow::{Context as _, Result};
use rust_embed::RustEmbed;
use std::fmt::Debug;
use std::{borrow::Cow, sync::Arc};
use tree_sitter::{self, Query};

/// A zero-indexed point in a text buffer consisting of a row and column.
#[derive(Clone, Copy, Default, Eq, PartialEq, Hash)]
pub struct Point {
    pub row: u32,
    pub column: u32,
}

impl Point {
    pub fn new(row: u32, column: u32) -> Self {
        Point { row, column }
    }
}

impl Debug for Point {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.row + 1, self.column + 1)
    }
}

impl std::fmt::Display for Point {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.row + 1, self.column + 1)
    }
}

pub struct OutlineConfig {
    pub query: Query,
    pub item_capture_ix: u32,
    pub name_capture_ix: u32,
    pub context_capture_ix: Option<u32>,
    pub extra_context_capture_ix: Option<u32>,
    pub annotation_capture_ix: Option<u32>,
    pub signature_capture_ix: Option<u32>,
}

pub struct Language {
    pub name: LanguageName,
    pub extensions: Vec<String>,
    pub ts_language: tree_sitter::Language,
    pub outline_config: Option<OutlineConfig>,
    pub highlights_query: Option<Query>,
    pub supports_references: bool,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct LanguageName(pub Arc<str>);

impl Language {
    pub fn with_queries(mut self, queries: LanguageQueries) -> Result<Self> {
        if let Some(query) = queries.outline {
            self = self
                .with_outline_query(query.as_ref())
                .context("Error loading outline query")?;
        }
        if let Some(query) = queries.highlights {
            self.highlights_query = Some(
                Query::new(&self.ts_language, &query).context("Error loading highlights query")?,
            );
        }
        Ok(self)
    }

    pub fn with_outline_query(mut self, source: &str) -> Result<Self> {
        let query = Query::new(&self.ts_language, source)?;
        let mut item_capture_ix = None;
        let mut name_capture_ix = None;
        let mut context_capture_ix = None;
        let mut extra_context_capture_ix = None;
        let mut annotation_capture_ix = None;
        let mut signature_capture_ix = None;
        get_capture_indices(
            &query,
            &mut [
                ("item", &mut item_capture_ix),
                ("name", &mut name_capture_ix),
                ("context", &mut context_capture_ix),
                ("context.extra", &mut extra_context_capture_ix),
                ("annotation", &mut annotation_capture_ix),
                ("signature", &mut signature_capture_ix),
            ],
        );
        if let Some((item_capture_ix, name_capture_ix)) = item_capture_ix.zip(name_capture_ix) {
            self.outline_config = Some(OutlineConfig {
                query,
                item_capture_ix,
                name_capture_ix,
                context_capture_ix,
                extra_context_capture_ix,
                annotation_capture_ix,
                signature_capture_ix,
            });
        }
        Ok(self)
    }

    pub fn capture_id_for_highlight(&self, name: &str) -> Option<u32> {
        self.highlights_query.as_ref()?.capture_index_for_name(name)
    }
}

fn get_capture_indices(query: &Query, captures: &mut [(&str, &mut Option<u32>)]) {
    for (ix, name) in query.capture_names().iter().enumerate() {
        for (capture_name, index) in captures.iter_mut() {
            if capture_name == name {
                **index = Some(ix as u32);
                break;
            }
        }
    }
}

pub const QUERY_FILENAME_PREFIXES: &[(
    &str,
    fn(&mut LanguageQueries) -> &mut Option<Cow<'static, str>>,
)] = &[
    ("outline", |q| &mut q.outline),
    ("highlights", |q| &mut q.highlights),
];

/// Tree-sitter language queries for a given language.
#[derive(Debug, Default)]
pub struct LanguageQueries {
    pub outline: Option<Cow<'static, str>>,
    pub highlights: Option<Cow<'static, str>>,
}

#[derive(RustEmbed)]
#[folder = "languages"]
struct LanguageDir;

pub fn load_queries(name: &str) -> LanguageQueries {
    let mut result = LanguageQueries::default();
    for path in LanguageDir::iter() {
        if let Some(remainder) = path.strip_prefix(name).and_then(|p| p.strip_prefix('/')) {
            if !remainder.ends_with(".scm") {
                continue;
            }
            for (name, query) in QUERY_FILENAME_PREFIXES {
                if remainder.starts_with(name) {
                    let contents = asset_str::<LanguageDir>(path.as_ref());
                    match query(&mut result) {
                        None => *query(&mut result) = Some(contents),
                        Some(r) => r.to_mut().push_str(contents.as_ref()),
                    }
                }
            }
        }
    }
    result
}

/// Get an embedded file as a string.
pub fn asset_str<A: rust_embed::RustEmbed>(path: &str) -> Cow<'static, str> {
    match A::get(path).expect(path).data {
        Cow::Borrowed(bytes) => Cow::Borrowed(std::str::from_utf8(bytes).unwrap()),
        Cow::Owned(bytes) => Cow::Owned(String::from_utf8(bytes).unwrap()),
    }
}
