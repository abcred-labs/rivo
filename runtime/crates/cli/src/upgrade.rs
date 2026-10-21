use crate::ui;
use anyhow::{bail, Context};
use fs2::FileExt;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const DEFAULT_RELEASE_BASE_URL: &str =
    "https://github.com/abcred-labs/rivo/releases/latest/download";
const DEFAULT_RELEASE_DOWNLOAD_ROOT: &str = "https://github.com/abcred-labs/rivo/releases/download";
const GITHUB_ORIGIN: &str = "https://github.com";
const RELEASE_ASSETS_ORIGIN: &str = "https://release-assets.githubusercontent.com";
const MAX_REDIRECTS: usize = 5;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Optional download mirrors for networks with slow or blocked GitHub access.
/// Only the transport is redirected: the manifest, artifact names and SHA-256
/// checks are unchanged, so a tampered mirror response is rejected.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Mirrors {
    github: Option<String>,
    release_assets: Option<String>,
}

impl Mirrors {
    fn from_env() -> anyhow::Result<Self> {
        Self::parse(
            std::env::var("RIVO_GITHUB_MIRROR").ok().as_deref(),
            std::env::var("RIVO_RELEASE_ASSETS_MIRROR").ok().as_deref(),
        )
    }

    fn parse(github: Option<&str>, release_assets: Option<&str>) -> anyhow::Result<Self> {
        Ok(Self {
            github: normalize_mirror("RIVO_GITHUB_MIRROR", github)?,
            release_assets: normalize_mirror("RIVO_RELEASE_ASSETS_MIRROR", release_assets)?,
        })
    }

    fn rewrite(&self, url: &str) -> String {
        for (origin, mirror) in [
            (GITHUB_ORIGIN, &self.github),
            (RELEASE_ASSETS_ORIGIN, &self.release_assets),
        ] {
            if let (Some(mirror), Some(rest)) = (mirror, url.strip_prefix(origin)) {
                if rest.starts_with('/') {
                    return format!("{mirror}{rest}");
                }
            }
        }
        url.to_owned()
    }
}

fn normalize_mirror(name: &str, value: Option<&str>) -> anyhow::Result<Option<String>> {
    let value = value
        .map(|item| item.trim().trim_end_matches('/'))
        .unwrap_or("");
    if value.is_empty() {
        return Ok(None);
    }
    let valid = value.strip_prefix("https://").is_some_and(|rest| {
        !rest.is_empty()
            && !rest.starts_with('/')
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:/_~".contains(c))
    });
    if !valid {
        bail!("{name} must be an https:// URL");
    }
    Ok(Some(value.to_owned()))
}

struct Http {
    client: Client,
    mirrors: Mirrors,
}

impl Http {
    /// GETs `url`, following at most `MAX_REDIRECTS` HTTPS redirects by hand so each
    /// hop can be rewritten to its configured mirror.
    async fn get(&self, url: &str) -> anyhow::Result<reqwest::Response> {
        let mut current = self.mirrors.rewrite(url);
        for _ in 0..=MAX_REDIRECTS {
            let response = self
                .client
                .get(&current)
                .send()
                .await
                .with_context(|| format!("HTTP request failed: {current}"))?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .context("HTTP redirect has no Location header")?;
            let next = response
                .url()
                .join(location)
                .context("HTTP redirect has an invalid Location header")?;
            if next.scheme() != "https" {
                bail!("refusing non-HTTPS redirect");
            }
            current = self.mirrors.rewrite(next.as_str());
        }
        bail!("too many HTTP redirects from {url}")
    }
}

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
    channel: Option<String>,
    artifacts: Option<Vec<ReleaseArtifact>>,
}

#[derive(Debug, Deserialize, Clone)]
struct ReleaseArtifact {
    file: String,
    sha256: String,
    bytes: Option<u64>,
}

struct ReleaseSource {
    manifest_url: String,
    artifact_base_url: String,
    default_github: bool,
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

pub async fn run(apply: bool, yes: bool) -> anyhow::Result<()> {
    let target = target()?;
    let current = env!("RIVO_RELEASE_VERSION");
    let configured = std::env::var("RIVO_RELEASE_BASE_URL").ok();
    let base = configured
        .as_deref()
        .unwrap_or(DEFAULT_RELEASE_BASE_URL)
        .trim_end_matches('/');
    if configured.is_some() {
        validate_base_url(base)?;
    }
    let source = release_source(base, configured.is_none(), target);
    let client = Client::builder()
        .user_agent(format!("rivo/{current}"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context(ui::text(
            "cli-upgrade-http-client-failed",
            "Failed to create the upgrade HTTP client",
        ))?;
    let http = Http {
        client,
        mirrors: Mirrors::from_env()?,
    };
    let manifest: ReleaseManifest =
        fetch_json(&http, &source.manifest_url)
            .await
            .with_context(|| {
                ui::text_with(
                    "cli-upgrade-manifest-read-failed",
                    &format!("Failed to read latest manifest: {}", source.manifest_url),
                    &[("url", source.manifest_url.clone())],
                )
            })?;
    validate_manifest(&manifest, target)?;
    println!(
        "{}",
        ui::text_with(
            "cli-upgrade-current-version",
            &format!("Current version: {current}"),
            &[("version", current.to_owned())]
        )
    );
    println!(
        "{}",
        ui::text_with(
            "cli-upgrade-latest-version",
            &format!("Latest version: {}", manifest.version),
            &[("version", manifest.version.clone())]
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
        Ordering::Greater => bail!(ui::text_with(
            "cli-upgrade-downgrade",
            &format!(
                "Remote version {} is lower than current version {}; downgrade rejected",
                manifest.version, current
            ),
            &[
                ("remote", manifest.version.clone()),
                ("current", current.to_owned())
            ]
        )),
        Ordering::Less if !apply => {
            println!(
                "{}",
                ui::text("cli-upgrade-available", "An update is available.")
            );
            Ok(())
        }
        Ordering::Less => apply_update(&http, &source, &manifest, target, yes).await,
    }
}

/// A custom release base must use HTTPS; plain HTTP is only accepted for loopback
/// hosts so local mirrors and tests keep working.
fn validate_base_url(base: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(base).context("RIVO_RELEASE_BASE_URL is not a valid URL")?;
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => bail!(
            "RIVO_RELEASE_BASE_URL must be an https:// URL (http is only allowed for loopback)"
        ),
    }
}

fn release_source(base: &str, default_github: bool, target: Target) -> ReleaseSource {
    if default_github {
        ReleaseSource {
            manifest_url: format!(
                "{base}/release-manifest-{}-{}.json",
                target.platform, target.arch
            ),
            artifact_base_url: base.to_owned(),
            default_github: true,
        }
    } else {
        let directory = format!("{base}/{}-{}", target.platform, target.arch);
        ReleaseSource {
            manifest_url: format!("{directory}/release-manifest.json"),
            artifact_base_url: directory,
            default_github: false,
        }
    }
}

async fn apply_update(
    http: &Http,
    source: &ReleaseSource,
    manifest: &ReleaseManifest,
    target: Target,
    yes: bool,
) -> anyhow::Result<()> {
    let artifact = manifest
        .artifacts
        .as_deref()
        .and_then(|items| items.first())
        .context("release manifest does not contain an archive artifact")?
        .clone();
    validate_artifact(&artifact, manifest, target)?;
    let root = installation_root()?;
    ensure_no_other_rivo_processes()?;
    fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
    let lock_path = root.join(".upgrade.lock");
    let lock = File::create(&lock_path)
        .with_context(|| format!("cannot create {}", lock_path.display()))?;
    lock.try_lock_exclusive()
        .context("another Rivo upgrade is already running")?;
    if !yes
        && !confirm(&format!(
            "Install Rivo {} from {} into {}? [y/N] ",
            manifest.version,
            artifact.file,
            root.display()
        ))?
    {
        bail!("upgrade cancelled")
    }

    let temp = tempfile::tempdir_in(&root).context("cannot create upgrade staging directory")?;
    let artifact_base = if source.default_github {
        format!("{DEFAULT_RELEASE_DOWNLOAD_ROOT}/v{}", manifest.version)
    } else {
        source.artifact_base_url.clone()
    };
    let archive = temp.path().join(&artifact.file);
    download_to(
        http,
        &format!("{artifact_base}/{}", artifact.file),
        &archive,
    )
    .await?;
    verify_archive(http, &artifact_base, &artifact, &archive).await?;
    let extracted = temp.path().join("extracted");
    fs::create_dir_all(&extracted)?;
    extract_archive(&archive, &extracted, target)?;
    let payload = extracted.join(archive_root_name(&artifact.file, target)?);
    validate_payload(&payload, target)?;
    #[cfg(windows)]
    {
        let staging = temp.keep();
        if let Err(error) = spawn_windows_upgrade_helper(&root, &payload, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        println!("upgrade staged; the replacement helper will finish after this process exits");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        install_unix(&root, &payload, &manifest.version)?;
        println!("installed Rivo {}", manifest.version);
        Ok(())
    }
}

async fn download_to(http: &Http, url: &str, path: &Path) -> anyhow::Result<()> {
    let response = http.get(url).await?;
    if !response.status().is_success() {
        bail!("HTTP {} from {url}", response.status());
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARCHIVE_BYTES)
    {
        bail!("release archive exceeds the {MAX_ARCHIVE_BYTES} byte limit");
    }
    let mut response = response;
    let mut file =
        File::create(path).with_context(|| format!("cannot write {}", path.display()))?;
    let mut written = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read release archive")?
    {
        written += chunk.len() as u64;
        if written > MAX_ARCHIVE_BYTES {
            bail!("release archive exceeds the {MAX_ARCHIVE_BYTES} byte limit");
        }
        file.write_all(&chunk)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

async fn read_limited(mut response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_TEXT_BYTES {
            bail!("HTTP response exceeds the {MAX_TEXT_BYTES} byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn verify_archive(
    http: &Http,
    base: &str,
    artifact: &ReleaseArtifact,
    archive: &Path,
) -> anyhow::Result<()> {
    let sidecar = fetch_text(http, &format!("{base}/{}.sha256", artifact.file)).await?;
    let declared = sidecar
        .split_whitespace()
        .next()
        .context("checksum sidecar is empty")?;
    if declared != artifact.sha256 {
        bail!("manifest and checksum sidecar disagree");
    }
    let mut file = File::open(archive)?;
    let mut digest = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        count += read as u64;
    }
    let actual = hex::encode(digest.finalize());
    if actual != artifact.sha256 {
        bail!("release archive checksum verification failed");
    }
    if artifact.bytes.is_some_and(|expected| expected != count) {
        bail!("release archive size does not match the manifest");
    }
    Ok(())
}

async fn fetch_text(http: &Http, url: &str) -> anyhow::Result<String> {
    let response = http.get(url).await?;
    if !response.status().is_success() {
        bail!("HTTP {} from {url}", response.status());
    }
    String::from_utf8(read_limited(response).await?).context("response is not UTF-8")
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(http: &Http, url: &str) -> anyhow::Result<T> {
    let response = http.get(url).await.context(ui::text(
        "cli-upgrade-http-request-failed",
        "HTTP request failed",
    ))?;
    if !response.status().is_success() {
        bail!("HTTP {} from {url}", response.status());
    }
    let body = read_limited(response).await?;
    serde_json::from_slice(&body).context(ui::text(
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
    if manifest
        .channel
        .as_deref()
        .is_some_and(|channel| channel != "stable")
    {
        bail!("release manifest is not a stable release");
    }
    validate_version(&manifest.version)
}

fn validate_artifact(
    artifact: &ReleaseArtifact,
    manifest: &ReleaseManifest,
    target: Target,
) -> anyhow::Result<()> {
    let suffix = if target.platform == "windows" {
        ".zip"
    } else {
        ".tar.gz"
    };
    let expected = format!(
        "rivo-{}-{}-{}{}",
        manifest.version, target.platform, target.arch, suffix
    );
    if artifact.file != expected
        || artifact.sha256.len() != 64
        || !artifact.sha256.chars().all(|c| c.is_ascii_hexdigit())
    {
        bail!("release artifact does not match the current target");
    }
    Ok(())
}

fn archive_root_name(file: &str, target: Target) -> anyhow::Result<String> {
    let suffix = if target.platform == "windows" {
        ".zip"
    } else {
        ".tar.gz"
    };
    file.strip_suffix(suffix)
        .map(str::to_owned)
        .context("invalid release archive name")
}

fn validate_payload(payload: &Path, target: Target) -> anyhow::Result<()> {
    let binary = payload.join(if target.platform == "windows" {
        "rivo.exe"
    } else {
        "rivo"
    });
    if !binary.is_file() || !payload.join("share/rivo/languages/en-US.ftl").is_file() {
        bail!("release archive is missing the executable or language resources");
    }
    if target.platform == "windows" && !payload.join("rivo-browser-extension-host.exe").is_file() {
        bail!("release archive is missing the Windows extension host");
    }
    Ok(())
}

fn extract_archive(archive: &Path, destination: &Path, target: Target) -> anyhow::Result<()> {
    if target.platform == "windows" {
        let mut zip =
            zip::ZipArchive::new(File::open(archive)?).context("cannot read ZIP archive")?;
        if zip.len() > MAX_ARCHIVE_ENTRIES {
            bail!("archive contains too many entries");
        }
        let mut unpacked = 0_u64;
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index)?;
            if entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                bail!("archive contains a symbolic link");
            }
            unpacked = unpacked.saturating_add(entry.size());
            if unpacked > MAX_UNPACKED_BYTES {
                bail!("archive expands beyond the size limit");
            }
            let entry_name = entry.name()?;
            let name = Path::new(entry_name.as_ref());
            if !safe_archive_path(name) {
                bail!("archive contains an unsafe path");
            }
            let output = destination.join(name);
            if entry.is_dir() {
                fs::create_dir_all(output)?;
            } else {
                if let Some(parent) = output.parent() {
                    fs::create_dir_all(parent)?;
                }
                let limit = MAX_UNPACKED_BYTES;
                io::copy(&mut (&mut entry).take(limit), &mut File::create(output)?)?;
            }
        }
    } else {
        let decoder = flate2::read::GzDecoder::new(File::open(archive)?);
        let mut tar = tar::Archive::new(decoder);
        let mut count = 0_usize;
        let mut unpacked = 0_u64;
        for entry in tar.entries()? {
            let mut entry = entry?;
            count += 1;
            unpacked = unpacked.saturating_add(entry.header().size().unwrap_or(0));
            if count > MAX_ARCHIVE_ENTRIES || unpacked > MAX_UNPACKED_BYTES {
                bail!("archive exceeds the entry or size limit");
            }
            let path = entry.path()?.into_owned();
            if !safe_archive_path(&path)
                || (!entry.header().entry_type().is_file() && !entry.header().entry_type().is_dir())
            {
                bail!("archive contains an unsafe entry");
            }
            entry.unpack_in(destination)?;
        }
    }
    Ok(())
}

fn safe_archive_path(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(not(windows))]
fn install_unix(root: &Path, payload: &Path, version: &str) -> anyhow::Result<()> {
    let versions = root.join("versions");
    fs::create_dir_all(&versions)?;
    let version_dir = versions.join(version);
    let displaced = versions.join(format!("{version}.old.{}", std::process::id()));
    let had_existing = version_dir.exists();
    if had_existing {
        fs::rename(&version_dir, &displaced)?;
    }
    if let Err(error) = fs::rename(payload, &version_dir) {
        if had_existing {
            let _ = fs::rename(&displaced, &version_dir);
        }
        return Err(error.into());
    }
    if had_existing {
        let _ = fs::remove_dir_all(&displaced);
    }
    let current = root.join("current");
    if current.exists() || current.is_symlink() {
        if let Ok(previous_target) = fs::read_link(&current) {
            let previous = root.join("previous.new");
            let _ = fs::remove_file(&previous);
            std::os::unix::fs::symlink(previous_target, &previous)?;
            let _ = fs::remove_file(root.join("previous"));
            fs::rename(previous, root.join("previous"))?;
        }
    }
    let next = root.join("current.new");
    let _ = fs::remove_file(&next);
    std::os::unix::fs::symlink(format!("versions/{version}"), &next)?;
    fs::rename(next, current)?;
    Ok(())
}

fn installation_root() -> anyhow::Result<PathBuf> {
    if let Some(root) = std::env::var_os("RIVO_INSTALL_ROOT").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    let executable = std::env::current_exe().context("cannot locate the current executable")?;
    let mut candidate = executable.parent().context("executable has no parent")?;
    for _ in 0..5 {
        if candidate.join("versions").is_dir() && candidate.join("current").exists() {
            return Ok(candidate.to_owned());
        }
        candidate = match candidate.parent() {
            Some(parent) => parent,
            None => break,
        };
    }
    bail!("cannot determine the Rivo installation root; run the release installer")
}

fn ensure_no_other_rivo_processes() -> anyhow::Result<()> {
    let current_pid = std::process::id().to_string();
    #[cfg(windows)]
    let output = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq rivo.exe", "/FO", "CSV", "/NH"])
        .output();
    #[cfg(not(windows))]
    let output = Command::new("ps").args(["-axo", "pid=,comm="]).output();
    let Ok(output) = output else { return Ok(()) };
    if !output.status.success() {
        return Ok(());
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        #[cfg(windows)]
        let is_rivo = {
            let fields: Vec<_> = line.trim_matches('"').split("\",\"").collect();
            fields
                .first()
                .is_some_and(|name| name.eq_ignore_ascii_case("rivo.exe"))
                && fields.get(1).is_some_and(|pid| pid.trim() != current_pid)
        };
        #[cfg(not(windows))]
        let is_rivo = {
            let mut fields = line.split_whitespace();
            let pid = fields.next();
            let name = fields.next();
            pid != Some(current_pid.as_str())
                && name.is_some_and(|value| {
                    Path::new(value).file_name().and_then(|item| item.to_str()) == Some("rivo")
                })
        };
        if is_rivo {
            bail!("another Rivo process is running; stop it before applying an upgrade");
        }
    }
    Ok(())
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("upgrade apply requires an interactive terminal or --yes");
    }
    print!("{prompt}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(windows)]
fn spawn_windows_upgrade_helper(root: &Path, payload: &Path, staging: &Path) -> anyhow::Result<()> {
    let helper = std::env::temp_dir().join(format!("rivo-upgrade-{}.ps1", std::process::id()));
    let script = r#"param([string]$Root,[string]$Payload,[string]$StageRoot,[int]$ParentPid,[string]$Self)
$ErrorActionPreference='Stop'
try { while (Get-Process -Id $ParentPid -ErrorAction SilentlyContinue) { Start-Sleep -Milliseconds 250 }; $current=Join-Path $Root 'current'; $previous=Join-Path $Root 'previous'; if (Test-Path $previous) { Remove-Item -Recurse -Force $previous }; if (Test-Path $current) { Move-Item $current $previous }; Move-Item $Payload $current; & (Join-Path $current 'rivo.exe') --version } catch { if ((Test-Path $previous) -and -not (Test-Path $current)) { Move-Item $previous $current }; Write-Error $_; exit 1 } finally { Remove-Item -Recurse -Force $StageRoot -ErrorAction SilentlyContinue; Remove-Item -Force $Self -ErrorAction SilentlyContinue }"#;
    fs::write(&helper, script)?;
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            helper.to_str().context("helper path is not UTF-8")?,
            "-Root",
            root.to_str().context("install root is not UTF-8")?,
            "-Payload",
            payload.to_str().context("staged path is not UTF-8")?,
            "-StageRoot",
            staging.to_str().context("staging path is not UTF-8")?,
            "-ParentPid",
            &std::process::id().to_string(),
            "-Self",
            helper.to_str().context("helper path is not UTF-8")?,
        ])
        .spawn()
        .context("cannot start the Windows upgrade helper")?;
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
        || value.split(['.', '-', '+']).any(str::is_empty)
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
    #[test]
    fn rewrites_only_configured_github_origins() {
        let mirrors = Mirrors::parse(
            Some("https://github-mirror.example/"),
            Some("https://assets.example/x"),
        )
        .unwrap();
        assert_eq!(
            mirrors.rewrite("https://github.com/abcred-labs/rivo/releases/latest/download/a.json"),
            "https://github-mirror.example/abcred-labs/rivo/releases/latest/download/a.json"
        );
        assert_eq!(
            mirrors.rewrite("https://release-assets.githubusercontent.com/p?q=1"),
            "https://assets.example/x/p?q=1"
        );
        assert_eq!(
            mirrors.rewrite("https://github.com.evil.test/a"),
            "https://github.com.evil.test/a"
        );
        assert_eq!(
            Mirrors::default().rewrite("https://github.com/a"),
            "https://github.com/a"
        );
    }
    #[test]
    fn restricts_custom_release_base_urls() {
        assert!(validate_base_url("https://mirror.example/rivo").is_ok());
        assert!(validate_base_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_base_url("http://localhost:8080").is_ok());
        assert!(validate_base_url("http://mirror.example").is_err());
        assert!(validate_base_url("ftp://mirror.example").is_err());
        assert!(validate_base_url("not a url").is_err());
    }
    #[test]
    fn rejects_non_https_mirrors() {
        assert!(Mirrors::parse(Some("http://mirror.example"), None).is_err());
        assert!(Mirrors::parse(None, Some("https://m.example/a b")).is_err());
        assert_eq!(
            Mirrors::parse(Some(" "), Some("")).unwrap(),
            Mirrors::default()
        );
    }
    #[test]
    fn validates_target_artifact_names() {
        let manifest = ReleaseManifest {
            schema: 2,
            product: "rivo".into(),
            version: "0.2.0".into(),
            platform: "linux".into(),
            arch: "amd64".into(),
            channel: Some("stable".into()),
            artifacts: None,
        };
        let artifact = ReleaseArtifact {
            file: "rivo-0.2.0-linux-amd64.tar.gz".into(),
            sha256: "a".repeat(64),
            bytes: None,
        };
        validate_artifact(
            &artifact,
            &manifest,
            Target {
                platform: "linux",
                arch: "amd64",
            },
        )
        .unwrap();
        assert!(validate_artifact(
            &ReleaseArtifact {
                file: "rivo-0.2.0-linux-arm64.tar.gz".into(),
                ..artifact
            },
            &manifest,
            Target {
                platform: "linux",
                arch: "amd64"
            }
        )
        .is_err());
    }
}
