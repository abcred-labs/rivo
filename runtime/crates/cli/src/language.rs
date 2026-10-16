use crate::ui;
use anyhow::Context;
use clap::{Parser, Subcommand};
use rivo_i18n::{
    InstalledLanguage, LanguageFileSource, LocalLanguageManager, LocaleId, RemoteLanguageClient,
    RemoteLanguageReceipt,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Parser)]
#[command(
    name = "rivo language",
    about = "管理 UI 语言文件",
    disable_help_flag = true,
    disable_help_subcommand = true,
    after_help = "Examples:\n  rivo language list\n  rivo language validate .\\fr-FR.ftl\n  rivo language install .\\fr-FR.ftl\n  rivo language remote-list --registry https://languages.example/index.json\n  rivo language download fr-FR --registry https://languages.example/index.json\n  rivo language update fr-FR --yes\n  rivo language rollback fr-FR --yes\n  rivo language remove fr-FR --yes"
)]
struct LanguageCli {
    #[command(subcommand)]
    command: Option<LanguageCommand>,
    /// 显示帮助。
    #[arg(
        short = 'h',
        long = "help",
        action = clap::ArgAction::Help,
        global = true
    )]
    help: Option<bool>,
}

#[derive(Debug, Subcommand)]
enum LanguageCommand {
    /// 显示用户语言文件目录。
    Path,
    /// 列出 bundled 和用户语言文件。
    List,
    /// 显示一个语言文件的 metadata。
    Show { locale: String },
    /// 只读校验一个规范命名的 .ftl 文件。
    Validate { file: PathBuf },
    /// 校验并原子安装语言文件；覆盖已有文件需要 --yes。
    Install {
        file: PathBuf,
        #[arg(long)]
        yes: bool,
    },
    /// 删除用户层语言文件，不影响 bundled fallback。
    Remove {
        locale: String,
        #[arg(long)]
        yes: bool,
    },
    /// 从显式 HTTPS registry 列出可下载语言。
    RemoteList {
        #[arg(long)]
        registry: String,
    },
    /// 从显式 HTTPS registry 下载、校验并原子安装语言。
    Download {
        locale: String,
        #[arg(long)]
        registry: String,
        #[arg(long)]
        yes: bool,
    },
    /// 使用本地 receipt 显式检查并安装语言更新。
    Update {
        locale: String,
        #[arg(long)]
        registry: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// 不联网，验证并恢复同 locale 的本地备份。
    Rollback {
        locale: String,
        #[arg(long)]
        yes: bool,
    },
}

pub async fn run(args: Vec<String>) -> anyhow::Result<i32> {
    if ui::has_explicit_locale() && args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", ui::text("cli-language-help", "Manage local UI language files.\n\nUsage: rivo language <COMMAND>\n\nCommands: path, list, show, validate, install, remove, remote-list, download, update, rollback\n\nOptions: -h, --help  Show help"));
        return Ok(0);
    }
    let command = match LanguageCli::try_parse_from(
        std::iter::once("rivo language".to_owned()).chain(args),
    ) {
        Ok(cli) => cli.command.unwrap_or(LanguageCommand::List),
        Err(error) => {
            let exit_code = error.exit_code();
            error.print().context("无法输出 language 命令帮助")?;
            return Ok(exit_code);
        }
    };
    let manager = LocalLanguageManager::from_environment()?;
    let output = match command {
        LanguageCommand::RemoteList { registry } => remote_list(&registry).await?,
        LanguageCommand::Download {
            locale,
            registry,
            yes,
        } => download(&manager, &locale, &registry, yes).await?,
        LanguageCommand::Update {
            locale,
            registry,
            yes,
        } => update(&manager, &locale, registry.as_deref(), yes).await?,
        LanguageCommand::Rollback { locale, yes } => {
            if !yes {
                anyhow::bail!(ui::text(
                    "cli-language-confirm-rollback",
                    "Rolling back a language file requires explicit --yes",
                ));
            }
            let locale = LocaleId::parse(&locale)?;
            let installed = manager.rollback(&locale)?;
            format!("rolled-back\t{}\n", language_line(&installed))
        }
        command => execute(&manager, command)?,
    };
    print!("{output}");
    Ok(0)
}

fn execute(manager: &LocalLanguageManager, command: LanguageCommand) -> anyhow::Result<String> {
    match command {
        LanguageCommand::Path => Ok(format!("{}\n", manager.user_dir().display())),
        LanguageCommand::List => {
            let mut output = String::new();
            for language in manager.list()? {
                output.push_str(&language_line(&language));
                output.push('\n');
            }
            Ok(output)
        }
        LanguageCommand::Show { locale } => {
            let locale = LocaleId::parse(&locale)?;
            Ok(format!("{}\n", language_line(&manager.show(&locale)?)))
        }
        LanguageCommand::Validate { file } => {
            let language = manager.validate_file(&file)?;
            Ok(format!("valid\t{}\n", language_line(&language)))
        }
        LanguageCommand::Install { file, yes } => {
            let candidate = manager.inspect_install(&file)?;
            let destination = candidate.path.as_ref().context(ui::text(
                "cli-language-destination-missing",
                "Language installation destination is missing",
            ))?;
            if destination.exists() && !yes {
                anyhow::bail!(ui::text_with(
                    "cli-language-confirm-install",
                    &format!(
                        "Language file already exists: {}; replacing it requires explicit --yes",
                        destination.display()
                    ),
                    &[("path", destination.display().to_string())],
                ));
            }
            let installed = manager.install(&file, yes)?;
            Ok(format!("installed\t{}\n", language_line(&installed)))
        }
        LanguageCommand::Remove { locale, yes } => {
            if !yes {
                anyhow::bail!(ui::text(
                    "cli-language-confirm-remove",
                    "Removing a language file requires explicit --yes",
                ));
            }
            let locale = LocaleId::parse(&locale)?;
            let path = manager.remove(&locale)?;
            Ok(format!("removed\t{locale}\t{}\n", path.display()))
        }
        LanguageCommand::RemoteList { .. }
        | LanguageCommand::Download { .. }
        | LanguageCommand::Update { .. }
        | LanguageCommand::Rollback { .. } => {
            anyhow::bail!(ui::text(
                "cli-language-async-command",
                "Remote language commands must use the asynchronous entry point",
            ))
        }
    }
}

async fn remote_list(registry_url: &str) -> anyhow::Result<String> {
    let client = RemoteLanguageClient::new(concat!("rivo/", env!("RIVO_RELEASE_VERSION")))?;
    let registry = client.fetch_registry(registry_url).await?;
    let mut output = String::new();
    for entry in registry.entries() {
        output.push_str(&format!(
            "{}\t{}\trevision={}\tbytes={}\n",
            entry.locale, entry.display_name, entry.revision, entry.bytes
        ));
    }
    Ok(output)
}

async fn download(
    manager: &LocalLanguageManager,
    locale: &str,
    registry_url: &str,
    replace: bool,
) -> anyhow::Result<String> {
    let locale = LocaleId::parse(locale)?;
    let destination = manager.language_path(&locale);
    if destination.exists() && !replace {
        anyhow::bail!(ui::text_with(
            "cli-language-confirm-install",
            &format!(
                "Language file already exists: {}; replacing it requires explicit --yes",
                destination.display()
            ),
            &[("path", destination.display().to_string())],
        ));
    }
    let client = RemoteLanguageClient::new(concat!("rivo/", env!("RIVO_RELEASE_VERSION")))?;
    let registry = client.fetch_registry(registry_url).await?;
    let entry = registry.find(&locale).cloned().with_context(|| {
        ui::text_with(
            "cli-language-registry-missing",
            &format!("Registry has no language {locale}"),
            &[("locale", locale.to_string())],
        )
    })?;
    let package = client.fetch_package(entry).await?;
    let receipt = RemoteLanguageReceipt::new(registry_url, package.entry(), unix_timestamp())?;
    let installed = manager.install_remote(&package, &receipt, replace)?;
    Ok(format!("downloaded\t{}\n", language_line(&installed)))
}

async fn update(
    manager: &LocalLanguageManager,
    locale: &str,
    registry_override: Option<&str>,
    confirmed: bool,
) -> anyhow::Result<String> {
    if !confirmed {
        anyhow::bail!(ui::text(
            "cli-language-confirm-update",
            "Updating a language file requires explicit --yes",
        ));
    }
    let locale = LocaleId::parse(locale)?;
    let previous = manager.receipt(&locale)?;
    let registry_url = registry_override.unwrap_or(&previous.registry_url);
    let client = RemoteLanguageClient::new(concat!("rivo/", env!("RIVO_RELEASE_VERSION")))?;
    let registry = client.fetch_registry(registry_url).await?;
    let entry = registry.find(&locale).cloned().with_context(|| {
        ui::text_with(
            "cli-language-registry-missing",
            &format!("Registry has no language {locale}"),
            &[("locale", locale.to_string())],
        )
    })?;
    if entry.revision == previous.revision && entry.sha256 == previous.sha256 {
        return Ok(format!("current\t{locale}\trevision={}\n", entry.revision));
    }
    let package = client.fetch_package(entry).await?;
    let receipt = RemoteLanguageReceipt::new(registry_url, package.entry(), unix_timestamp())?;
    let installed = manager.install_remote(&package, &receipt, true)?;
    Ok(format!("updated\t{}\n", language_line(&installed)))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn language_line(language: &InstalledLanguage) -> String {
    let locale = language
        .locale
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "invalid".to_owned());
    let display_name = language.display_name.as_deref().unwrap_or("-");
    let revision = language.revision.as_deref().unwrap_or("-");
    let source = match language.source {
        LanguageFileSource::User => "user",
        LanguageFileSource::Bundled => "bundled",
    };
    let path = language
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_owned());
    let status = language.error.as_deref().unwrap_or("valid");
    format!("{locale}\t{display_name}\trevision={revision}\tsource={source}\tpath={path}\tstatus={status}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn language_file(locale: &str) -> String {
        format!(
            "### rivo-language-schema: 1\n### locale: {locale}\n### display-name: Test {locale}\n### revision: test.1\n\ncommon-ok = OK\n"
        )
    }

    #[test]
    fn list_contains_bundled_languages() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path());
        let output = execute(&manager, LanguageCommand::List).unwrap();
        assert!(output.contains("en-US\tEnglish (United States)"));
        assert!(output.contains("zh-CN\t简体中文"));
    }

    #[test]
    fn install_requires_yes_only_for_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path().join("languages"));
        let source = directory.path().join("source.ftl");
        fs::write(&source, language_file("fr-FR")).unwrap();

        execute(
            &manager,
            LanguageCommand::Install {
                file: source.clone(),
                yes: false,
            },
        )
        .unwrap();
        let error = execute(
            &manager,
            LanguageCommand::Install {
                file: source.clone(),
                yes: false,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("--yes"));
        execute(
            &manager,
            LanguageCommand::Install {
                file: source,
                yes: true,
            },
        )
        .unwrap();
    }

    #[test]
    fn remove_requires_yes_and_keeps_bundled_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let manager = LocalLanguageManager::new(directory.path().join("languages"));
        let source = directory.path().join("source.ftl");
        fs::write(&source, language_file("zh-CN")).unwrap();
        manager.install(&source, false).unwrap();

        let locale = LocaleId::parse("zh-CN").unwrap();
        assert!(execute(
            &manager,
            LanguageCommand::Remove {
                locale: locale.to_string(),
                yes: false,
            }
        )
        .is_err());
        execute(
            &manager,
            LanguageCommand::Remove {
                locale: locale.to_string(),
                yes: true,
            },
        )
        .unwrap();
        assert_eq!(
            manager.show(&locale).unwrap().source,
            LanguageFileSource::Bundled
        );
    }
}
