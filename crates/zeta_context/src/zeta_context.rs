mod excerpt;
mod identifier_index;
mod identifier_references;
mod imports;
mod outline;
mod prompt;
mod similarity;
mod treesitter_util;
mod zed_code;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use ordered_float::OrderedFloat;
use serde_json::json;
use std::fmt::Display;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::{path::PathBuf, sync::Arc};

use crate::excerpt::{ExcerptOptions, ExcerptRangesInput};
use crate::identifier_index::IdentifierIndex;
use crate::identifier_references::{
    ReferenceRegion, SnippetStyle, gather_snippets, local_identifiers,
};
use crate::prompt::PromptPlanner;
use crate::treesitter_util::{
    SourceLocation, language_for_file, load_languages, offset_from_point, parse_file,
    point_from_offset, point_range_from_offset_range, range_size,
};
use crate::zed_code::{Language, Point};

#[derive(Parser, Debug)]
#[command(name = "zeta_context")]
struct ZetaContextArgs {
    #[command(subcommand)]
    command: Command,
    #[arg(long, default_value_t = FileOrStdio::Stdio)]
    log: FileOrStdio,
}

#[derive(Subcommand, Debug)]
enum Command {
    ShowIndex {
        directory: PathBuf,
    },
    NearbyReferences {
        cursor_position: SourceLocation,
        #[arg(long, default_value_t = 10)]
        context_lines: u32,
    },

    Run {
        directory: PathBuf,
        cursor_position: CursorPosition,
        #[arg(long, default_value_t = 2048)]
        prompt_limit: usize,
        #[arg(long)]
        output_scores: Option<FileOrStdio>,
        #[command(flatten)]
        excerpt_options: ExcerptOptions,
    },
}

#[derive(Clone, Debug)]
enum CursorPosition {
    Random,
    Specific(SourceLocation),
}

impl CursorPosition {
    fn to_source_location_within(
        &self,
        languages: &[Arc<Language>],
        directory: &Path,
    ) -> SourceLocation {
        match self {
            CursorPosition::Random => {
                let entries = ignore::Walk::new(directory)
                    .filter_map(|result| result.ok())
                    .filter(|entry| language_for_file(languages, entry.path()).is_some())
                    .collect::<Vec<_>>();
                let selected_entry_ix = rand::random_range(0..entries.len());
                let path = entries[selected_entry_ix].path().to_path_buf();
                let source = std::fs::read_to_string(&path).unwrap();
                let offset = rand::random_range(0..source.len());
                let point = point_from_offset(&source, offset);
                let source_location = SourceLocation { path, point };
                log::info!("Selected random cursor position: {source_location}");
                source_location
            }
            CursorPosition::Specific(location) => location.clone(),
        }
    }
}

impl Display for CursorPosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CursorPosition::Random => write!(f, "random"),
            CursorPosition::Specific(location) => write!(f, "{}", &location),
        }
    }
}

impl FromStr for CursorPosition {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "random" => Ok(CursorPosition::Random),
            _ => Ok(CursorPosition::Specific(SourceLocation::from_str(s)?)),
        }
    }
}

#[derive(Debug, Clone)]
enum FileOrStdio {
    File(PathBuf),
    Stdio,
}

impl FileOrStdio {
    #[allow(dead_code)]
    fn read_to_string(&self) -> Result<String, std::io::Error> {
        match self {
            FileOrStdio::File(path) => std::fs::read_to_string(path),
            FileOrStdio::Stdio => std::io::read_to_string(std::io::stdin()),
        }
    }

    fn write_file_or_stdout(&self) -> Result<Box<dyn Write + Send + 'static>, std::io::Error> {
        match self {
            FileOrStdio::File(path) => Ok(Box::new(std::fs::File::create(path)?)),
            FileOrStdio::Stdio => Ok(Box::new(std::io::stdout())),
        }
    }

    fn write_file_or_stderr(
        &self,
    ) -> Result<Box<dyn std::io::Write + Send + 'static>, std::io::Error> {
        match self {
            FileOrStdio::File(path) => Ok(Box::new(std::fs::File::create(path)?)),
            FileOrStdio::Stdio => Ok(Box::new(std::io::stderr())),
        }
    }
}

impl Display for FileOrStdio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileOrStdio::File(path) => write!(f, "{}", path.display()),
            FileOrStdio::Stdio => write!(f, "-"),
        }
    }
}

impl FromStr for FileOrStdio {
    type Err = <PathBuf as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "-" => Ok(Self::Stdio),
            _ => Ok(Self::File(PathBuf::from_str(s)?)),
        }
    }
}

fn main() -> Result<()> {
    let args = ZetaContextArgs::parse();
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Pipe(args.log.write_file_or_stderr()?))
        .init();
    let languages = load_languages();
    match &args.command {
        Command::ShowIndex { directory } => {
            let directory = directory.canonicalize()?;
            let index = IdentifierIndex::index_path(&languages, &directory)?;
            for ((identifier, language_name), files) in &index.identifier_to_definitions {
                println!("\n{} ({})", identifier.0, language_name.0);
                for (file, definitions) in files {
                    println!("  {:?}", file);
                    for definition in definitions {
                        println!("    {}", definition.path_string(&index));
                    }
                }
            }
            Ok(())
        }

        Command::NearbyReferences {
            cursor_position,
            context_lines,
        } => {
            let (language, source, tree) = parse_file(&languages, &cursor_position.path)?;
            let start_offset = offset_from_point(
                &source,
                Point::new(cursor_position.point.row.saturating_sub(*context_lines), 0),
            );
            let end_offset = offset_from_point(
                &source,
                Point::new(cursor_position.point.row + context_lines, 0),
            );
            let references = local_identifiers(
                ReferenceRegion::Nearby,
                &language,
                &tree,
                &source,
                start_offset..end_offset,
            );
            for reference in references {
                println!(
                    "{:?} {}",
                    point_range_from_offset_range(&source, reference.range),
                    reference.identifier.0,
                );
            }
            Ok(())
        }

        Command::Run {
            directory,
            cursor_position,
            prompt_limit,
            output_scores,
            excerpt_options,
        } => {
            let directory = directory.canonicalize()?;
            let index = IdentifierIndex::index_path(&languages, &directory)?;
            let cursor_position = cursor_position.to_source_location_within(&languages, &directory);
            let excerpt_file: Arc<Path> = cursor_position.path.as_path().into();
            let (language, source, tree) = parse_file(&languages, &excerpt_file)?;
            let cursor_offset = offset_from_point(&source, cursor_position.point);
            let Some(excerpt_ranges) = ExcerptRangesInput {
                language: &language,
                tree: &tree,
                source: &source,
                cursor_offset,
                options: excerpt_options,
            }
            .select() else {
                return Err(anyhow!("line containing cursor does not fit within window"));
            };
            let mut snippets = gather_snippets(
                &language,
                &index,
                &tree,
                &excerpt_file,
                &source,
                excerpt_ranges.clone(),
                cursor_offset,
            );
            let planned_prompt = PromptPlanner::populate(
                &index,
                snippets.clone(),
                excerpt_file,
                excerpt_ranges.clone(),
                cursor_offset,
                *prompt_limit,
                &directory,
            );
            let prompt_string = planned_prompt.to_prompt_string(&index);
            println!("{}", &prompt_string);

            if let Some(output_scores) = output_scores {
                snippets.sort_by_key(|snippet| OrderedFloat(-snippet.scores.signature));
                let writer = output_scores.write_file_or_stdout()?;
                serde_json::to_writer_pretty(
                    writer,
                    &snippets
                        .into_iter()
                        .map(|snippet| {
                            json!({
                                "file": snippet.definition_file,
                                "symbol_path": snippet.definition.path_string(&index),
                                "signature_score": snippet.scores.signature,
                                "definition_score": snippet.scores.definition,
                                "signature_score_density": snippet.score_density(&index, SnippetStyle::Signature),
                                "definition_score_density": snippet.score_density(&index, SnippetStyle::Definition),
                                "score_components": snippet.score_components
                            })
                        })
                        .collect::<Vec<_>>(),
                )?;
            }

            let actual_window_size = range_size(excerpt_ranges.excerpt_range);
            if actual_window_size > excerpt_options.window_max_bytes {
                let exceeded_amount = actual_window_size - excerpt_options.window_max_bytes;
                if exceeded_amount as f64 / excerpt_options.window_max_bytes as f64 > 0.05 {
                    log::error!("Exceeded max main excerpt size by {exceeded_amount} bytes");
                }
            }

            if prompt_string.len() > *prompt_limit {
                let exceeded_amount = prompt_string.len() - *prompt_limit;
                if exceeded_amount as f64 / *prompt_limit as f64 > 0.1 {
                    log::error!(
                        "Exceeded max prompt size of {prompt_limit} bytes by {exceeded_amount} bytes"
                    );
                }
            }

            Ok(())
        }
    }
}
