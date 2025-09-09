use anyhow::{Result, anyhow};
use std::{
    fs,
    ops::{Range, Sub},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use tree_sitter::Tree;

use crate::zed_code::{Language, LanguageName, Point, load_queries};

pub fn load_languages() -> Vec<Arc<Language>> {
    vec![
        load_language("c", &["c"], tree_sitter_c::LANGUAGE, true),
        load_language(
            "cpp",
            &["cpp", "hpp", "cc"],
            tree_sitter_cpp::LANGUAGE,
            true,
        ),
        load_language("css", &["css"], tree_sitter_css::LANGUAGE, false),
        load_language("go", &["go"], tree_sitter_go::LANGUAGE, true),
        load_language("json", &["json"], tree_sitter_json::LANGUAGE, false),
        load_language("python", &["py"], tree_sitter_python::LANGUAGE, true),
        load_language("rust", &["rs"], tree_sitter_rust::LANGUAGE, true),
        load_language("tsx", &["tsx"], tree_sitter_typescript::LANGUAGE_TSX, true),
        load_language(
            "typescript",
            &["ts", "js"],
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            true,
        ),
        load_language("yaml", &["yaml", "yml"], tree_sitter_yaml::LANGUAGE, false),
    ]
}

pub fn load_language(
    name: &str,
    extensions: &[&str],
    ts_language: impl Into<tree_sitter::Language>,
    supports_references: bool,
) -> Arc<Language> {
    Arc::new(
        Language {
            name: LanguageName(name.into()),
            extensions: extensions
                .iter()
                .map(|&suffix| suffix.to_string())
                .collect(),
            ts_language: ts_language.into(),
            outline_config: None,
            highlights_query: None,
            supports_references,
        }
        .with_queries(load_queries(name))
        .unwrap(),
    )
}

pub fn language_for_file(languages: &[Arc<Language>], path: &Path) -> Option<Arc<Language>> {
    let extension = path.extension().and_then(|ext| ext.to_str())?;
    languages
        .into_iter()
        .find(|lang| lang.extensions.iter().any(|ext| ext == extension))
        .cloned()
}

pub fn language_for_name(
    languages: &[Arc<Language>],
    name: &LanguageName,
) -> Option<Arc<Language>> {
    languages
        .into_iter()
        .find(|lang| &lang.name == name)
        .cloned()
}

pub fn parse_file(
    languages: &[Arc<Language>],
    path: &Path,
) -> Result<(Arc<Language>, String, tree_sitter::Tree)> {
    let language = language_for_file(&languages, path)
        .ok_or_else(|| anyhow!("No language found for file {:?}", path))?;
    let source =
        fs::read_to_string(path).map_err(|e| anyhow!("Failed to read file {:?}: {}", path, e))?;
    let tree = parse_source(&language, &source);
    Ok((language, source, tree))
}

pub fn parse_source(language: &Language, source: &str) -> Tree {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.ts_language).unwrap();
    parser.parse(source, None).unwrap()
}

pub fn range_is_superset_of<T: PartialOrd>(a: &Range<T>, b: &Range<T>) -> bool {
    a.start <= b.start && a.end >= b.end
}

pub fn range_intersection<T: Ord + Clone>(a: &Range<T>, b: &Range<T>) -> Option<Range<T>> {
    let start = a.start.clone().max(b.start.clone());
    let end = a.end.clone().min(b.end.clone());
    if start < end {
        Some(Range { start, end })
    } else {
        None
    }
}

pub fn range_size<T: Sub<Output = T>>(range: Range<T>) -> T {
    range.end - range.start
}

pub fn offset_from_point(source: &str, point: Point) -> usize {
    let mut offset = 0;
    let mut current_row = 0;
    let mut current_col = 0;

    for ch in source.chars() {
        if current_row == point.row && current_col == point.column {
            return offset;
        }

        if ch == '\n' {
            if current_row == point.row {
                if current_col + 1 != point.column {
                    log::warn!(
                        "Couldn't find column {} in line {}, as the line is {} chars long",
                        point.column,
                        point.row,
                        current_col
                    );
                }
                return offset;
            }
            current_row += 1;
            current_col = 0;
        } else {
            current_col += 1;
        }

        offset += ch.len_utf8();
    }

    offset
}

pub fn point_from_offset(source: &str, offset: usize) -> Point {
    let mut current_offset = 0;
    let mut current_row = 0;
    let mut current_col = 0;

    for ch in source.chars() {
        if current_offset >= offset {
            return Point::new(current_row, current_col);
        }

        if ch == '\n' {
            current_row += 1;
            current_col = 0;
        } else {
            current_col += 1;
        }

        current_offset += ch.len_utf8();
    }

    Point::new(current_row, current_col)
}

pub fn point_range_from_offset_range(source: &str, range: Range<usize>) -> Range<Point> {
    let start = point_from_offset(source, range.start);
    let end = point_from_offset(source, range.end);
    start..end
}

/// Expands the range to begin and end at line boundaries.
pub fn expand_range_to_line_boundaries(source: &str, range: Range<usize>) -> Range<usize> {
    let start = line_start_from_offset(source, range.start);
    let end = line_end_from_offset(source, range.end);
    start..end
}

/// Finds the offset of the start of the line containing the given offset.
pub fn line_start_from_offset(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map(|pos| pos + 1).unwrap_or(0)
}

/// Finds the offset of the newline at the end of the line containing the given offset.
pub fn line_end_from_offset(source: &str, offset: usize) -> usize {
    source[offset..]
        .find('\n')
        .map(|pos| offset + pos)
        .unwrap_or(source.len())
}

/// Finds the byte length of the line containing the given row.
pub fn line_len(source: &str, row: u32) -> usize {
    let mut current_row = 0;
    let mut line_start = 0;

    // Find the start of the target row
    for (i, ch) in source.char_indices() {
        if current_row == row {
            line_start = i;
            break;
        }
        if ch == '\n' {
            current_row += 1;
            line_start = i + 1;
        }
    }

    // If we didn't find the row, return 0
    if current_row != row {
        return 0;
    }

    // Find the end of the line (next newline or end of string)
    let line_end = source[line_start..]
        .find('\n')
        .map(|pos| line_start + pos)
        .unwrap_or(source.len());

    line_end - line_start
}

#[derive(Debug, Clone)]
pub struct SourceLocation {
    pub path: PathBuf,
    pub point: Point,
}

impl std::fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.path.display(), self.point)
    }
}

impl FromStr for SourceLocation {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 {
            return Err(anyhow!(
                "Invalid source location format. Expected 'file.rs:line:column', got '{}'",
                s
            ));
        }

        let path = PathBuf::from(parts[0]);
        let line: u32 = parts[1]
            .parse()
            .map_err(|_| anyhow!("Invalid line number: '{}'", parts[1]))?;
        let column: u32 = parts[2]
            .parse()
            .map_err(|_| anyhow!("Invalid column number: '{}'", parts[2]))?;

        // Convert from 1-based to 0-based indexing
        let point = Point::new(line.saturating_sub(1), column.saturating_sub(1));

        Ok(SourceLocation { path, point })
    }
}
