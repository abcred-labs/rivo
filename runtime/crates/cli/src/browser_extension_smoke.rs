//! Independent Chrome/Edge extension smoke runner.
//!
//! This command intentionally talks to the Rivo named-pipe client directly.
//! It does not use a desktop connector, CDP, or an external browser driver.

use anyhow::{bail, Context, Result};
#[cfg(windows)]
use rivo_policy::{
    CapabilityProfile, Operation, PolicyDecision, PolicyEvaluator, PolicyProfile, ToolRequest,
};
use rivo_tools::browser_extension::{BrowserInstanceRef, ExtensionBrowserClient};
use serde_json::{json, Value};

#[cfg(windows)]
use base64::Engine;
#[cfg(windows)]
use rivo_tools::computer::{
    ComputerAction, ComputerBackend, Screenshot, WindowInfo, WindowsComputerBackend,
};

#[cfg(windows)]
pub async fn run_seo_smoke(args: Vec<String>) -> Result<i32> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("Usage: rivo browser-extension-seo-smoke --family <chrome|edge> --pipe <named-pipe> --url <http-url> --output <snapshot.json> --screenshot <page.png> [--extension-id <id> --installation-id <id> --profile-key <key> --boot-id <id>]");
        return Ok(0);
    }
    let options = SeoOptions::parse(args)?;
    let policy = PolicyEvaluator::new(std::env::current_dir()?)?;
    let expected = BrowserInstanceRef {
        browser_family: options.family.clone(),
        extension_id: options.extension_id.clone().unwrap_or_default(),
        installation_id: options.installation_id.clone().unwrap_or_default(),
        profile_key: options.profile_key.clone().unwrap_or_default(),
        boot_id: options.boot_id.clone().unwrap_or_default(),
    };
    let client = ExtensionBrowserClient::connect_named_pipe(&options.pipe, expected)
        .await
        .context("无法连接 Rivo 浏览器扩展")?;
    require_smoke_policy(&policy, "browser_list_tabs", Operation::Browser)?;
    let tabs = client.call("browser_list_tabs", Value::Null).await?;
    let tab = tabs
        .as_array()
        .and_then(|tabs| {
            tabs.iter()
                .find(|tab| tab["selected"] == true)
                .or(tabs.first())
        })
        .context("Rivo 扩展未返回浏览器标签页")?;
    let tab_id = tab["id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| tab["id"].as_i64().map(|id| id.to_string()))
        .context("浏览器标签页缺少 ID")?;
    let mut navigation_error = None;
    require_smoke_policy(&policy, "browser_select_tab", Operation::Browser)?;
    if let Err(error) = client
        .call("browser_select_tab", json!({"id": tab_id}))
        .await
    {
        navigation_error = Some(format!("选择标签页失败: {error}"));
    } else {
        require_smoke_policy(&policy, "browser_navigate", Operation::Browser)?;
        if let Err(error) = client
            .call("browser_navigate", json!({"url": options.url}))
            .await
        {
            navigation_error = Some(format!("页面导航失败: {error}"));
        }
    }
    let mut wait_error = None;
    if navigation_error.is_none() {
        require_smoke_policy(&policy, "browser_wait", Operation::Browser)?;
        if let Err(error) = client
            .call("browser_wait", json!({"timeout_ms": 3000}))
            .await
        {
            wait_error = Some(format!("页面等待失败: {error}"));
        }
    }
    let (seo, seo_error) = if navigation_error.is_none() && wait_error.is_none() {
        require_smoke_policy(&policy, "browser_seo_snapshot", Operation::Browser)?;
        match client.call("browser_seo_snapshot", Value::Null).await {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(format!("SEO 页面读取失败: {error}"))),
        }
    } else {
        (None, None)
    };

    require_smoke_policy(&policy, "computer_list_windows", Operation::Computer)?;
    let backend = WindowsComputerBackend;
    let (screenshot_dimensions, screenshot_error) = match async {
        let windows: Vec<WindowInfo> =
            serde_json::from_value(backend.execute(ComputerAction::ListWindows).await?)?;
        let expected_process = match options.family.as_str() {
            "chrome" => "chrome.exe",
            "edge" => "msedge.exe",
            _ => bail!("不支持的浏览器 family"),
        };
        let browser_window = windows
            .iter()
            .find(|window| {
                !window.sensitive
                    && window.process.eq_ignore_ascii_case(expected_process)
                    && window.class_name.contains("Chrome_WidgetWin")
            })
            .with_context(|| format!("未找到可安全截图的 {expected_process} 窗口"))?;
        require_smoke_policy(&policy, "computer_screenshot", Operation::Computer)?;
        let screenshot: Screenshot = serde_json::from_value(
            backend
                .execute(ComputerAction::Screenshot {
                    window_id: Some(browser_window.id.clone()),
                })
                .await?,
        )?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(&screenshot.bytes_base64)
            .context("Rivo Computer Use PNG base64 无效")?;
        if screenshot.media_type != "image/png"
            || screenshot.width == 0
            || screenshot.height == 0
            || !png.starts_with(b"\x89PNG\r\n\x1a\n")
        {
            bail!("Rivo Computer Use 未返回有效 PNG 截图");
        }
        write_new_file(&options.screenshot, &png)?;
        Ok::<_, anyhow::Error>((
            screenshot.width,
            screenshot.height,
            browser_window.process.clone(),
        ))
    }
    .await
    {
        Ok(dimensions) => (Some(dimensions), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let status = if seo.is_some() && screenshot_dimensions.is_some() {
        "passed"
    } else {
        "partial"
    };
    let result = json!({
        "status": status,
        "browser": {
            "family": client.instance().browser_family,
            "extension_id": client.instance().extension_id,
            "installation_id": client.instance().installation_id,
            "profile_key": client.instance().profile_key,
            "boot_id": client.instance().boot_id,
            "control_path": "Rivo Manifest V3 extension -> Native Messaging -> named pipe",
        },
        "requested_url": options.url,
        "seo": seo,
        "navigation_error": navigation_error,
        "wait_error": wait_error,
        "seo_error": seo_error,
        "screenshot": screenshot_dimensions.as_ref().map(|(width, height, window_process)| json!({
            "path": options.screenshot,
            "width": width,
            "height": height,
            "window_process": window_process,
            "capture_path": "Rivo Windows Computer Use GDI/PNG",
        })),
        "screenshot_error": screenshot_error,
    });
    let json_bytes = serde_json::to_vec_pretty(&result)?;
    if json_bytes.len() > 64 * 1024 {
        bail!("SEO smoke JSON output exceeds 64 KiB");
    }
    write_new_file(&options.output, &json_bytes)?;
    println!(
        "browser-extension-seo-smoke: family={} profile={} json_bytes={} screenshot={} status={status}",
        client.instance().browser_family,
        client.instance().profile_key,
        json_bytes.len(),
        screenshot_dimensions
            .as_ref()
            .map(|(width, height, _)| format!("{width}x{height}"))
            .unwrap_or_else(|| "unavailable".into()),
    );
    Ok(0)
}

#[cfg(windows)]
fn require_smoke_policy(
    policy: &PolicyEvaluator,
    tool_name: &str,
    operation: Operation,
) -> Result<()> {
    let request = ToolRequest {
        tool_name: tool_name.into(),
        operation,
        targets: Vec::new(),
        program: None,
        args: Vec::new(),
        arguments_summary: if tool_name == "computer_screenshot" {
            "computer screenshot current non-sensitive browser window".into()
        } else if tool_name == "computer_list_windows" {
            "computer list_windows visible windows".into()
        } else {
            format!("read-only extension browser operation {tool_name}")
        },
        binary: tool_name == "computer_screenshot",
        item_count: 1,
        network: false,
    };
    match policy.evaluate(PolicyProfile::Medium, CapabilityProfile::General, &request) {
        PolicyDecision::Allow { .. } => Ok(()),
        decision => bail!("Rivo policy did not allow SEO smoke action: {decision:?}"),
    }
}

#[cfg(not(windows))]
pub async fn run_seo_smoke(_args: Vec<String>) -> Result<i32> {
    bail!("browser-extension-seo-smoke currently requires Windows Computer Use")
}

#[cfg(windows)]
fn write_new_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("无法新建输出文件: {}", path.display()))?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(windows)]
struct SeoOptions {
    family: String,
    pipe: String,
    url: String,
    output: std::path::PathBuf,
    screenshot: std::path::PathBuf,
    extension_id: Option<String>,
    installation_id: Option<String>,
    profile_key: Option<String>,
    boot_id: Option<String>,
}

#[cfg(windows)]
impl SeoOptions {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut family = None;
        let mut pipe = None;
        let mut url = None;
        let mut output = None;
        let mut screenshot = None;
        let mut extension_id = None;
        let mut installation_id = None;
        let mut profile_key = None;
        let mut boot_id = None;
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            let value = || anyhow::anyhow!("{} 需要参数", arg);
            match arg.as_str() {
                "--family" => family = Some(iter.next().ok_or_else(value)?),
                "--pipe" => pipe = Some(iter.next().ok_or_else(value)?),
                "--url" => url = Some(iter.next().ok_or_else(value)?),
                "--output" => {
                    output = Some(std::path::PathBuf::from(iter.next().ok_or_else(value)?))
                }
                "--screenshot" => {
                    screenshot = Some(std::path::PathBuf::from(iter.next().ok_or_else(value)?))
                }
                "--extension-id" => extension_id = Some(iter.next().ok_or_else(value)?),
                "--installation-id" => installation_id = Some(iter.next().ok_or_else(value)?),
                "--profile-key" => profile_key = Some(iter.next().ok_or_else(value)?),
                "--boot-id" => boot_id = Some(iter.next().ok_or_else(value)?),
                other => bail!("未知参数: {other}"),
            }
        }
        let family = family.context("--family 是必需的")?;
        if family != "chrome" && family != "edge" {
            bail!("--family 只能是 chrome 或 edge");
        }
        let pipe = pipe.context("--pipe 是必需的")?;
        let url = url.context("--url 是必需的")?;
        let parsed = reqwest::Url::parse(&url).context("--url 不是有效 URL")?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            bail!("--url 只能使用无凭据 HTTP(S) 地址");
        }
        let output = output.context("--output 是必需的")?;
        let screenshot = screenshot.context("--screenshot 是必需的")?;
        if output == screenshot
            || output.extension().and_then(|value| value.to_str()) != Some("json")
            || screenshot.extension().and_then(|value| value.to_str()) != Some("png")
        {
            bail!("--output 和 --screenshot 必须分别指向不同的 .json 与 .png 文件");
        }
        Ok(Self {
            family,
            pipe,
            url,
            output,
            screenshot,
            extension_id,
            installation_id,
            profile_key,
            boot_id,
        })
    }
}

pub async fn run(args: Vec<String>) -> Result<i32> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("Usage: rivo browser-extension-smoke --family <chrome|edge> --pipe <named-pipe> --url <http-url> [--extension-id <id> --installation-id <id> --profile-key <key> --boot-id <id> --expect-marker <text>]");
        return Ok(0);
    }
    let options = Options::parse(args)?;
    let expected = BrowserInstanceRef {
        browser_family: options.family.clone(),
        extension_id: options.extension_id.clone().unwrap_or_default(),
        installation_id: options.installation_id.clone().unwrap_or_default(),
        profile_key: options.profile_key.clone().unwrap_or_default(),
        boot_id: options.boot_id.clone().unwrap_or_default(),
    };
    let client = ExtensionBrowserClient::connect_named_pipe(&options.pipe, expected)
        .await
        .with_context(|| format!("无法连接 Rivo 浏览器扩展管道: {}", options.pipe))?;

    let tabs = client.call("browser_list_tabs", Value::Null).await?;
    let tab_count = tabs.as_array().map_or(0, Vec::len);
    if tab_count == 0 {
        bail!("扩展未返回任何标签页");
    }
    let tab_id = tabs[0]
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            tabs[0]
                .get("id")
                .and_then(Value::as_i64)
                .map(|id| id.to_string())
        })
        .context("扩展标签页缺少 ID")?;
    let selected = client
        .call("browser_select_tab", json!({"id": tab_id}))
        .await?;
    if selected.get("selected").and_then(Value::as_bool) != Some(true) {
        bail!("扩展未确认标签页选择");
    }

    client
        .call("browser_navigate", json!({ "url": options.url }))
        .await?;
    let snapshot = snapshot_with_retry(&client).await?;
    let elements = snapshot
        .get("elements")
        .and_then(Value::as_array)
        .context("扩展快照缺少 elements 数组")?;

    let button_ref = find_ref(elements, |element| {
        element.get("text").and_then(Value::as_str) == Some("Rivo smoke button")
    })?;
    client
        .call("browser_click", json!({ "ref": button_ref }))
        .await
        .context("扩展点击 smoke button 失败")?;

    let input_ref = find_ref(elements, |element| {
        element.get("placeholder").and_then(Value::as_str) == Some("smoke-input")
    })?;
    client
        .call(
            "browser_type",
            json!({ "ref": input_ref, "text": "rivo-smoke", "clear": true }),
        )
        .await
        .context("扩展输入 smoke-input 失败")?;

    let select_ref = find_ref(elements, |element| {
        element.get("tag").and_then(Value::as_str) == Some("select")
    })?;
    let selected_value = client
        .call("browser_select", json!({"ref": select_ref, "value": "two"}))
        .await?;
    if selected_value.get("value").and_then(Value::as_str) != Some("two") {
        bail!("扩展 select 操作未返回目标值");
    }
    client
        .call("browser_keypress", json!({"key": "Enter"}))
        .await
        .context("扩展 keypress smoke 操作失败")?;

    let wait_button_ref = find_ref(elements, |element| {
        element.get("text").and_then(Value::as_str) == Some("Rivo wait target")
    })?;
    let wait_timeout_ref = find_ref(elements, |element| {
        element.get("text").and_then(Value::as_str) == Some("Rivo wait timeout target")
    })?;
    let blank_link_ref = find_ref(elements, |element| {
        element.get("text").and_then(Value::as_str) == Some("Rivo blank link")
    })?;
    client
        .call("browser_click", json!({"ref": wait_button_ref}))
        .await
        .context("扩展点击等待 smoke target 失败")?;
    let wait_result = client
        .call(
            "browser_wait",
            json!({"ref": wait_button_ref, "timeout_ms": 2000}),
        )
        .await
        .context("扩展等待 smoke target 失败")?;
    if wait_result.get("ready").and_then(Value::as_bool) != Some(true) {
        bail!("扩展 browser_wait 未返回 ready=true");
    }
    client
        .call("browser_click", json!({"ref": wait_timeout_ref}))
        .await
        .context("扩展点击永久移除的 wait smoke target 失败")?;
    let wait_timeout = client
        .call(
            "browser_wait",
            json!({"ref": wait_timeout_ref, "timeout_ms": 100}),
        )
        .await
        .expect_err("永久移除的观察引用必须超时");
    if !wait_timeout.to_string().contains("超时") {
        bail!("browser_wait 缺少引用超时结果: {wait_timeout}");
    }

    let scroll_result = client
        .call("browser_scroll", json!({"x": 0, "y": 500}))
        .await
        .context("扩展 scroll smoke 操作失败")?;
    if scroll_result
        .get("y")
        .and_then(Value::as_i64)
        .unwrap_or_default()
        < 300
    {
        bail!("扩展 scroll smoke 未改变页面滚动位置");
    }
    let delay_result = client
        .call("browser_wait", json!({"timeout_ms": 50}))
        .await
        .context("扩展有界等待失败")?;
    if delay_result
        .get("waited_ms")
        .and_then(Value::as_u64)
        .unwrap_or_default()
        < 50
    {
        bail!("扩展无引用 wait 未遵守最小等待时间");
    }
    let final_snapshot = snapshot_with_retry(&client).await?;
    let body = final_snapshot
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !body.contains("Rivo smoke clicked") {
        bail!("点击结果未出现在页面快照中");
    }
    let input_value = final_snapshot
        .get("elements")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|element| {
                (element.get("placeholder").and_then(Value::as_str) == Some("smoke-input"))
                    .then(|| element.get("text").and_then(Value::as_str))
                    .flatten()
            })
        })
        .unwrap_or_default();
    if input_value != "rivo-smoke" {
        bail!("输入结果未出现在页面快照中");
    }
    for marker in ["Rivo smoke clicked", "Rivo enter pressed", "selected:two"] {
        if !body.contains(marker) {
            bail!("页面未显示动作结果: {marker}");
        }
    }
    if let Some(marker) = options.expect_marker.as_deref() {
        if !body.contains(marker) {
            bail!("Profile 页面标记不匹配，缺少: {marker}");
        }
    }
    if final_snapshot
        .get("text")
        .and_then(Value::as_str)
        .map_or(usize::MAX, str::len)
        > 65_536
    {
        bail!("页面观察超过 64 KiB 上限");
    }

    for url in [
        "javascript:alert(1)",
        "file:///C:/Windows/win.ini",
        "https://user:pass@example.test/",
    ] {
        expect_failure(&client, "browser_navigate", json!({"url": url})).await?;
    }
    for method in [
        "browser_download",
        "browser_upload",
        "browser_execute_script",
    ] {
        expect_failure(&client, method, json!({})).await?;
    }
    for (ref_id, text) in [("rivo-900", "secret"), ("rivo-901", "C:\\temp\\file")] {
        expect_failure(
            &client,
            "browser_type",
            json!({"ref": ref_id, "text": text}),
        )
        .await?;
    }
    for ref_id in ["rivo-902", "rivo-999998"] {
        expect_failure(&client, "browser_click", json!({"ref": ref_id})).await?;
    }
    expect_failure(&client, "browser_custom", json!({})).await?;
    let before_blank_click = client.call("browser_list_tabs", Value::Null).await?;
    expect_failure(&client, "browser_click", json!({"ref": blank_link_ref})).await?;
    let after_blank_click = client.call("browser_list_tabs", Value::Null).await?;
    if before_blank_click.as_array().map(Vec::len) != after_blank_click.as_array().map(Vec::len) {
        bail!("被拒绝的 _blank 点击仍然打开了新标签页");
    }

    println!(
        "browser-extension-smoke: family={} installation={} profile={} instance={} tabs={} actions=10 negative_cases=passed status=passed",
        client.instance().browser_family,
        client.instance().installation_id,
        client.instance().profile_key,
        client.instance().boot_id,
        tab_count
    );
    Ok(0)
}

async fn expect_failure(
    client: &ExtensionBrowserClient,
    method: &str,
    params: Value,
) -> Result<()> {
    match client.call(method, params).await {
        Ok(_) => bail!("不安全或无效操作意外成功: {method}"),
        Err(_) => Ok(()),
    }
}

fn find_ref(elements: &[Value], mut predicate: impl FnMut(&Value) -> bool) -> Result<String> {
    elements
        .iter()
        .find(|element| predicate(element))
        .and_then(|element| element.get("ref"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("扩展快照缺少 smoke 所需元素")
}

async fn snapshot_with_retry(client: &ExtensionBrowserClient) -> Result<Value> {
    let mut last_error = None;
    for _ in 0..20 {
        match client.call("browser_snapshot", Value::Null).await {
            Ok(snapshot) if snapshot.get("elements").and_then(Value::as_array).is_some() => {
                return Ok(snapshot);
            }
            Ok(_) => {
                last_error = Some(anyhow::anyhow!("扩展页面脚本尚未就绪"));
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(error) => {
                last_error = Some(error.into());
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
    Err(anyhow::anyhow!(
        "扩展页面快照在 5 秒内不可用: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "未知错误".into())
    ))
}

struct Options {
    family: String,
    pipe: String,
    url: String,
    extension_id: Option<String>,
    installation_id: Option<String>,
    profile_key: Option<String>,
    boot_id: Option<String>,
    expect_marker: Option<String>,
}

impl Options {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut family = None;
        let mut pipe = None;
        let mut url = None;
        let mut extension_id = None;
        let mut installation_id = None;
        let mut profile_key = None;
        let mut boot_id = None;
        let mut expect_marker = None;
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            let value = || anyhow::anyhow!("{} 需要参数", arg);
            match arg.as_str() {
                "--family" => family = Some(iter.next().ok_or_else(value)?),
                "--pipe" => pipe = Some(iter.next().ok_or_else(value)?),
                "--url" => url = Some(iter.next().ok_or_else(value)?),
                "--extension-id" => extension_id = Some(iter.next().ok_or_else(value)?),
                "--installation-id" => installation_id = Some(iter.next().ok_or_else(value)?),
                "--profile-key" => profile_key = Some(iter.next().ok_or_else(value)?),
                "--boot-id" => boot_id = Some(iter.next().ok_or_else(value)?),
                "--expect-marker" => expect_marker = Some(iter.next().ok_or_else(value)?),
                "--help" | "-h" => unreachable!("handled before parsing"),
                other => bail!("未知参数: {other}"),
            }
        }
        let family = family.context("--family 是必需的")?;
        if family != "chrome" && family != "edge" {
            bail!("--family 只能是 chrome 或 edge");
        }
        let pipe = pipe.context("--pipe 是必需的")?;
        let url = url.context("--url 是必需的")?;
        let parsed = reqwest::Url::parse(&url).context("--url 不是有效 URL")?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.username() != ""
            || parsed.password().is_some()
        {
            bail!("--url 只能使用无凭据 HTTP(S) 地址");
        }
        for (name, value) in [
            ("--extension-id", extension_id.as_deref()),
            ("--installation-id", installation_id.as_deref()),
            ("--profile-key", profile_key.as_deref()),
            ("--boot-id", boot_id.as_deref()),
        ] {
            if value.is_some_and(|value| value.is_empty() || value.len() > 128) {
                bail!("{name} 长度必须在 1..=128 字节范围内");
            }
        }
        if expect_marker
            .as_deref()
            .is_some_and(|marker| marker.is_empty() || marker.len() > 512)
        {
            bail!("--expect-marker 长度必须在 1..=512 字节范围内");
        }
        Ok(Self {
            family,
            pipe,
            url,
            extension_id,
            installation_id,
            profile_key,
            boot_id,
            expect_marker,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Options;

    #[test]
    fn parses_family_pipe_and_safe_http_url() {
        let options = Options::parse(vec![
            "--family".into(),
            "edge".into(),
            "--pipe".into(),
            r"\\.\pipe\rivo-browser-extension-edge".into(),
            "--url".into(),
            "http://127.0.0.1:18765/index.html".into(),
        ])
        .unwrap();
        assert_eq!(options.family, "edge");
        assert_eq!(options.pipe, r"\\.\pipe\rivo-browser-extension-edge");
    }

    #[test]
    fn parses_profile_identity_and_bounds_expected_marker() {
        let options = Options::parse(vec![
            "--family".into(),
            "chrome".into(),
            "--pipe".into(),
            r"\\.\pipe\rivo-browser-extension-chrome".into(),
            "--url".into(),
            "http://127.0.0.1:18765/index.html?profile=A".into(),
            "--extension-id".into(),
            "extension-id".into(),
            "--installation-id".into(),
            "install-a".into(),
            "--profile-key".into(),
            "profile-a".into(),
            "--boot-id".into(),
            "boot-a".into(),
            "--expect-marker".into(),
            "Rivo profile: A".into(),
        ])
        .unwrap();
        assert_eq!(options.installation_id.as_deref(), Some("install-a"));
        assert_eq!(options.profile_key.as_deref(), Some("profile-a"));
        assert_eq!(options.boot_id.as_deref(), Some("boot-a"));
        assert_eq!(options.expect_marker.as_deref(), Some("Rivo profile: A"));

        let mut oversized = vec![
            "--family".into(),
            "chrome".into(),
            "--pipe".into(),
            r"\\.\pipe\rivo-browser-extension-chrome".into(),
            "--url".into(),
            "http://127.0.0.1:18765/index.html".into(),
            "--expect-marker".into(),
        ];
        oversized.push("x".repeat(513));
        assert!(Options::parse(oversized).is_err());
    }

    #[test]
    fn rejects_non_http_and_credential_urls() {
        for url in [
            "file:///tmp/page.html",
            "javascript:alert(1)",
            "https://user:pass@example.test/",
        ] {
            assert!(Options::parse(vec![
                "--family".into(),
                "chrome".into(),
                "--pipe".into(),
                r"\\.\pipe\rivo-browser-extension-chrome".into(),
                "--url".into(),
                url.into(),
            ])
            .is_err());
        }
    }
}
