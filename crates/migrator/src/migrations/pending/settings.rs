use std::ops::Range;
use tree_sitter::{Query, QueryMatch};

use crate::MigrationPatterns;
use crate::patterns::{
    SETTINGS_ASSISTANT_INLINE_ALTERNATIVE_MODEL_NAMES, SETTINGS_ASSISTANT_MODEL_NAMES,
    SETTINGS_LANGUAGE_MODELS_AVAILABLE_MODEL_NAMES,
};

pub const SETTINGS_PATTERNS: MigrationPatterns = &[
    (SETTINGS_ASSISTANT_MODEL_NAMES, migrate_google_model_names),
    (
        SETTINGS_ASSISTANT_INLINE_ALTERNATIVE_MODEL_NAMES,
        migrate_google_model_names,
    ),
    (
        SETTINGS_LANGUAGE_MODELS_AVAILABLE_MODEL_NAMES,
        migrate_google_model_names,
    ),
];

fn migrate_google_model_names(
    contents: &str,
    mat: &QueryMatch,
    query: &Query,
) -> Option<(Range<usize>, String)> {
    let model_name_capture_ix = query.capture_index_for_name("model_name")?;
    let model_name_range = mat
        .nodes_for_capture_index(model_name_capture_ix)
        .next()?
        .byte_range();
    let model_name = contents.get(model_name_range.clone())?;

    let new_model_name = match model_name {
        "gemini-2.0-flash-thinking-exp" => Some("gemini-2.5-flash-preview-04-17"),
        "gemini-2.0-pro-exp" => Some("gemini-2.5-pro-preview-03-25"),
        "gemini-2.0-flash-lite-preview" => Some("gemini-2.0-flash-lite"),
        _ => None,
    };

    new_model_name.map(|new_model_name| (model_name_range, new_model_name.to_string()))
}
