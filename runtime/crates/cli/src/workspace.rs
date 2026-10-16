use crate::ui;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::output::{print_query, QueryOutput, QueryOutputFormat, Table};

async fn with_store<T, F>(path: &Path, operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&rivo_storage::Store) -> Result<T> + Send + 'static,
{
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let store = rivo_storage::Store::open(path)?;
        operation(&store)
    })
    .await
    .context(ui::explicit_text(
        "cli-workspace-storage-worker-interrupted",
        "本地会话存储 worker 已中断",
    ))?
}

pub(crate) async fn run_workspace_command(args: Vec<String>) -> Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", ui::text("cli-workspace-help", "Manage local workspaces.\n\nUsage: rivo workspace list|add|open|show|rename|rebind|remove\n\nOptions: -h, --help  Show help"));
        return Ok(());
    }
    let path = crate::history_database_path()?;
    let operation = args.first().map(String::as_str).unwrap_or("list");
    if operation != "list" && !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    match operation {
        "list" => {
            let format = parse_format(&args)?;
            let workspaces = if path.exists() {
                with_store(&path, |store| Ok(store.list_workspaces()?)).await?
            } else {
                Vec::new()
            };
            let mut text = String::new();
            let mut table = Table::new([
                ui::text("cli-workspace-id", "ID"),
                ui::text("cli-workspace-name", "Name"),
                ui::text("cli-workspace-path", "Path"),
                ui::text("cli-workspace-default", "Default"),
                ui::text("cli-workspace-last-opened", "Last opened"),
            ]);
            let mut json = Vec::new();
            for workspace in workspaces {
                text.push_str(&format!(
                    "{}\t{}\t{}\tdefault={}\tlast_opened_at={}\n",
                    workspace.id,
                    workspace.display_name,
                    workspace.workspace_root,
                    workspace.is_default,
                    workspace.last_opened_at
                ));
                table.push_row([
                    workspace.id.clone(),
                    workspace.display_name.clone(),
                    workspace.workspace_root.clone(),
                    workspace.is_default.to_string(),
                    workspace.last_opened_at.clone(),
                ]);
                json.push(serde_json::json!({"workspace_id": workspace.id, "display_name": workspace.display_name, "root_path": workspace.workspace_root, "is_default": workspace.is_default, "last_opened_at": workspace.last_opened_at}));
            }
            print_query(
                format,
                QueryOutput::new(text, table, serde_json::Value::Array(json)),
            )?;
        }
        "add" => {
            let (root, name) = parse_add(&args)?;
            let workspace = with_store(&path, move |store| {
                Ok(store.ensure_workspace(root, name.as_deref())?)
            })
            .await?;
            println!(
                "{}",
                ui::text_with(
                    "cli-workspace-added",
                    &format!(
                        "workspace_id={}\nname={}\npath={}",
                        workspace.id, workspace.display_name, workspace.workspace_root
                    ),
                    &[
                        ("id", workspace.id.clone()),
                        ("name", workspace.display_name.clone()),
                        ("path", workspace.workspace_root.clone()),
                    ],
                )
            );
        }
        "open" => {
            let reference = one_arg(&args, "open")?;
            let workspace = with_store(&path, move |store| {
                let workspace = if let Some(id) = store.resolve_workspace_ref(&reference)? {
                    store.get_workspace(&id)?
                } else {
                    store.ensure_workspace(PathBuf::from(&reference), None)?
                };
                store.touch_workspace(&workspace.id)?;
                Ok(store.get_workspace(&workspace.id)?)
            })
            .await?;
            println!(
                "{}",
                ui::text_with(
                    "cli-workspace-opened",
                    &format!(
                        "opened workspace {} ({})",
                        workspace.display_name, workspace.workspace_root
                    ),
                    &[
                        ("name", workspace.display_name),
                        ("path", workspace.workspace_root),
                    ],
                )
            );
        }
        "show" => {
            let reference = one_arg(&args, "show")?;
            let workspace = resolve(&path, &reference).await?;
            println!(
                "workspace_id={}\nname={}\npath={}\ndefault={}\nlast_opened_at={}",
                workspace.id,
                workspace.display_name,
                workspace.workspace_root,
                workspace.is_default,
                workspace.last_opened_at
            );
        }
        "rename" => {
            if args.len() != 3 {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-workspace-rename-usage",
                        "用法: rivo workspace rename <PATH|WORKSPACE_ID> <NAME>",
                    )
                );
            }
            let reference = args[1].clone();
            let name = args[2].clone();
            let workspace = resolve(&path, &reference).await?;
            let id = workspace.id;
            let rename_id = id.clone();
            with_store(&path, move |store| {
                store.rename_workspace(&rename_id, &name)?;
                Ok(())
            })
            .await?;
            println!(
                "{}",
                ui::text_with(
                    "cli-workspace-renamed",
                    &format!("renamed workspace {id}"),
                    &[("id", id)],
                )
            );
        }
        "rebind" => {
            if args.len() != 3 {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-workspace-rebind-usage",
                        "用法: rivo workspace rebind <WORKSPACE_ID> <PATH>",
                    )
                );
            }
            let id = args[1].clone();
            let root = PathBuf::from(&args[2]);
            let workspace =
                with_store(&path, move |store| Ok(store.rebind_workspace(&id, root)?)).await?;
            println!(
                "{}",
                ui::text_with(
                    "cli-workspace-rebound",
                    &format!(
                        "rebound workspace {} to {}",
                        workspace.id, workspace.workspace_root
                    ),
                    &[("id", workspace.id), ("path", workspace.workspace_root),],
                )
            );
        }
        "remove" => {
            if args.len() != 3 || args[2] != "--yes" {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text(
                        "cli-workspace-remove-usage",
                        "用法: rivo workspace remove <PATH|WORKSPACE_ID> --yes",
                    )
                );
            }
            let workspace = resolve(&path, &args[1]).await?;
            let id = workspace.id.clone();
            let session_ids = with_store(&path, move |store| {
                let session_ids = store.session_ids_for_workspace(&id)?;
                store.delete_workspace_cascade(&id)?;
                Ok(session_ids)
            })
            .await?;
            for session_id in session_ids {
                rivo_session::remove_media_assets(&session_id);
            }
            println!(
                "{}",
                ui::text_with(
                    "cli-workspace-removed",
                    &format!("removed workspace {} and its sessions", workspace.id),
                    &[("id", workspace.id)],
                )
            );
        }
        _ => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-workspace-usage",
                "用法: rivo workspace list|add|open|show|rename|rebind|remove",
            )
        ),
    }
    Ok(())
}

fn parse_format(args: &[String]) -> Result<QueryOutputFormat> {
    let mut format = QueryOutputFormat::Text;
    let mut index = 1;
    while index < args.len() {
        let value = &args[index];
        let format_value = if value == "--format" {
            index += 1;
            args.get(index)
                .context(ui::explicit_text(
                    "cli-workspace-format-value",
                    "--format 缺少值",
                ))?
                .as_str()
        } else if let Some(value) = value.strip_prefix("--format=") {
            if value.is_empty() {
                anyhow::bail!(
                    "{}",
                    ui::explicit_text("cli-workspace-format-value", "--format 缺少值")
                );
            }
            value
        } else {
            anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-workspace-list-arg",
                    &format!("未知 workspace list 参数: {value}"),
                    &[("value", value.to_owned())],
                )
            );
        };
        format = match format_value {
            "text" => QueryOutputFormat::Text,
            "table" => QueryOutputFormat::Table,
            "json" => QueryOutputFormat::Json,
            _ => anyhow::bail!(
                "{}",
                ui::text_with(
                    "cli-workspace-format-invalid",
                    &format!("无效输出格式: {format_value}"),
                    &[("value", format_value.to_owned())],
                )
            ),
        };
        index += 1;
    }
    Ok(format)
}

fn parse_add(args: &[String]) -> Result<(PathBuf, Option<String>)> {
    if args.len() < 2 || args.len() > 4 {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-workspace-add-usage",
                "用法: rivo workspace add <PATH> [--name <NAME>]",
            )
        );
    }
    let root = PathBuf::from(&args[1]);
    let name = match args.get(2..).unwrap_or_default() {
        [] => None,
        [flag, value] if flag == "--name" => Some(value.clone()),
        _ => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-workspace-add-usage",
                "用法: rivo workspace add <PATH> [--name <NAME>]",
            )
        ),
    };
    Ok((root, name))
}

fn one_arg(args: &[String], command: &str) -> Result<String> {
    if args.len() != 2 {
        anyhow::bail!(
            "{}",
            ui::text_with(
                "cli-workspace-one-arg-usage",
                &format!("用法: rivo workspace {command} <PATH|WORKSPACE_ID>"),
                &[("command", command.to_owned())],
            )
        );
    }
    Ok(args[1].clone())
}

async fn resolve(path: &Path, reference: &str) -> Result<rivo_storage::Workspace> {
    with_store(path, {
        let reference = reference.to_owned();
        move |store| {
            if let Some(id) = store.resolve_workspace_ref(&reference)? {
                return Ok(store.get_workspace(&id)?);
            }
            Err(rivo_storage::StorageError::NotFound(reference).into())
        }
    })
    .await
}
