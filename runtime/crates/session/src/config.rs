use rivo_policy::{CapabilityProfile, PolicyProfile};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    /// Canonical model publisher selected for this turn.
    pub vendor: Option<String>,
    /// API key environment variable name used only by this turn.
    pub api_key_env: Option<String>,
    /// Explicit provider connection ID for the catalog-based router.
    pub provider_id: Option<String>,
    /// Explicitly select Auto, overriding a configured default Provider.
    pub auto_provider: bool,
    pub model: Option<String>,
    pub capability_profile: Option<CapabilityProfile>,
    pub policy_profile: Option<PolicyProfile>,
    pub workspace: Option<PathBuf>,
    pub browser_cdp_url: Option<String>,
    /// UI and default response locale for this invocation.
    pub locale: Option<String>,
}

pub const CONFIG_VERSION: u32 = 6;

/// 环境变量注入的完整主配置文档（TOML 原文），供自动化测试免临时文件使用。
pub const CONFIG_CONTENT_ENV: &str = "RIVO_CONFIG_CONTENT";

/// Resolve the platform configuration path without touching or creating it.
pub fn config_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|path| path.join("rivo").join("config.toml"))
}

/// 读取注入的主配置文档；空值与未设置等价，与 `RIVO_DATA_DIR` 的空值语义一致。
pub fn injected_config_content() -> Option<String> {
    std::env::var_os(CONFIG_CONTENT_ENV)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string_lossy().into_owned())
}
