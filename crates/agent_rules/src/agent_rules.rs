use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::{App, AppContext, Task};
use project::ProjectPath;
use prompt_store::SystemPromptRulesFile;
use util::maybe;
use worktree::Worktree;

const RULES_FILE_NAMES: [&'static str; 6] = [
    ".rules",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    ".github/copilot-instructions.md",
    "CLAUDE.md",
];

pub enum WhenUsed {
    WithinSubdirectory(PathBuf),
}

pub fn rules_files_for_path(worktree: &Worktree, path: Arc<Path>) -> Vec<(ProjectPath, WhenUsed)> {
    if worktree.is_single_file() {
        return vec![];
    }

    let start_directory = if let Some(path_entry) = worktree.entry_for_path(&path) {
        if path_entry.is_dir() {
            &path
        } else {
            if let Some(parent) = path.parent() {
                parent
            } else {
                return vec![];
            }
        }
    } else {
        return vec![];
    };

    start_directory
        .ancestors()
        .flat_map(|ancestor| {
            RULES_FILE_NAMES
                .into_iter()
                .filter_map(|name| {
                    worktree
                        .entry_for_path(ancestor.join(name))
                        .filter(|entry| entry.is_file())
                })
                .next()
                .map(|entry| {
                    (
                        ProjectPath {
                            worktree_id: worktree.id(),
                            path: entry.path.clone(),
                        },
                        WhenUsed::WithinSubdirectory(ancestor.to_path_buf()),
                    )
                })
        })
        .collect::<Vec<_>>()
}

pub fn load_worktree_rules_file(
    fs: Arc<dyn Fs>,
    worktree: &Worktree,
    cx: &App,
) -> Option<Task<Result<SystemPromptRulesFile>>> {
    let selected_rules_file = RULES_FILE_NAMES
        .into_iter()
        .filter_map(|name| {
            worktree
                .entry_for_path(name)
                .filter(|entry| entry.is_file())
        })
        .next()
        .map(|entry| (entry.path.clone(), worktree.absolutize(&entry.path)));

    // Note that Cline supports `.clinerules` being a directory, but that is not currently
    // supported. This doesn't seem to occur often in GitHub repositories.
    selected_rules_file.map(|(path_in_worktree, abs_path)| {
        let fs = fs.clone();
        cx.background_spawn(maybe!(async move {
            let abs_path = abs_path?;
            let text = fs
                .load(&abs_path)
                .await
                .with_context(|| format!("Failed to load assistant rules file {:?}", abs_path))?;
            anyhow::Ok(SystemPromptRulesFile {
                path_in_worktree,
                abs_path: abs_path.into(),
                text: text.trim().to_string(),
            })
        }))
    })
}
