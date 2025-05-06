use anyhow::{Context as _, Result};
use editor::Editor;
use fs::{Fs, MTime};
use futures::FutureExt as _;
use futures::future::Shared;
use migrator::{migrate_keymap, migrate_settings};
use settings::{KeymapFile, SettingsStore};
use util::ResultExt;
use workspace::notifications::NotifyTaskExt;

use std::sync::Arc;

use gpui::{Empty, Entity, EventEmitter, Global, Task};
use ui::prelude::*;
use workspace::item::ItemHandle;
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace};

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum MigrationType {
    Keymap,
    Settings,
}

pub struct MigrationBanner {
    /// Populated when the pane's item could have a migration.
    migration_type: Option<MigrationType>,
}

pub enum MigrationEvent {
    ContentChanged {
        migration_type: MigrationType,
        migrated: bool,
    },
}

pub struct MigrationState {
    should_migrate_keymap_task: Shared<Task<Option<(MTime, bool)>>>,
    should_migrate_settings_task: Shared<Task<Option<(MTime, bool)>>>,
}

impl EventEmitter<MigrationEvent> for MigrationState {}

impl MigrationState {
    pub fn global(cx: &mut App) -> Entity<Self> {
        match cx.try_global::<GlobalMigrationState>() {
            None => {
                let state = cx.new(|_| MigrationState {
                    should_migrate_keymap_task: todo!(),
                    should_migrate_settings_task: todo!(),
                });
                let global_state = GlobalMigrationState(state.clone());
                cx.set_global(global_state);
                state
            }
            Some(global_state) => global_state.0.clone(),
        }
    }
}

struct GlobalMigrationState(Entity<MigrationState>);

impl Global for GlobalMigrationState {}

impl MigrationBanner {
    pub fn new(_: &Workspace, cx: &mut Context<Self>) -> Self {
        if let Some(notifier) = MigrationState::try_global(cx) {
            cx.subscribe(
                &notifier,
                move |migrator_banner, _, event: &MigrationEvent, cx| {
                    migrator_banner.handle_notification(event, cx);
                },
            )
            .detach();
        }
        let fs = <dyn Fs>::global(cx);
        Self {
            migration_type: None,
            should_migrate_keymap_task: cx
                .background_spawn(should_migrate_keymap(fs.clone(), None))
                .shared(),
            should_migrate_settings_task: cx
                .background_spawn(should_migrate_settings(fs.clone(), None))
                .shared(),
        }
    }

    fn handle_notification(&mut self, event: &MigrationEvent, cx: &mut Context<Self>) {
        match event {
            MigrationEvent::ContentChanged {
                migration_type,
                migrated,
            } => {
                // todo! also replace the should migrate tasks? Need mtime though.
                if self.migration_type == Some(*migration_type) {
                    let location = if *migrated {
                        ToolbarItemLocation::Secondary
                    } else {
                        ToolbarItemLocation::Hidden
                    };
                    cx.emit(ToolbarItemEvent::ChangeLocation(location));
                    cx.notify();
                }
            }
        }
    }

    fn check_should_migrate_keymap(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Shared<Task<Option<(MTime, bool)>>> {
        let last_result = self.should_migrate_keymap_task.clone().now_or_never();
        match last_result {
            None => self.should_migrate_keymap_task.clone(),
            Some(last_result) => {
                let fs = <dyn Fs>::global(cx);
                let task = cx
                    .background_spawn(should_migrate_keymap(fs, last_result))
                    .shared();
                self.should_migrate_keymap_task = task.clone();
                task
            }
        }
    }

    fn check_should_migrate_settings(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Shared<Task<Option<(MTime, bool)>>> {
        let last_result = self.should_migrate_settings_task.clone().now_or_never();
        match last_result {
            None => self.should_migrate_settings_task.clone(),
            Some(last_result) => {
                let fs = <dyn Fs>::global(cx);
                let task = cx
                    .background_spawn(should_migrate_settings(fs, last_result))
                    .shared();
                self.should_migrate_settings_task = task.clone();
                task
            }
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for MigrationBanner {}

impl ToolbarItemView for MigrationBanner {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        cx.notify();

        self.migration_type = if let Some(target) = active_pane_item
            .and_then(|item| item.act_as::<Editor>(cx))
            .and_then(|editor| editor.update(cx, |editor, cx| editor.target_file_abs_path(cx)))
        {
            if &target == paths::keymap_file() {
                Some(MigrationType::Keymap)
            } else if &target == paths::settings_file() {
                Some(MigrationType::Settings)
            } else {
                None
            }
        } else {
            None
        };

        match self.migration_type {
            None => {}
            Some(MigrationType::Keymap) => {
                let should_migrate = self.check_should_migrate_keymap(cx);
                cx.spawn(async move |this, cx| {
                    if let Some((_, true)) = should_migrate.await {
                        this.update(cx, |this, cx| {
                            // Only show the banner if keymap still open.
                            if this.migration_type == Some(MigrationType::Keymap) {
                                cx.emit(ToolbarItemEvent::ChangeLocation(
                                    ToolbarItemLocation::Secondary,
                                ));
                                cx.notify();
                            }
                        })
                        .ok();
                    }
                })
                .detach();
            }
            Some(MigrationType::Settings) => {
                let should_migrate = self.check_should_migrate_settings(cx);
                cx.spawn(async move |this, cx| {
                    if let Some((_, true)) = should_migrate.await {
                        this.update(cx, |this, cx| {
                            // Only show the banner if settings still open.
                            if this.migration_type == Some(MigrationType::Settings) {
                                cx.emit(ToolbarItemEvent::ChangeLocation(
                                    ToolbarItemLocation::Secondary,
                                ));
                                cx.notify();
                            }
                        })
                        .ok();
                    }
                })
                .detach();
            }
        }

        ToolbarItemLocation::Hidden
    }
}

impl MigrationType {
    fn file_type(self) -> &'static str {
        match self {
            MigrationType::Keymap => "keymap",
            MigrationType::Settings => "settings",
        }
    }

    fn backup_file_name(self) -> String {
        match self {
            MigrationType::Keymap => paths::keymap_backup_file()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            MigrationType::Settings => paths::settings_backup_file()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        }
    }
}

impl Render for MigrationBanner {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(migration_type) = self.migration_type else {
            return Empty.into_any_element();
        };

        h_flex()
            .py_1()
            .pl_2()
            .pr_1()
            .flex_wrap()
            .justify_between()
            .bg(cx.theme().status().info_background.opacity(0.6))
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .overflow_hidden()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Label::new(format!(
                                    "Your {} file uses deprecated settings which can be \
                                    automatically updated. A backup will be saved to",
                                    migration_type.file_type(),
                                ))
                                .color(Color::Default),
                            )
                            .child(
                                div()
                                    .px_1()
                                    .bg(cx.theme().colors().background)
                                    .rounded_xs()
                                    .child(
                                        Label::new(migration_type.backup_file_name())
                                            .buffer_font(cx)
                                            .size(LabelSize::Small),
                                    ),
                            ),
                    ),
            )
            .child(
                Button::new("backup-and-migrate", "Backup and Update").on_click(
                    move |_, window, cx| {
                        let fs = <dyn Fs>::global(cx);
                        match migration_type {
                            MigrationType::Keymap => {
                                cx.background_spawn(write_keymap_migration(fs.clone()))
                                    .detach_and_notify_err(window, cx);
                            }
                            MigrationType::Settings => {
                                cx.background_spawn(write_settings_migration(fs.clone()))
                                    .detach_and_notify_err(window, cx);
                            }
                        }
                    },
                ),
            )
            .into_any_element()
    }
}

async fn should_migrate_keymap(
    fs: Arc<dyn Fs>,
    last_result: Option<(MTime, bool)>,
) -> Option<(MTime, bool)> {
    let metadata = fs
        .metadata(paths::keymap_file())
        .await
        .log_err()
        .flatten()?;
    let mtime = metadata.mtime;
    if let Some((last_mtime, last_should_migrate)) = last_result {
        if mtime == last_mtime {
            return Some((last_mtime, last_should_migrate));
        }
    }

    let old_text = KeymapFile::load_keymap_file(&fs).await.log_err()?;
    let should_migrate = migrate_keymap(&old_text).log_err().flatten().is_some();
    Some((mtime, should_migrate))
}

async fn should_migrate_settings(
    fs: Arc<dyn Fs>,
    last_result: Option<(MTime, bool)>,
) -> Option<(MTime, bool)> {
    let metadata = fs
        .metadata(paths::settings_file())
        .await
        .log_err()
        .flatten()?;
    let mtime = metadata.mtime;
    if let Some((last_mtime, last_should_migrate)) = last_result {
        if mtime == last_mtime {
            return Some((last_mtime, last_should_migrate));
        }
    }

    let old_text = SettingsStore::load_settings(&fs).await.log_err()?;
    let should_migrate = migrate_settings(&old_text).log_err().flatten().is_some();
    Some((mtime, should_migrate))
}

async fn write_keymap_migration(fs: Arc<dyn Fs>) -> Result<()> {
    let old_text = KeymapFile::load_keymap_file(&fs).await?;
    let Ok(Some(new_text)) = migrate_keymap(&old_text) else {
        return Ok(());
    };
    let keymap_path = paths::keymap_file().as_path();
    if fs.is_file(keymap_path).await {
        fs.atomic_write(paths::keymap_backup_file().to_path_buf(), old_text)
            .await
            .with_context(|| "Failed to create settings backup in home directory".to_string())?;
        let resolved_path = fs
            .canonicalize(keymap_path)
            .await
            .with_context(|| format!("Failed to canonicalize keymap path {:?}", keymap_path))?;
        fs.atomic_write(resolved_path.clone(), new_text)
            .await
            .with_context(|| format!("Failed to write keymap to file {:?}", resolved_path))?;
    } else {
        fs.atomic_write(keymap_path.to_path_buf(), new_text)
            .await
            .with_context(|| format!("Failed to write keymap to file {:?}", keymap_path))?;
    }
    Ok(())
}

async fn write_settings_migration(fs: Arc<dyn Fs>) -> Result<()> {
    let old_text = SettingsStore::load_settings(&fs).await?;
    let Ok(Some(new_text)) = migrate_settings(&old_text) else {
        return Ok(());
    };
    let settings_path = paths::settings_file().as_path();
    if fs.is_file(settings_path).await {
        fs.atomic_write(paths::settings_backup_file().to_path_buf(), old_text)
            .await
            .with_context(|| "Failed to create settings backup in home directory".to_string())?;
        let resolved_path = fs
            .canonicalize(settings_path)
            .await
            .with_context(|| format!("Failed to canonicalize settings path {:?}", settings_path))?;
        fs.atomic_write(resolved_path.clone(), new_text)
            .await
            .with_context(|| format!("Failed to write settings to file {:?}", resolved_path))?;
    } else {
        fs.atomic_write(settings_path.to_path_buf(), new_text)
            .await
            .with_context(|| format!("Failed to write settings to file {:?}", settings_path))?;
    }
    Ok(())
}
