use std::path::PathBuf;
use std::sync::Arc;

use fs::{Fs, MTime};
use gpui::{App, AppContext, Entity, SharedString, Subscription};
use parking_lot::Mutex;
use project::Project;
use project::Worktree;
use sum_tree::TreeMap;
use util::ResultExt;

struct RulesSet {
    project: Entity<Project>,
    rules_files: Mutex<TreeMap<Entity<Worktree>, RulesFile>>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct RulesFile {
    path: PathBuf,
    mtime: MTime,
    content: SharedString,
}

impl RulesSet {
    fn new(project: Entity<Project>, fs: Arc<dyn Fs>, cx: &mut App) -> Self {
        let rules_files = Mutex::new(TreeMap::default());

        let subscription = cx.observe_async(&project, move |project, cx| {
            let fs = fs.clone();
            let tasks = project
                .read(cx)
                .worktrees(cx)
                .filter_map(|worktree_entity| {
                    let fs = fs.clone();
                    let worktree = worktree_entity.read(cx);
                    let Some(entry) = worktree.entry_for_path(".cursorrules") else {
                        return None;
                    };
                    if !entry.is_file() {
                        return None;
                    }
                    let Some(mtime) = entry.mtime else {
                        return None;
                    };
                    let path = match worktree.absolutize(&entry.path) {
                        Ok(abs_path) => abs_path,
                        Err(err) => {
                            log::error!(
                                "Unexpected error absolutizing rules file path {:?}: {}",
                                &entry.path,
                                &err
                            );
                            return None;
                        }
                    };
                    let prev_mtime = rules_files
                        .lock()
                        .get(&worktree_entity)
                        .map(|file: &RulesFile| file.mtime);
                    if prev_mtime == Some(mtime) {
                        return None;
                    }
                    Some(cx.background_spawn(async move {
                        // todo! Do something better than logging the error?
                        let Some(content) = fs.load(&path).await.log_err() else {
                            return;
                        };
                        let rules_file = RulesFile {
                            path,
                            mtime,
                            content: content.into(),
                        };
                        rules_files
                            .lock()
                            .insert(worktree_entity.clone(), rules_file);
                    }))
                })
                .collect::<Vec<_>>();

            async move {
                futures::future::join_all(tasks).await;
            }
        });

        Self {
            project,
            rules_files: Mutex::new(TreeMap::default()),
            _subscription: subscription,
        }
    }
}
