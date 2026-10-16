use crate::ui;
use anyhow::{Context, Result};
use rivo_instance_control::{process_is_running, registry_path, InstanceRegistry, SkillRegistry};
use std::path::PathBuf;

pub(crate) fn run(args: Vec<String>) -> Result<()> {
    let help = match args.as_slice() {
        [help] if matches!(help.as_str(), "--help" | "-h") => Some(ui::text(
            "cli-instance-help",
            "Inspect local Rivo instances.\n\nUsage: rivo instance [COMMAND]\n\nCommands: list, endpoint, skill\n\nOptions: -h, --help  Show help",
        )),
        [command, help] if matches!(help.as_str(), "--help" | "-h") => match command.as_str() {
            "list" => Some(ui::text(
                "cli-instance-list-help",
                "List registered local instances as JSON.\n\nUsage: rivo instance list",
            )),
            "endpoint" => Some(ui::text(
                "cli-instance-endpoint-help",
                "Show the local instance-control endpoint.\n\nUsage: rivo instance endpoint",
            )),
            "skill" => Some(ui::text(
                "cli-instance-skill-help",
                "Show the built-in process-control Skill.\n\nUsage: rivo instance skill",
            )),
            _ => None,
        },
        _ => None,
    };
    if let Some(help) = help {
        println!("{}", help);
        return Ok(());
    }
    let root = std::env::var_os("RIVO_DATA_DIR")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .context(ui::text(
            "cli-instance-data-directory-error",
            "Could not determine the instance data directory",
        ))?;
    let registry = InstanceRegistry::new(registry_path(&root));
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => {
            let mut instances = registry.prune(process_is_running)?;
            instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
            println!(
                "{}",
                serde_json::to_string(&rivo_instance_control::InstanceListResponse { instances })?
            );
        }
        "endpoint" => println!("{}", rivo_instance_control::local_endpoint(&root).display()),
        "skill" => {
            let registry = SkillRegistry::load_defaults();
            let skill = registry.get("process-control").ok_or_else(|| {
                anyhow::anyhow!(ui::text(
                    "cli-instance-skill-missing",
                    "Default process-control Skill was not loaded"
                ))
            })?;
            println!("{}", serde_json::to_string(skill)?);
        }
        _ => anyhow::bail!(
            "{}",
            ui::explicit_text(
                "cli-instance-usage",
                "用法: rivo instance [list|endpoint|skill]",
            )
        ),
    }
    Ok(())
}
