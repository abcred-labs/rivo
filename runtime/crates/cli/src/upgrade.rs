use crate::ui;
use anyhow::{bail, Context};
use reqwest::Client;
use serde::Deserialize;
use std::cmp::Ordering;

const DEFAULT_RELEASE_BASE_URL: &str =
    "https://github.com/abcred-labs/rivo/releases/latest/download";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub platform: &'static str,
    pub arch: &'static str,
}

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    schema: u32,
    product: String,
    version: String,
    platform: String,
    arch: String,
}

pub fn target() -> anyhow::Result<Target> {
    let platform = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        other => bail!(
            "{}",
            ui::text_with(
                "cli-upgrade-platform-unsupported",
                &format!("rivo upgrade does not support platform: {other}"),
                &[("platform", other.to_owned())]
            )
        ),
    };
    let arch = match (platform, std::env::consts::ARCH) {
        (_, "x86_64") => "amd64",
        ("linux" | "macos", "aarch64") => "arm64",
        (_, other) => bail!(
            "{}",
            ui::text_with(
                "cli-upgrade-arch-unsupported",
                &format!("rivo upgrade does not support architecture: {other}"),
                &[("arch", other.to_owned())]
            )
        ),
    };
    Ok(Target { platform, arch })
}

pub async fn run() -> anyhow::Result<()> {
    let target = target()?;
    let current = env!("RIVO_RELEASE_VERSION");
    let base_url = std::env::var("RIVO_RELEASE_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_RELEASE_BASE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned();
    let client = Client::builder()
        .user_agent(format!("rivo/{current}"))
        .build()
        .context(ui::text(
            "cli-upgrade-http-client-failed",
            "Failed to create the upgrade HTTP client",
        ))?;

    let manifest_url = if base_url == DEFAULT_RELEASE_BASE_URL {
        format!(
            "{base_url}/release-manifest-{}-{}.json",
            target.platform, target.arch
        )
    } else {
        let directory_url = format!("{base_url}/{}-{}", target.platform, target.arch);
        format!("{directory_url}/release-manifest.json")
    };
    let manifest: ReleaseManifest =
        fetch_json(&client, &manifest_url).await.with_context(|| {
            ui::text_with(
                "cli-upgrade-manifest-read-failed",
                &format!("Failed to read latest manifest: {manifest_url}"),
                &[("url", manifest_url.clone())],
            )
        })?;
    validate_manifest(&manifest, target)?;

    println!(
        "{}",
        ui::text_with(
            "cli-upgrade-current-version",
            &format!("Current version: {current}"),
            &[("version", current.to_owned())],
        )
    );
    println!(
        "{}",
        ui::text_with(
            "cli-upgrade-latest-version",
            &format!("Latest version: {}", manifest.version),
            &[("version", manifest.version.clone())],
        )
    );
    match compare_versions(current, &manifest.version)? {
        Ordering::Equal => {
            println!(
                "{}",
                ui::text("cli-upgrade-current", "rivo is already up to date.")
            );
            Ok(())
        }
        Ordering::Greater => {
            bail!(ui::text_with(
                "cli-upgrade-downgrade",
                &format!(
                    "Remote version {} is lower than current version {}; downgrade rejected",
                    manifest.version, current
                ),
                &[
                    ("remote", manifest.version.clone()),
                    ("current", current.to_owned()),
                ],
            ));
        }
        Ordering::Less => {
            println!(
                "{}",
                ui::text("cli-upgrade-available", "An update is available.")
            );
            Ok(())
        }
    }
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(client: &Client, url: &str) -> anyhow::Result<T> {
    let response = client.get(url).send().await.context(ui::text(
        "cli-upgrade-http-request-failed",
        "HTTP request failed",
    ))?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {status} from {url}");
    }
    response.json().await.context(ui::text(
        "cli-upgrade-json-parse-failed",
        "Failed to parse the JSON response",
    ))
}

fn validate_manifest(manifest: &ReleaseManifest, target: Target) -> anyhow::Result<()> {
    if !matches!(manifest.schema, 1 | 2) {
        bail!(
            "{}",
            ui::text_with(
                "cli-upgrade-schema-unsupported",
                &format!("Unsupported release manifest schema: {}", manifest.schema),
                &[("schema", manifest.schema.to_string())]
            )
        );
    }
    if manifest.product != "rivo" {
        bail!(
            "{}",
            ui::text_with(
                "cli-upgrade-product-mismatch",
                &format!(
                    "Release manifest product does not match: {}",
                    manifest.product
                ),
                &[("product", manifest.product.clone())]
            )
        );
    }
    if manifest.platform != target.platform || manifest.arch != target.arch {
        bail!(
            "{}",
            ui::text_with(
                "cli-upgrade-target-mismatch",
                &format!(
                    "Release manifest target does not match: {}/{}; current is {}/{}",
                    manifest.platform, manifest.arch, target.platform, target.arch
                ),
                &[
                    ("manifest_platform", manifest.platform.clone()),
                    ("manifest_arch", manifest.arch.clone()),
                    ("platform", target.platform.to_owned()),
                    ("arch", target.arch.to_owned())
                ]
            )
        );
    }
    validate_version(&manifest.version)?;
    Ok(())
}

fn validate_version(value: &str) -> anyhow::Result<()> {
    let mut core = value.split(['-', '+']);
    let numbers = core
        .next()
        .unwrap_or_default()
        .split('.')
        .collect::<Vec<_>>();
    if numbers.len() != 3
        || numbers
            .iter()
            .any(|part| part.is_empty() || part.parse::<u64>().is_err())
        || value.starts_with('v')
        || value.chars().any(char::is_whitespace)
    {
        bail!(
            "{}",
            ui::text_with(
                "cli-upgrade-semver-invalid",
                &format!("Version is not a supported SemVer: {value}"),
                &[("value", value.to_owned())]
            )
        );
    }
    if value.split(['.', '-', '+']).any(str::is_empty) {
        bail!(
            "{}",
            ui::text_with(
                "cli-upgrade-semver-invalid",
                &format!("Version is not a supported SemVer: {value}"),
                &[("value", value.to_owned())]
            )
        );
    }
    Ok(())
}

fn compare_versions(left: &str, right: &str) -> anyhow::Result<Ordering> {
    validate_version(left)?;
    validate_version(right)?;
    let core = |value: &str| {
        value
            .split(['-', '+'])
            .next()
            .unwrap()
            .split('.')
            .map(|part| part.parse::<u64>().unwrap())
            .collect::<Vec<_>>()
    };
    let ordering = core(left).cmp(&core(right));
    if ordering != Ordering::Equal {
        return Ok(ordering);
    }
    Ok(match (prerelease(left), prerelease(right)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => compare_prerelease(left, right),
    })
}

fn prerelease(value: &str) -> Option<&str> {
    value
        .split_once('-')
        .and_then(|(_, suffix)| suffix.split('+').next())
}

fn compare_prerelease(left: &str, right: &str) -> Ordering {
    let left_parts = left.split('.');
    let right_parts = right.split('.');
    for (left_part, right_part) in left_parts.clone().zip(right_parts.clone()) {
        let ordering = match (left_part.parse::<u64>(), right_part.parse::<u64>()) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => left_part.cmp(right_part),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left_parts.count().cmp(&right_parts.count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_without_downgrade() {
        assert_eq!(compare_versions("0.1.0", "0.2.0").unwrap(), Ordering::Less);
        assert_eq!(compare_versions("0.1.0", "0.1.0").unwrap(), Ordering::Equal);
        assert_eq!(
            compare_versions("0.1.0", "0.0.9").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.0.0-beta.1", "1.0.0").unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("1.0.0-beta.10", "1.0.0-beta.2").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.0.0-alpha", "1.0.0-alpha.1").unwrap(),
            Ordering::Less
        );
    }
}
