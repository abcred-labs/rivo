use anyhow::Context;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::output::{load_query_format, parse_query_format, print_query, resolve_query_format};
use crate::ui;
use crate::{
    execute_headless_turn, history_database_path, ApprovalMode, HeadlessRuntime, OutputFormat,
    QueryOutput, QueryOutputFormat, Table,
};
use rivo_session::{ConfigOverrides, ResolvedConfig, Session, TurnOptions};

fn list_scope_args(args: &[String]) -> anyhow::Result<Option<rivo_storage::SessionScope>> {
    let mut scope = None;
    let mut index = 1;
    while index < args.len() {
        let value = &args[index];
        let candidate = if value == "--all-workspaces" {
            rivo_storage::SessionScope::AllWorkspaces
        } else if value == "--unscoped" {
            rivo_storage::SessionScope::Unscoped
        } else if value == "--workspace" {
            let path = args
                .get(index + 1)
                .context(ui::explicit_text(
                    "cli-sessions-workspace-value-missing",
                    "--workspace 缺少值",
                ))?
                .clone();
            index += 1;
            rivo_storage::SessionScope::CurrentWorkspace(path)
        } else if let Some(path) = value.strip_prefix("--workspace=") {
            if path.is_empty() {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text("cli-sessions-workspace-value-missing", "--workspace 缺少值")
                );
            }
            rivo_storage::SessionScope::CurrentWorkspace(path.to_owned())
        } else {
            anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-sessions-unknown-list-arg",
                    &format!("未知 sessions list 参数: {value}"),
                    &[("value", value.clone())]
                )
            );
        };
        if scope.is_some() {
            anyhow::bail!(
                "{}",
                ui::explicit_text(
                    "cli-sessions-scope-exclusive",
                    "--workspace、--all-workspaces、--unscoped 互斥"
                )
            );
        }
        scope = Some(candidate);
        index += 1;
    }
    Ok(scope)
}

fn configured_workspace() -> PathBuf {
    ResolvedConfig::load(ConfigOverrides::default())
        .map(|config| config.workspace)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

async fn with_history_store<T, F>(path: &Path, operation: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&rivo_storage::Store) -> anyhow::Result<T> + Send + 'static,
{
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let store = rivo_storage::Store::open(&path)?;
        operation(&store)
    })
    .await
    .context(ui::explicit_text(
        "cli-sessions-storage-worker-interrupted",
        "本地会话存储 worker 已中断",
    ))?
}

pub(crate) async fn run_sessions_command(args: Vec<String>) -> anyhow::Result<()> {
    if print_sessions_help(&args) {
        return Ok(());
    }
    let operation = args.first().map(String::as_str).unwrap_or("list");
    let configured = if matches!(operation, "list" | "show")
        && parse_query_format(&mut args.clone())?.is_none()
    {
        load_query_format()?
    } else {
        None
    };
    let path = history_database_path()?;
    run_sessions_command_with_format(
        &path,
        args,
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
        configured,
    )
    .await
}

/// Session maintenance operations accept an explicit database path internally,
/// keeping deterministic tests isolated from the user's data directory.
#[cfg(test)]
pub(crate) async fn run_sessions_command_at(path: &Path, args: Vec<String>) -> anyhow::Result<()> {
    run_sessions_command_at_with_terminal(
        path,
        args,
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn run_sessions_command_at_with_terminal(
    path: &Path,
    args: Vec<String>,
    stdin_is_terminal: bool,
    stderr_is_terminal: bool,
) -> anyhow::Result<()> {
    run_sessions_command_with_format(path, args, stdin_is_terminal, stderr_is_terminal, None).await
}

async fn run_sessions_command_with_format(
    path: &Path,
    args: Vec<String>,
    stdin_is_terminal: bool,
    stderr_is_terminal: bool,
    configured: Option<rivo_session::QueryFormat>,
) -> anyhow::Result<()> {
    if print_sessions_help(&args) {
        return Ok(());
    }
    let mut args = args;
    let operation = args.first().map(String::as_str).unwrap_or("list");
    let format = if matches!(operation, "list" | "show") {
        resolve_query_format(parse_query_format(&mut args)?, configured)
    } else {
        QueryOutputFormat::Text
    };
    match args.first().map(String::as_str).unwrap_or("list") {
        "path" => println!("{}", path.display()),
        "backup" => {
            let destination = match args.as_slice() {
                [_, destination] => PathBuf::from(destination),
                _ => anyhow::bail!("{}", ui::explicit_text("cli-sessions-backup-usage", "用法: rivo sessions backup <target>")),
            };
            let source = path.to_path_buf();
            let backup = tokio::task::spawn_blocking(move || rivo_storage::backup_database(source, destination))
                .await
                .context(ui::explicit_text("cli-sessions-storage-worker-interrupted", "本地会话存储 worker 已中断"))??;
            println!(
                "{}",
                ui::text_with(
                    "cli-sessions-backup-created",
                    &format!(
                        "backup created\nsource={}\ndestination={}\nschema_version={}\nsha256={}",
                        backup.source,
                        backup.destination,
                        backup.schema_version
                            .map_or_else(|| "unknown".into(), |version| version.to_string()),
                        backup.sha256
                    ),
                    &[
                        ("source", backup.source.clone()),
                        ("destination", backup.destination.clone()),
                        (
                            "schema",
                            backup
                                .schema_version
                                .map_or_else(|| "unknown".into(), |version| version.to_string()),
                        ),
                        ("sha256", backup.sha256.clone()),
                    ],
                )
            );
        }
        "reset" => {
            let backup = match args.as_slice() {
                [_, flag, destination, yes] if flag == "--backup" && yes == "--yes" => PathBuf::from(destination),
                _ => anyhow::bail!("{}", ui::explicit_text("cli-sessions-reset-usage", "用法: rivo sessions reset --backup <target> --yes")),
            };
            let source = path.to_path_buf();
            tokio::task::spawn_blocking(move || rivo_storage::reset_schema_14_sessions(source, backup))
                .await
                .context(ui::explicit_text("cli-sessions-storage-worker-interrupted", "本地会话存储 worker 已中断"))??;
            println!(
                "{}",
                ui::text(
                    "cli-sessions-reset-complete",
                    "reset complete: schema 18 session history created; the schema 14 backup is retained",
                )
            );
        }
        "migrate" => {
            match args.as_slice() {
                [_, yes] if yes == "--yes" => {
                    let source = path.to_path_buf();
                    let report = tokio::task::spawn_blocking(move || {
                        rivo_storage::auto_migrate_schema_17_workspaces(source)
                    })
                    .await
                    .context(ui::explicit_text("cli-sessions-storage-worker-interrupted", "本地会话存储 worker 已中断"))??;
                    if report.already_current {
                        println!(
                            "{}",
                            ui::text(
                                "cli-sessions-migration-current",
                                "migration not needed: schema 18 is already current",
                            )
                        );
                    } else if let Some(backup) = report.backup {
                        println!(
                            "{}",
                            ui::text_with(
                                "cli-sessions-migration-complete",
                                &format!(
                                    "migrate complete: schema 17 -> schema 18\nbackup={}\nbackup_sha256={}\nbound_sessions={}\nunscoped_sessions={}\nbackup retained",
                                    backup.destination,
                                    backup.sha256,
                                    report.bound_session_count,
                                    report.unscoped_session_count
                                ),
                                &[
                                    ("backup", backup.destination.clone()),
                                    ("sha256", backup.sha256.clone()),
                                    ("bound", report.bound_session_count.to_string()),
                                    ("unscoped", report.unscoped_session_count.to_string()),
                                ],
                            )
                        );
                    }
                }
                [_, flag, destination, yes] if flag == "--backup" && yes == "--yes" => {
                    let backup = PathBuf::from(destination);
                    let source = path.to_path_buf();
                    tokio::task::spawn_blocking(move || {
                        rivo_storage::migrate_schema_17_workspaces(source, backup)
                    })
                    .await
                    .context(ui::explicit_text("cli-sessions-storage-worker-interrupted", "本地会话存储 worker 已中断"))??;
                    println!(
                        "{}",
                        ui::text(
                            "cli-sessions-migration-complete-simple",
                            "migrate complete: schema 18 Workspace session history created; backup retained",
                        )
                    );
                }
                _ => anyhow::bail!("{}", ui::explicit_text("cli-sessions-migrate-usage", "用法: rivo sessions migrate --yes|--backup <target> --yes")),
            }
        }
        "list" => {
            let explicit_scope = list_scope_args(&args)?;
            if !path.exists() {
                if format == QueryOutputFormat::Json {
                    println!("[]");
                } else if format == QueryOutputFormat::Table {
                    print_query(
                        format,
                        QueryOutput::new(
                            String::new(),
                            Table::new([
                                "ID",
                                "Workspace ID",
                                "Workspace",
                                "Workspace Path",
                                "Created",
                                "Summary",
                                "Events",
                                "Messages",
                                "Status",
                            ]),
                            serde_json::json!([]),
                        ),
                    )?;
                }
                return Ok(());
            }
            let scope = explicit_scope.unwrap_or_else(|| rivo_storage::SessionScope::CurrentWorkspace(configured_workspace().display().to_string()));
            let (sessions, workspaces) = with_history_store(path, move |store| {
                Ok((store.list_sessions_scoped(scope)?, store.list_workspaces()?))
            }).await?;
            let mut text = String::new();
            let mut table = Table::new([
                ui::text("cli-sessions-column-id", "ID"),
                ui::text("cli-sessions-column-workspace-id", "Workspace ID"),
                ui::text("cli-sessions-column-workspace", "Workspace"),
                ui::text("cli-sessions-column-workspace-path", "Workspace Path"),
                ui::text("cli-sessions-column-created", "Created"),
                ui::text("cli-sessions-column-summary", "Summary"),
                ui::text("cli-sessions-column-events", "Events"),
                ui::text("cli-sessions-column-messages", "Messages"),
                ui::text("cli-sessions-column-status", "Status"),
            ]);
            let mut json = Vec::new();
            for session in sessions {
                let workspace = session.project_id.as_deref().and_then(|id| {
                    workspaces.iter().find(|workspace| workspace.id == id).map(|workspace| workspace.display_name.clone())
                }).unwrap_or_else(|| "unscoped".into());
                let task_summary = session
                    .task_summary
                    .clone()
                    .unwrap_or_else(|| ui::text("cli-sessions-no-persisted-task", "No persisted task"))
                    .replace(['\n', '\r', '\t'], " ");
                let availability = if session.expired {
                    let reason = session
                        .expiration_reason
                        .clone()
                        .unwrap_or_else(|| ui::text("cli-sessions-retention-policy", "local retention policy"));
                    ui::text_with(
                        "cli-sessions-expired",
                        &format!("已过期：{reason}"),
                        &[("reason", reason)],
                    )
                } else {
                    ui::text("cli-sessions-restorable", "可恢复")
                };
                let availability_value = if session.expired { "expired" } else { "restorable" };
                text.push_str(&format!(
                    "{}\tworkspace_id={} workspace={} workspace_path={}\t{}\tsummary={}\tevents={} messages={}\t{}\n",
                    session.session_id,
                    session.project_id.as_deref().unwrap_or("unscoped"),
                    workspace,
                    session.workspace_root.as_deref().unwrap_or(""),
                    session.created_at,
                    task_summary,
                    session.event_count,
                    session.message_count,
                    availability
                ));
                table.push_row(vec![
                    session.session_id.clone(),
                    session
                        .project_id
                        .clone()
                        .unwrap_or_else(|| "unscoped".into()),
                    workspace.clone(),
                    session.workspace_root.clone().unwrap_or_default(),
                    session.created_at.to_string(),
                    task_summary.clone(),
                    session.event_count.to_string(),
                    session.message_count.to_string(),
                    availability.clone(),
                ]);
                json.push(serde_json::json!({
                    "session_id": session.session_id,
                    "workspace_id": session.project_id,
                    "workspace_name": workspace,
                    "workspace_path": session.workspace_root,
                    "created_at": session.created_at,
                    "task_summary": task_summary,
                    "event_count": session.event_count,
                    "message_count": session.message_count,
                    "availability": availability_value,
                }));
            }
            print_query(format, QueryOutput::new(text, table, serde_json::Value::Array(json)))?;
        }
        "show" => {
            let id = args
                .get(1)
                .context(ui::explicit_text("cli-sessions-show-usage", "用法: rivo sessions show <session_id>"))?;
            let id = id.clone();
            let replay_id = id.clone();
            let (summary, events, messages, workspace_name) = with_history_store(path, move |store| {
                let summary = store
                    .list_sessions()?
                    .into_iter()
                    .find(|summary| summary.session_id == replay_id)
                    .ok_or_else(|| rivo_storage::StorageError::NotFound(replay_id.clone()))?;
                let workspace_name = summary.project_id.as_deref().and_then(|workspace_id| {
                    store
                        .get_workspace(workspace_id)
                        .ok()
                        .map(|workspace| workspace.display_name)
                });
                Ok((
                    summary,
                    store.replay(&replay_id)?,
                    store.replay_messages(&replay_id)?,
                    workspace_name,
                ))
            })
            .await?;
            if format == QueryOutputFormat::Text {
                println!(
                    "session_id={}\nworkspace_id={}\nworkspace_name={}\nworkspace_path={}\ncreated_at={}\nevent_count={}\nmessage_count={}",
                    summary.session_id,
                    summary.project_id.as_deref().unwrap_or("unscoped"),
                    workspace_name.as_deref().unwrap_or("unscoped"),
                    summary.workspace_root.as_deref().unwrap_or(""),
                    summary.created_at,
                    summary.event_count,
                    summary.message_count
                );
                println!("{}:", ui::text("cli-sessions-events", "events"));
                for event in events {
                    println!("{}\t{}\t{}", event.sequence, event.kind, event.body);
                }
                println!("{}:", ui::text("cli-sessions-messages", "messages"));
                for message in messages {
                    println!("{}\t{}\t{}", message.sequence, message.role, message.body);
                }
                return Ok(());
            }
            let mut table = Table::new([
                ui::text("cli-sessions-column-type", "Type"),
                ui::text("cli-sessions-column-sequence", "Sequence"),
                ui::text("cli-sessions-column-kind", "Kind"),
                ui::text("cli-sessions-column-summary", "Summary"),
            ]);
            table.push_row(vec![
                "session".into(),
                String::new(),
                format!(
                    "workspace_id={}",
                    summary.project_id.as_deref().unwrap_or("unscoped")
                ),
                format!(
                    "workspace_name={} workspace_path={}",
                    workspace_name.as_deref().unwrap_or("unscoped"),
                    summary.workspace_root.as_deref().unwrap_or("")
                ),
            ]);
            let event_rows = events
                .iter()
                .map(|event| {
                    let summary = query_summary(&event.body);
                    table.push_row(vec!["event".into(), event.sequence.to_string(), event.kind.clone(), summary.clone()]);
                    serde_json::json!({"sequence": event.sequence, "kind": event.kind, "summary": summary})
                })
                .collect::<Vec<_>>();
            let message_rows = messages
                .iter()
                .map(|message| {
                    let summary = query_summary(&message.body);
                    table.push_row(vec!["message".into(), message.sequence.to_string(), message.role.clone(), summary.clone()]);
                    serde_json::json!({"sequence": message.sequence, "role": message.role, "summary": summary})
                })
                .collect::<Vec<_>>();
            print_query(
                format,
                QueryOutput::new(
                    String::new(),
                    table,
                    serde_json::json!({
                        "session_id": id,
                        "workspace_id": summary.project_id,
                        "workspace_name": workspace_name,
                        "workspace_path": summary.workspace_root,
                        "created_at": summary.created_at,
                        "event_count": summary.event_count,
                        "message_count": summary.message_count,
                        "events": event_rows,
                        "messages": message_rows
                    }),
                ),
            )?;
        }
        "delete" => {
            let (id, force) = match args.as_slice() {
                [_, id] => (id, false),
                [_, id, flag] if flag == "--yes" => (id, true),
                _ => anyhow::bail!("{}", ui::explicit_text("cli-sessions-delete-usage", "用法: rivo sessions delete <session_id> [--yes]")),
            };
            crate::confirm_history_deletion_with_terminal(
                &ui::text_with(
                    "cli-sessions-confirm-delete",
                    &format!("删除本地会话 {id}"),
                    &[("id", id.clone())],
                ),
                force,
                stdin_is_terminal,
                stderr_is_terminal,
            )?;
            let id = id.clone();
            let deleted_id = id.clone();
            with_history_store(path, move |store| Ok(store.delete_session(&deleted_id)?)).await?;
            rivo_session::remove_media_assets(&id);
            println!(
                "{}",
                ui::text_with(
                    "cli-sessions-deleted",
                    &format!("deleted {id}"),
                    &[("id", id)],
                )
            );
        }
        "clear" => {
            let force = match args.as_slice() {
                [_] => false,
                [_, flag] if flag == "--yes" => true,
                _ => anyhow::bail!("{}", ui::explicit_text("cli-sessions-clear-usage", "用法: rivo sessions clear [--yes]")),
            };
            crate::confirm_history_deletion_with_terminal(
                &ui::text("cli-sessions-confirm-clear", "清空所有本地会话历史"),
                force,
                stdin_is_terminal,
                stderr_is_terminal,
            )?;
            if path.exists() {
                with_history_store(path, |store| Ok(store.clear_history()?)).await?;
            }
            rivo_session::remove_all_media_assets();
            println!(
                "{}",
                ui::text(
                    "cli-sessions-cleared",
                    "cleared local session history",
                )
            );
        }
        "resume" => {
            let id = args
                .get(1)
                .context(ui::explicit_text("cli-sessions-resume-usage", "用法: rivo sessions resume <session_id> (--prompt <TEXT>|--stdin)"))?;
            let (workspace_reference, prompt, stdin) =
                extract_resume_args(args.get(2..).unwrap_or_default())?;
            let text = if stdin {
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                if text.trim().is_empty() {
                    anyhow::bail!("{}", ui::explicit_text("cli-sessions-stdin-empty", "--stdin 没有收到非空 UTF-8 任务"));
                }
                text
            } else {
                let prompt = prompt.expect("parser requires --prompt or --stdin");
                if prompt.trim().is_empty() {
                    anyhow::bail!("{}", ui::explicit_text("cli-sessions-prompt-empty", "--prompt 必须提供非空任务文本"));
                }
                prompt
            };
            let workspace = if let Some(reference) = workspace_reference {
                let workspace = with_history_store(path, move |store| {
                    let id = store
                        .resolve_workspace_ref(&reference)?
                        .ok_or_else(|| rivo_storage::StorageError::NotFound(reference.clone()))?;
                    Ok(PathBuf::from(store.get_workspace(&id)?.workspace_root))
                }).await?;
                Some(workspace)
            } else {
                None
            };
            let config = ResolvedConfig::load(ConfigOverrides { workspace, ..ConfigOverrides::default() })?;
            let session = Arc::new(Session::resume(config.clone(), id.clone())?);
            let options = TurnOptions {
                capability_profile: config.capability_profile,
                policy_profile: config.policy_profile,
                ..TurnOptions::default()
            };
            let _ = execute_headless_turn(
                session,
                text,
                options,
                HeadlessRuntime {
                    format: OutputFormat::Text,
                    approval_mode: ApprovalMode::Deny,
                    manifest: None,
                    known_api_keys: config.known_api_keys(),
                    media_refs: Vec::new(),
                },
            )
            .await?;
        }
        "attach" => {
            if args.len() != 3 { anyhow::bail!("{}", ui::explicit_text("cli-sessions-attach-usage", "用法: rivo sessions attach <session_id> <PATH|WORKSPACE_ID>")); }
            let session_id = args[1].clone();
            let reference = args[2].clone();
            let attach_id = session_id.clone();
            with_history_store(path, move |store| {
                let workspace_id = store.resolve_workspace_ref(&reference)?.ok_or_else(|| rivo_storage::StorageError::NotFound(reference.clone()))?;
                Ok(store.attach_unscoped_session(&attach_id, &workspace_id)?)
            }).await?;
            println!(
                "{}",
                ui::text_with(
                    "cli-sessions-attached",
                    &format!("attached session {session_id}"),
                    &[("id", session_id)],
                )
            );
        }
        _ => anyhow::bail!("{}", ui::explicit_text("cli-sessions-usage", "用法: rivo sessions path|backup <target>|reset --backup <target> --yes|migrate --yes|migrate --backup <target> --yes|list [--workspace <path|id>|--all-workspaces|--unscoped]|show <id>|resume <id> (--prompt <TEXT>|--stdin) [--workspace <path|id>]|attach <id> <path|id>|delete <id> [--yes]|clear [--yes]")),
    }
    Ok(())
}

fn extract_resume_args(args: &[String]) -> anyhow::Result<(Option<String>, Option<String>, bool)> {
    let mut workspace = None;
    let mut prompt = None;
    let mut stdin = false;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--workspace" {
            let value = args.get(index + 1).context(ui::explicit_text(
                "cli-sessions-workspace-value-missing",
                "--workspace 缺少值",
            ))?;
            workspace = Some(value.clone());
            index += 2;
        } else if let Some(value) = args[index].strip_prefix("--workspace=") {
            if value.is_empty() {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text("cli-sessions-workspace-value-missing", "--workspace 缺少值")
                );
            }
            workspace = Some(value.to_owned());
            index += 1;
        } else if args[index] == "--prompt" {
            let value = args.get(index + 1).context(ui::explicit_text(
                "cli-sessions-prompt-value-missing",
                "--prompt 缺少值",
            ))?;
            if prompt.replace(value.clone()).is_some() {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text("cli-sessions-prompt-duplicate", "--prompt 只能指定一次")
                );
            }
            index += 2;
        } else if args[index] == "--stdin" {
            if stdin {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text("cli-sessions-stdin-duplicate", "--stdin 只能指定一次")
                );
            }
            stdin = true;
            index += 1;
        } else {
            anyhow::bail!("{}", ui::explicit_text("cli-sessions-resume-usage-workspace", "用法: rivo sessions resume <session_id> (--prompt <TEXT>|--stdin) [--workspace <path|id>]"));
        }
    }
    if stdin == prompt.is_some() {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-sessions-prompt-or-stdin",
                "必须且只能指定 --prompt 或 --stdin"
            )
        );
    }
    Ok((workspace, prompt, stdin))
}

fn query_summary(value: &str) -> String {
    const LIMIT: usize = 160;
    let normalized = value.replace(['\n', '\r', '\t'], " ");
    if normalized.chars().count() <= LIMIT {
        normalized
    } else {
        format!("{}...", normalized.chars().take(LIMIT).collect::<String>())
    }
}

pub(crate) fn print_sessions_help(args: &[String]) -> bool {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!("{}", ui::text("cli-sessions-help", "Manage local Workspace session history.\n\nUsage: rivo sessions [COMMAND]\n\nCommands: path, backup, reset, migrate, list, show, resume, attach, delete, clear\n\nOptions: -h, --help  Show help"));
        return true;
    }
    let (id, fallback) = match args {
        [help] if matches!(help.as_str(), "--help" | "-h") => (
            "cli-sessions-help",
            "Manage local Workspace session history.\n\nUsage: rivo sessions [COMMAND]\n\nCommands: path, backup, reset, migrate, list, show, resume, attach, delete, clear\n\nOptions: -h, --help  Show help",
        ),
        [command, help] if matches!(help.as_str(), "--help" | "-h") => match command.as_str() {
            "path" => ("cli-sessions-path-help", "Show the local session database path.\n\nUsage: rivo sessions path"),
            "backup" => ("cli-sessions-backup-help", "Back up the current database without modifying the source.\n\nUsage: rivo sessions backup <target>"),
            "reset" => ("cli-sessions-reset-help", "Rebuild schema 14 sessions as schema 18 after validating a backup.\n\nUsage: rivo sessions reset --backup <target> --yes"),
            "migrate" => ("cli-sessions-migrate-help", "Back up, validate and migrate schema 17 to schema 18 Workspace history.\n\nUsage: rivo sessions migrate --yes\n  rivo sessions migrate --backup <target> --yes"),
            "list" => ("cli-sessions-list-help", "List local sessions.\n\nUsage: rivo sessions list"),
            "show" => ("cli-sessions-show-help", "Show session events and messages.\n\nUsage: rivo sessions show <SESSION_ID>"),
            "resume" => ("cli-sessions-resume-help", "Resume a session in its matching Workspace and submit a new task.\n\nUsage: rivo sessions resume <SESSION_ID> (--prompt <TEXT>|--stdin) [--workspace <PATH|WORKSPACE_ID>]"),
            "attach" => ("cli-sessions-attach-help", "Explicitly attach an unscoped legacy session to a Workspace.\n\nUsage: rivo sessions attach <SESSION_ID> <PATH|WORKSPACE_ID>"),
            "delete" => ("cli-sessions-delete-help", "Delete a local session.\n\nUsage: rivo sessions delete <SESSION_ID> [--yes]"),
            "clear" => ("cli-sessions-clear-help", "Clear all local session history.\n\nUsage: rivo sessions clear [--yes]"),
            _ => return false,
        },
        _ => return false,
    };
    println!("{}", ui::text(id, fallback));
    true
}
