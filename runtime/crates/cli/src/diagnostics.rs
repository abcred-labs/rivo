use crate::ui;
use anyhow::{Context, Result};
use rivo_session::{redacted_config, ConfigOverrides, ResolvedConfig};
use std::path::PathBuf;

pub(crate) async fn run(args: Vec<String>) -> Result<()> {
    if matches!(args.as_slice(), [help] | [_, help] if matches!(help.as_str(), "--help" | "-h")) {
        let fallback = "创建脱敏诊断包。\n\nUsage: rivo diagnostics create <PATH> [--yes]\n\nArguments:\n  <PATH>  诊断包输出路径\n\nOptions:\n      --yes  跳过本命令的交互确认\n  -h, --help  显示帮助\n\nExamples:\n  rivo diagnostics create .\\rivo-diagnostics.zip --yes";
        println!("{}", ui::text("cli-diagnostics-help", fallback));
        return Ok(());
    }
    if args.first().map(String::as_str) != Some("create") {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-diagnostics-usage",
                "用法: rivo diagnostics create <path> [--yes]",
            )
        );
    }
    let destination = PathBuf::from(args.get(1).context(ui::explicit_text(
        "cli-diagnostics-usage",
        "用法: rivo diagnostics create <path> [--yes]",
    ))?);
    if args.len() > 3 || args.get(2).is_some_and(|value| value != "--yes") {
        anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-diagnostics-usage",
                "用法: rivo diagnostics create <path> [--yes]",
            )
        );
    }
    if args.get(2).map(String::as_str) != Some("--yes") {
        eprint!(
            "{}",
            ui::text(
                "cli-diagnostics-confirm",
                "The diagnostic bundle may contain redacted user business content. Continue export? [y/N] "
            )
        );
        std::io::Write::flush(&mut std::io::stderr())?;
        let mut confirmation = String::new();
        std::io::stdin().read_line(&mut confirmation)?;
        if !matches!(
            confirmation.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ) {
            anyhow::bail!(
                "{}",
                ui::explicit_text(
                    "cli-diagnostics-cancelled",
                    "已取消诊断导出；自动化调用请显式传入 --yes",
                )
            );
        }
    }
    let config = ResolvedConfig::load(ConfigOverrides::default())?;
    let log_files = crate::telemetry_directory()
        .and_then(|directory| std::fs::read_dir(directory).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    let summary = serde_json::to_value(redacted_config(&config))?;
    let history_path = crate::history_database_path()?;
    let storage_health = rivo_storage::inspect_database(&history_path);
    let journal = if history_path.is_file()
        && storage_health.schema_version == Some(rivo_storage::SCHEMA_VERSION)
    {
        let path = history_path.clone();
        match tokio::task::spawn_blocking(move || {
            let store = rivo_storage::Store::open(path)?;
            Ok::<_, anyhow::Error>(store.full_diagnostic_journal()?)
        })
        .await
        .context(ui::explicit_text(
            "cli-diagnostics-storage-worker-interrupted",
            "本地会话存储 worker 已中断",
        ))? {
            Ok(journal) => journal,
            Err(error) => serde_json::json!({"omitted": true, "reason": error.to_string()}),
        }
    } else if !history_path.is_file() {
        serde_json::json!({"sessions": [], "ui_audit_events": [], "diagnostics": []})
    } else {
        serde_json::json!({"omitted": true, "reason": "SQLite schema is unavailable or incompatible"})
    };
    rivo_telemetry::write_complete_diagnostic_bundle(
        &destination,
        env!("RIVO_RELEASE_VERSION"),
        std::env::consts::OS,
        &summary,
        &log_files,
        rivo_telemetry::DiagnosticPayload {
            journal: &journal,
            storage_health: &serde_json::to_value(storage_health)?,
        },
        &config.known_api_keys(),
    )?;
    println!(
        "{}",
        ui::text_with(
            "cli-diagnostics-created",
            &format!("created {}", destination.display()),
            &[("path", destination.display().to_string())],
        )
    );
    Ok(())
}
