//! 唯一模型目录、精确自动路由及受控配置提交路径。查询不联网、不落盘。
use super::*;
use fs2::FileExt;
use std::io::Write;

#[cfg(test)]
#[path = "model_routing_tests.rs"]
mod tests;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultCatalog {
    pub application_version: String,
    pub schema_version: u32,
    pub vendors: BTreeMap<String, VendorCatalogEntry>,
    pub models: BTreeMap<String, ModelCatalogEntry>,
    /// 仍可使用但计划下线的内置模型；至少保留一个发布版本后再移入 `retired_models`。
    #[serde(default)]
    pub deprecated_models: BTreeMap<String, ModelLifecycleNote>,
    /// 已从内置目录移除的模型 ID 墓碑。用于识别用户配置中残留的旧条目，永不参与路由。
    #[serde(default)]
    pub retired_models: BTreeMap<String, ModelLifecycleNote>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLifecycleNote {
    #[serde(default)]
    pub replaced_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelLifecycleStatus {
    Active,
    Deprecated,
    Retired,
}

/// 内置目录与本地配置不一致时给客户端的可操作提示；不阻断读取或运行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogWarning {
    pub code: &'static str,
    pub model: String,
    pub replaced_by: Option<String>,
}

pub fn read_default_catalog() -> Result<DefaultCatalog, SessionError> {
    toml::from_str(include_str!("defaults.toml")).map_err(config_parse_error)
}

/// 内置目录内容的稳定标识；客户端据此判断升级 runtime 后是否需要刷新模型列表。
pub fn default_catalog_revision() -> String {
    rivo_storage::sha256_text(include_str!("defaults.toml"))
}

/// 内置目录自身的生命周期一致性：弃用项必须仍在目录中，墓碑必须已移除，替代模型必须可用。
pub fn validate_default_catalog(catalog: &DefaultCatalog) -> Result<(), SessionError> {
    for (id, note) in &catalog.deprecated_models {
        if !catalog.models.contains_key(id) {
            return Err(invalid(format!("deprecated model {id} 不在内置目录中")));
        }
        validate_replacement(catalog, id, note)?;
    }
    for (id, note) in &catalog.retired_models {
        if catalog.models.contains_key(id) {
            return Err(invalid(format!("retired model {id} 仍在内置目录中")));
        }
        validate_replacement(catalog, id, note)?;
    }
    Ok(())
}

fn validate_replacement(
    catalog: &DefaultCatalog,
    id: &str,
    note: &ModelLifecycleNote,
) -> Result<(), SessionError> {
    match &note.replaced_by {
        Some(next)
            if !catalog.models.contains_key(next)
                || catalog.deprecated_models.contains_key(next) =>
        {
            Err(invalid(format!(
                "model {id} 的 replaced_by {next} 必须是未弃用的内置模型"
            )))
        }
        _ => Ok(()),
    }
}

/// 模型在内置目录中的生命周期；本地新增模型视为 active。
pub fn model_lifecycle(
    catalog: &DefaultCatalog,
    id: &str,
) -> (ModelLifecycleStatus, Option<String>) {
    if let Some(note) = catalog.retired_models.get(id) {
        (ModelLifecycleStatus::Retired, note.replaced_by.clone())
    } else if let Some(note) = catalog.deprecated_models.get(id) {
        (ModelLifecycleStatus::Deprecated, note.replaced_by.clone())
    } else {
        (ModelLifecycleStatus::Active, None)
    }
}

/// 列出本地配置中因内置目录升级而需要用户处理的条目。
pub fn catalog_warnings(file: &FileConfig) -> Result<Vec<CatalogWarning>, SessionError> {
    let catalog = read_default_catalog()?;
    let models = merged_model_catalog(file)?;
    let mut warnings = Vec::new();
    for id in file.models.keys() {
        if let (ModelLifecycleStatus::Retired, replaced_by) = model_lifecycle(&catalog, id) {
            warnings.push(CatalogWarning {
                code: "retired_local_model",
                model: id.clone(),
                replaced_by,
            });
        }
    }
    if let Some(id) = &file.defaults.model {
        let (status, replaced_by) = model_lifecycle(&catalog, id);
        let code = match status {
            _ if !models.contains_key(id) => Some("default_model_missing"),
            ModelLifecycleStatus::Retired => Some("default_model_retired"),
            ModelLifecycleStatus::Deprecated => Some("default_model_deprecated"),
            ModelLifecycleStatus::Active => None,
        };
        if let Some(code) = code {
            warnings.push(CatalogWarning {
                code,
                model: id.clone(),
                replaced_by,
            });
        }
    }
    Ok(warnings)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyFlatConfig {
    #[serde(default)]
    version: Option<u32>,
    provider: ProviderKind,
    model: String,
    base_url: String,
    #[serde(default)]
    credential_name: Option<String>,
    #[serde(default)]
    capability_profile: Option<CapabilityProfile>,
    #[serde(default)]
    policy_profile: Option<PolicyProfile>,
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    api_key_env: Option<String>,
}

fn migrate_legacy_flat_config(content: &str) -> Result<FileConfig, SessionError> {
    let legacy: LegacyFlatConfig = toml::from_str(content).map_err(|_| {
        invalid("旧配置无法安全升级；请移除无法映射的字段后重试，程序不会覆盖原文件")
    })?;
    if !matches!(legacy.version, None | Some(3)) {
        return Err(invalid("不支持的旧配置版本"));
    }
    if legacy.api_key_env.is_some() {
        return Err(invalid(
            "旧 api_key_env 无法安全升级；请在运行时使用 --api-key-env，程序不会覆盖原文件",
        ));
    }
    let builtin = read_default_catalog()?
        .models
        .get(&legacy.model)
        .cloned()
        .ok_or_else(|| {
            invalid("旧配置的 model 不在内置目录中，无法确定 vendor；程序不会覆盖原文件")
        })?;
    let vendor = builtin.vendor.clone();
    let provider_id = legacy.provider.to_string();
    let mut config = FileConfig::default();
    // 旧扁平配置的协议选择迁移为本地模型协议顺序覆盖，Provider 不再携带协议。
    if builtin.supported_protocols.contains(&legacy.provider)
        && builtin.supported_protocols.first() != Some(&legacy.provider)
    {
        let mut model = builtin;
        model.supported_protocols.retain(|p| *p != legacy.provider);
        model.supported_protocols.insert(0, legacy.provider);
        config.models.insert(legacy.model.clone(), model);
    }
    config.defaults.vendor = Some(vendor);
    config.defaults.model = Some(legacy.model);
    config.defaults.provider_id = Some(provider_id.clone());
    config.defaults.capability = legacy.capability_profile;
    config.defaults.policy = legacy.policy_profile;
    config.defaults.workspace = legacy.workspace;
    config.providers.insert(
        provider_id,
        ProviderConfig {
            base_url: legacy.base_url,
            auth: AuthConfig {
                credential: legacy.credential_name,
            },
            enabled: true,
            priority: 0,
            discovery: ModelDiscoveryCache::default(),
        },
    );
    Ok(config)
}

fn migrate_schema3_routing_config(
    value: &toml::Value,
) -> Result<Option<toml::Value>, SessionError> {
    ensure_explicit_model_protocols(value)?;
    let mut value = value.clone();
    let Some(table) = value.as_table_mut() else {
        return Ok(None);
    };
    if table.get("version").and_then(toml::Value::as_integer) != Some(3) {
        return Ok(None);
    }
    if table.contains_key("provider") || table.contains_key("base_url") {
        return Ok(None);
    }
    if let Some(supervision) = table.remove("supervision") {
        let supervision = supervision
            .as_table()
            .ok_or_else(|| invalid("旧 supervision 配置无法安全升级；程序不会覆盖原文件"))?;
        if supervision.len() != 1
            || supervision.get("enabled") != Some(&toml::Value::Boolean(false))
        {
            return Err(invalid(
                "启用或扩展的旧 supervision 配置无法安全升级；程序不会覆盖原文件",
            ));
        }
    }
    table.insert("version".into(), toml::Value::Integer(5));
    Ok(Some(value))
}

/// schema 5 的 Provider `protocol` 字段已移除：协议只由模型决定。
/// 旧 discovery 缓存若与旧连接指纹一致，则按新指纹重新绑定，避免无谓的重新发现。
fn migrate_schema5_value(mut value: toml::Value) -> Result<FileConfig, SessionError> {
    #[derive(Deserialize)]
    struct Schema5Provider {
        #[serde(default)]
        protocol: Option<ProviderKind>,
        base_url: String,
        #[serde(default)]
        auth: AuthConfig,
    }
    let failed = || invalid("schema 5 配置无法安全升级；程序不会覆盖原文件");
    let table = value.as_table_mut().ok_or_else(failed)?;
    table.insert(
        "version".into(),
        toml::Value::Integer(i64::from(CONFIG_VERSION)),
    );
    if let Some(providers) = table.get_mut("providers") {
        let providers = providers.as_table_mut().ok_or_else(failed)?;
        for (id, entry) in providers.iter_mut() {
            let old: Schema5Provider = entry.clone().try_into().map_err(|_| failed())?;
            let old_fingerprint = old.protocol.map(|protocol| {
                rivo_storage::sha256_text(
                    &serde_json::json!([id, protocol, old.base_url, old.auth]).to_string(),
                )
            });
            let entry = entry.as_table_mut().ok_or_else(failed)?;
            entry.remove("protocol");
            if let Some(discovery) = entry
                .get_mut("discovery")
                .and_then(toml::Value::as_table_mut)
            {
                if discovery
                    .get("config_fingerprint")
                    .and_then(toml::Value::as_str)
                    .zip(old_fingerprint.as_deref())
                    .is_some_and(|(current, old)| current == old)
                {
                    discovery.insert(
                        "config_fingerprint".into(),
                        toml::Value::String(provider_identity_fingerprint(
                            id,
                            &old.base_url,
                            &old.auth,
                        )),
                    );
                }
            }
        }
    }
    value.try_into().map_err(|_| failed())
}

/// 内置身份不可重定义；本地可以覆盖 Model 协议列表与 priority。
pub fn merged_vendor_catalog(
    file: &FileConfig,
) -> Result<BTreeMap<String, VendorCatalogEntry>, SessionError> {
    let mut vendors = read_default_catalog()?.vendors;
    for (id, local) in &file.vendors {
        if vendors.get(id).is_some_and(|builtin| builtin != local) {
            return Err(invalid(format!("vendor {id} 不允许重定义内置身份")));
        }
        vendors.insert(id.clone(), local.clone());
    }
    Ok(vendors)
}

pub fn merged_model_catalog(
    file: &FileConfig,
) -> Result<BTreeMap<String, ModelCatalogEntry>, SessionError> {
    let mut models = read_default_catalog()?.models;
    for (id, local) in &file.models {
        if models.get(id).is_some_and(|builtin| {
            builtin.vendor != local.vendor || builtin.display_name != local.display_name
        }) {
            return Err(invalid(format!("model {id} 不允许重定义内置身份")));
        }
        models.insert(id.clone(), local.clone());
    }
    Ok(models)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Builtin,
    Local,
    BuiltinOverride,
}

pub fn model_catalog_sources(
    file: &FileConfig,
) -> Result<BTreeMap<String, CatalogSource>, SessionError> {
    let builtin = read_default_catalog()?;
    Ok(merged_model_catalog(file)?
        .keys()
        .map(|id| {
            let source = match (
                builtin.models.contains_key(id),
                file.models.contains_key(id),
            ) {
                (true, true) => CatalogSource::BuiltinOverride,
                (true, false) => CatalogSource::Builtin,
                _ => CatalogSource::Local,
            };
            (id.clone(), source)
        })
        .collect())
}

pub fn vendor_catalog_sources(
    file: &FileConfig,
) -> Result<BTreeMap<String, CatalogSource>, SessionError> {
    let builtin = read_default_catalog()?;
    Ok(merged_vendor_catalog(file)?
        .keys()
        .map(|id| {
            (
                id.clone(),
                if builtin.vendors.contains_key(id) {
                    CatalogSource::Builtin
                } else {
                    CatalogSource::Local
                },
            )
        })
        .collect())
}

fn invalid(message: impl Into<String>) -> SessionError {
    SessionError::InvalidConfig(message.into())
}

pub fn validate_provider_base_url(value: &str) -> Result<(), SessionError> {
    // 拒绝查询、片段、userinfo 和完整协议操作地址；私网/loopback 不受 Web policy 限制。
    let authority = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    if authority.is_empty()
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
        || value.contains(['@', '?', '#', '\\'])
        || authority.contains('%')
        || ["/responses", "/chat/completions", "/messages"]
            .iter()
            .any(|suffix| value.trim_end_matches('/').ends_with(suffix))
    {
        return Err(invalid(
            "Provider base_url 必须是无认证信息的 http(s) 协议路径前缀",
        ));
    }
    // 验证端口及 IPv6，而不是接受一个不可执行的连接地址。
    if let Some(authority) = authority.strip_prefix('[') {
        let Some((host, tail)) = authority.split_once(']') else {
            return Err(invalid("Provider base_url 主机无效"));
        };
        if host.parse::<std::net::Ipv6Addr>().is_err()
            || (!tail.is_empty()
                && tail
                    .strip_prefix(':')
                    .and_then(|p| p.parse::<u16>().ok())
                    .is_none())
        {
            return Err(invalid("Provider base_url 主机或端口无效"));
        }
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(h, p)| (h, Some(p)));
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
            || port.is_some_and(|p| p.parse::<u16>().is_err())
        {
            return Err(invalid("Provider base_url 主机或端口无效"));
        }
    }
    Ok(())
}

/// 严格校验，用于写入与显式 validate：`defaults.model` 必须存在于合并目录且属于默认 Vendor。
pub fn validate_model_routing_config(file: &FileConfig) -> Result<(), SessionError> {
    validate_model_routing_structure(file)?;
    validate_default_model(file)
}

/// 读取与运行路径只校验结构。内置目录升级后旧的 `defaults.model` 不应阻断显式选择了
/// 其他模型的会话；未覆盖时由路由解析报告同样明确的错误。
pub(crate) fn validate_model_routing_structure(file: &FileConfig) -> Result<(), SessionError> {
    if file.version != CONFIG_VERSION {
        return Err(invalid(format!(
            "必须使用 schema {CONFIG_VERSION}，请重新配置"
        )));
    }
    let vendors = merged_vendor_catalog(file)?;
    let models = merged_model_catalog(file)?;
    for (id, vendor) in &vendors {
        if id.trim().is_empty() || vendor.display_name.trim().is_empty() {
            return Err(invalid("Vendor ID/display_name 不能为空"));
        }
    }
    for (id, model) in &models {
        if id.trim().is_empty()
            || model.display_name.trim().is_empty()
            || model.priority > 1000
            || !vendors.contains_key(&model.vendor)
            || model.supported_protocols.is_empty()
            || model
                .supported_protocols
                .iter()
                .enumerate()
                .any(|(i, p)| model.supported_protocols[..i].contains(p))
        {
            return Err(invalid(format!(
                "model {id} 的身份、supported_protocols 或 priority 无效"
            )));
        }
    }
    for (id, provider) in &file.providers {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || provider.priority > 1000
        {
            return Err(invalid("Provider ID 或 priority 无效"));
        }
        validate_provider_base_url(&provider.base_url)?;
        if let Some(name) = &provider.auth.credential {
            validate_credential_name(name)?;
        }
    }
    if file
        .defaults
        .vendor
        .as_ref()
        .is_some_and(|v| !vendors.contains_key(v))
    {
        return Err(invalid("defaults.vendor 不存在"));
    }
    if file
        .defaults
        .provider_id
        .as_ref()
        .is_some_and(|id| !file.providers.contains_key(id))
    {
        return Err(invalid("defaults.provider_id 不存在"));
    }
    validate_budget(&file.defaults.budget)?;
    if file.defaults.max_subagents == 0 || !(1..=30).contains(&file.defaults.logging.retention_days)
    {
        return Err(invalid("defaults 子代理或日志限制无效"));
    }
    Ok(())
}

fn validate_default_model(file: &FileConfig) -> Result<(), SessionError> {
    let Some(model) = &file.defaults.model else {
        return Ok(());
    };
    let models = merged_model_catalog(file)?;
    match models.get(model) {
        None => Err(unknown_default_model(
            &models,
            model,
            file.defaults.vendor.as_deref(),
        )),
        Some(entry) if Some(entry.vendor.as_str()) != file.defaults.vendor.as_deref() => {
            Err(invalid("defaults.model 必须属于 defaults.vendor"))
        }
        Some(_) => Ok(()),
    }
}

fn unknown_default_model(
    models: &BTreeMap<String, ModelCatalogEntry>,
    model: &str,
    vendor: Option<&str>,
) -> SessionError {
    let available = models
        .iter()
        .filter(|(_, entry)| entry.enabled && Some(entry.vendor.as_str()) == vendor)
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    let hint = if available.is_empty() {
        String::new()
    } else {
        format!("，或改用：{}", available.join(", "))
    };
    let retired = read_default_catalog()
        .ok()
        .and_then(|catalog| catalog.retired_models.get(model).cloned())
        .map(|note| match note.replaced_by {
            Some(next) => format!("（该模型已从内置目录下线，建议改用 {next}）"),
            None => "（该模型已从内置目录下线）".into(),
        })
        .unwrap_or_default();
    invalid(format!(
        "defaults.model {model} 不在模型目录中{retired}；请在 [models] 中声明该模型{hint}"
    ))
}

pub fn provider_connection_fingerprint(id: &str, provider: &ProviderConfig) -> String {
    provider_identity_fingerprint(id, &provider.base_url, &provider.auth)
}

fn provider_identity_fingerprint(id: &str, base_url: &str, auth: &AuthConfig) -> String {
    let identity = serde_json::json!([id, base_url, auth]);
    rivo_storage::sha256_text(&identity.to_string())
}

/// 模型的首选协议：协议列表第一项。结构校验保证列表非空且无重复。
pub fn preferred_protocol(model: &ModelCatalogEntry) -> Option<ProviderKind> {
    model.supported_protocols.first().copied()
}

pub fn provider_config_fingerprint(file: &FileConfig, id: &str) -> Option<String> {
    file.providers
        .get(id)
        .map(|p| provider_connection_fingerprint(id, p))
}

pub fn provider_discovery_matches_current_config(id: &str, provider: &ProviderConfig) -> bool {
    provider.discovery.config_fingerprint.as_deref()
        == Some(provider_connection_fingerprint(id, provider).as_str())
}

fn available(
    id: &str,
    provider: &ProviderConfig,
    model_id: &str,
    model: &ModelCatalogEntry,
) -> bool {
    provider.enabled
        && !model.supported_protocols.is_empty()
        && provider.discovery.supported == Some(true)
        && provider.discovery.last_error.is_none()
        && provider.discovery.last_checked_at.is_some()
        && provider_discovery_matches_current_config(id, provider)
        && provider
            .discovery
            .models
            .iter()
            .any(|known| known == model_id)
}

/// None 表示 Auto；默认 Provider 由调用方显式传入，避免无法覆盖默认为 Auto。
pub fn resolve_available_route(
    file: &FileConfig,
    selected_vendor: Option<&str>,
    selected_model: Option<&str>,
    selected_provider: Option<&str>,
) -> Result<ResolvedRoute, SessionError> {
    validate_model_routing_structure(file)?;
    let vendor = selected_vendor
        .or(file.defaults.vendor.as_deref())
        .ok_or_else(|| invalid("必须选择 vendor"))?;
    let models = merged_model_catalog(file)?;
    let model_id = selected_model
        .or(file.defaults.model.as_deref())
        .map(str::to_owned)
        .or_else(|| {
            models
                .iter()
                .filter(|(_, m)| m.vendor == vendor && m.enabled)
                .max_by(|(li, l), (ri, r)| l.priority.cmp(&r.priority).then_with(|| ri.cmp(li)))
                .map(|(id, _)| id.clone())
        })
        .ok_or_else(|| invalid("Vendor 没有模型目录项"))?;
    let Some(model) = models.get(&model_id) else {
        if file.defaults.model.as_deref() == Some(model_id.as_str()) {
            return Err(unknown_default_model(&models, &model_id, Some(vendor)));
        }
        return Err(invalid("Model 不属于所选 Vendor"));
    };
    if model.vendor != vendor {
        return Err(invalid("Model 不属于所选 Vendor"));
    }
    if !model.enabled {
        return Err(invalid(format!("Model 已禁用: {model_id}")));
    }
    let mut candidates = file
        .providers
        .iter()
        .filter(|(id, p)| {
            selected_provider.is_none_or(|selected| selected == id.as_str())
                && available(id, p, &model_id, model)
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|(li, l), (ri, r)| r.priority.cmp(&l.priority).then_with(|| li.cmp(ri)));
    let protocol = preferred_protocol(model)
        .ok_or_else(|| invalid(format!("Model {model_id} 没有可用协议")))?;
    let (id, provider) = candidates.first().ok_or_else(|| {
        invalid(match selected_provider {
            Some(id) => {
                format!("显式 Provider {id} 不可用或没有精确模型 {model_id}；不会 fallback")
            }
            None => format!("没有已验证且精确发现模型 {model_id} 的 Provider"),
        })
    })?;
    Ok(ResolvedRoute {
        vendor: vendor.into(),
        model: model_id,
        provider_id: (*id).clone(),
        protocol,
        base_url: provider.base_url.clone(),
        priority: provider.priority,
    })
}

pub fn model_providers(
    file: &FileConfig,
    vendor: &str,
    model: &str,
) -> Result<Vec<ResolvedRoute>, SessionError> {
    validate_model_routing_structure(file)?;
    if merged_model_catalog(file)?
        .get(model)
        .is_none_or(|m| m.vendor != vendor)
    {
        return Err(invalid("Model 不属于 Vendor"));
    }
    let mut routes = file
        .providers
        .keys()
        .filter_map(|id| resolve_available_route(file, Some(vendor), Some(model), Some(id)).ok())
        .collect::<Vec<_>>();
    routes.sort_by(|l, r| {
        r.priority
            .cmp(&l.priority)
            .then_with(|| l.provider_id.cmp(&r.provider_id))
    });
    Ok(routes)
}

pub fn provider_models(file: &FileConfig, id: &str) -> Result<Vec<AffectedModel>, SessionError> {
    validate_model_routing_structure(file)?;
    let p = file
        .providers
        .get(id)
        .ok_or_else(|| invalid("Provider 不存在"))?;
    Ok(merged_model_catalog(file)?
        .into_iter()
        .filter(|(model, entry)| available(id, p, model, entry))
        .map(|(model, entry)| AffectedModel {
            vendor: entry.vendor,
            model,
        })
        .collect())
}

fn read_source(path: &Path) -> Result<Option<String>, SessionError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SessionError::ConfigRead {
            path: path.into(),
            source,
        }),
    }
}

/// Upgrade a supported historical configuration format before a normal
/// write-capable runtime path uses it. Read-only config queries keep using
/// `read_file_config` and therefore never modify user files.
pub fn upgrade_file_config(path: &Path) -> Result<FileConfig, SessionError> {
    if let Some(content) = injected_config_content() {
        return Ok(parse_injected_config(&content)?.config);
    }
    with_commit_lock(path, |path| {
        let source = read_source(path)?;
        let Some(content) = source.as_deref() else {
            return Ok(FileConfig::default());
        };
        let value: toml::Value = toml::from_str(content).map_err(config_parse_error)?;
        let version = value.get("version").and_then(toml::Value::as_integer);
        let config = parse_supported_file_config(content)?;
        if version != Some(i64::from(CONFIG_VERSION)) {
            atomic_replace(path, &source_revision(Some(content)), &config)?;
        }
        Ok(config)
    })
}

fn parse_supported_file_config(content: &str) -> Result<FileConfig, SessionError> {
    let value: toml::Value = toml::from_str(content).map_err(config_parse_error)?;
    let version = value.get("version").and_then(toml::Value::as_integer);
    if version == Some(i64::from(CONFIG_VERSION)) {
        return parse_file_config(content);
    }
    if let Some(version) = version.filter(|version| *version > i64::from(CONFIG_VERSION)) {
        return Err(invalid(format!(
            "配置版本 {version} 高于当前支持的版本 {CONFIG_VERSION}；程序不会覆盖原文件"
        )));
    }
    let config = if matches!(version, Some(4 | 5)) {
        // schema 4 与 5 只差版本号；二者都在此移除 Provider 协议。
        ensure_explicit_model_protocols(&value)?;
        migrate_schema5_value(value)?
    } else {
        match migrate_schema3_routing_config(&value)? {
            Some(value) => migrate_schema5_value(value)?,
            None => migrate_legacy_flat_config(content)?,
        }
    };
    validate_model_routing_config(&config)?;
    Ok(config)
}

fn ensure_explicit_model_protocols(value: &toml::Value) -> Result<(), SessionError> {
    let Some(models) = value.get("models") else {
        return Ok(());
    };
    let Some(models) = models.as_table() else {
        return Err(invalid("历史 models 配置无法安全升级；程序不会覆盖原文件"));
    };
    for (id, entry) in models {
        let Some(entry) = entry.as_table() else {
            return Err(invalid(format!(
                "历史 model {id} 缺少 supported_protocols；程序不会覆盖原文件"
            )));
        };
        if !entry.contains_key("supported_protocols") {
            return Err(invalid(format!(
                "历史 model {id} 缺少 supported_protocols；无法安全推断协议，程序不会覆盖原文件"
            )));
        }
    }
    Ok(())
}

fn source_revision(source: Option<&str>) -> String {
    // 缺文件与空文件使用不同 revision；已存在文件严格哈希原始 UTF-8 字节。
    source
        .map(rivo_storage::sha256_text)
        .unwrap_or_else(|| "missing".into())
}

pub fn read_model_routing_draft(path: &Path) -> Result<ModelRoutingDraft, SessionError> {
    if let Some(content) = injected_config_content() {
        return parse_injected_config(&content);
    }
    let source = read_source(path)?;
    let config = source
        .as_deref()
        .map(parse_supported_file_config)
        .transpose()?
        .unwrap_or_default();
    Ok(ModelRoutingDraft {
        revision: source_revision(source.as_deref()),
        config,
    })
}

/// 解析 `RIVO_CONFIG_CONTENT` 注入的主配置文档。注入态没有可写回的文件，
/// 因此不接受需要升级的旧 schema，只接受当前 schema。
fn parse_injected_config(content: &str) -> Result<ModelRoutingDraft, SessionError> {
    Ok(ModelRoutingDraft {
        revision: rivo_storage::sha256_text(content),
        config: parse_file_config(content)?,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoutingChangePlan {
    pub revision: String,
    pub confirmation_token: String,
    pub provider_impacts: Vec<ModelRoutingDeletePlan>,
}

fn identity_path(path: &Path) -> Result<PathBuf, SessionError> {
    let path = std::path::absolute(path).map_err(|source| SessionError::ConfigRead {
        path: path.into(),
        source,
    })?;
    // 缺失多级目录时也从已存在祖先取得规范前缀，避免 Windows 在
    // create_dir_all 前后切换到 \\?\ 路径，导致同一文件的 token/锁身份漂移。
    let mut ancestor = path.as_path();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                if ancestor != path {
                    canonical.push(
                        path.strip_prefix(ancestor)
                            .map_err(|_| invalid("配置路径标识无效"))?,
                    );
                }
                return Ok(canonical);
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| invalid("配置路径缺少已存在的祖先目录"))?;
            }
            Err(source) => {
                return Err(SessionError::ConfigRead {
                    path: ancestor.into(),
                    source,
                });
            }
        }
    }
}

fn token(
    path: &Path,
    revision: &str,
    operation: &str,
    payload: &impl Serialize,
) -> Result<String, SessionError> {
    let value = serde_json::json!([
        identity_path(path)?.to_string_lossy(),
        revision,
        operation,
        payload
    ]);
    Ok(rivo_storage::sha256_text(&value.to_string()))
}

fn provider_impact(
    file: &FileConfig,
    id: &str,
) -> Result<(Vec<AffectedModel>, bool), SessionError> {
    let p = file
        .providers
        .get(id)
        .ok_or_else(|| invalid("Provider 不存在"))?;
    let affected = merged_model_catalog(file)?
        .into_iter()
        .filter(|(model, entry)| {
            p.discovery.models.contains(model) && !entry.supported_protocols.is_empty()
        })
        .map(|(model, entry)| AffectedModel {
            vendor: entry.vendor,
            model,
        })
        .collect();
    let complete = p.discovery.supported == Some(true)
        && p.discovery.last_error.is_none()
        && p.discovery.last_checked_at.is_some()
        && provider_discovery_matches_current_config(id, p);
    Ok((affected, complete))
}

fn delete_plan(
    path: &Path,
    draft: &ModelRoutingDraft,
    kind: &str,
    id: &str,
) -> Result<ModelRoutingDeletePlan, SessionError> {
    let builtin = read_default_catalog()?;
    let (affected_models, impact_complete, clears_default_route) = match kind {
        "provider" => {
            let (models, complete) = provider_impact(&draft.config, id)?;
            (
                models,
                complete,
                draft.config.defaults.provider_id.as_deref() == Some(id),
            )
        }
        "model" if draft.config.models.contains_key(id) && !builtin.models.contains_key(id) => (
            vec![AffectedModel {
                vendor: draft.config.models[id].vendor.clone(),
                model: id.into(),
            }],
            true,
            draft.config.defaults.model.as_deref() == Some(id),
        ),
        "vendor" if draft.config.vendors.contains_key(id) && !builtin.vendors.contains_key(id) => {
            let affected = merged_model_catalog(&draft.config)?
                .into_iter()
                .filter(|(_, m)| m.vendor == id)
                .map(|(model, _)| AffectedModel {
                    vendor: id.into(),
                    model,
                })
                .collect();
            (
                affected,
                true,
                draft.config.defaults.vendor.as_deref() == Some(id),
            )
        }
        _ => return Err(invalid("删除目标不存在或为内置目录项")),
    };
    let mut plan = ModelRoutingDeletePlan {
        revision: draft.revision.clone(),
        confirmation_token: String::new(),
        target_kind: kind.into(),
        target_id: id.into(),
        affected_models,
        impact_complete,
        clears_default_route,
    };
    plan.confirmation_token = token(path, &draft.revision, "delete", &plan)?;
    Ok(plan)
}

pub fn plan_model_routing_delete(
    path: &Path,
    kind: &str,
    id: &str,
) -> Result<ModelRoutingDeletePlan, SessionError> {
    delete_plan(path, &read_model_routing_draft(path)?, kind, id)
}

pub fn plan_model_routing_change(
    path: &Path,
    revision: &str,
    config: &FileConfig,
) -> Result<ModelRoutingChangePlan, SessionError> {
    let current = read_model_routing_draft(path)?;
    check_revision(&current, revision)?;
    validate_model_routing_config(config)?;
    let provider_impacts = current
        .config
        .providers
        .iter()
        .filter(|(id, p)| {
            config.providers.get(*id).is_none_or(|next| {
                provider_connection_fingerprint(id, p) != provider_connection_fingerprint(id, next)
            })
        })
        .map(|(id, _)| delete_plan(path, &current, "provider", id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ModelRoutingChangePlan {
        revision: revision.into(),
        confirmation_token: token(path, revision, "change", config)?,
        provider_impacts,
    })
}

fn check_revision(current: &ModelRoutingDraft, expected: &str) -> Result<(), SessionError> {
    if current.revision != expected {
        return Err(SessionError::RevisionConflict);
    }
    Ok(())
}

fn with_commit_lock<T>(
    path: &Path,
    operation: impl FnOnce(&Path) -> Result<T, SessionError>,
) -> Result<T, SessionError> {
    if injected_config_content().is_some() {
        return Err(invalid(
            "RIVO_CONFIG_CONTENT 注入的配置为只读；请移除该环境变量后再写入配置",
        ));
    }
    let path = identity_path(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| SessionError::ConfigRead {
        path: parent.into(),
        source,
    })?;
    let lock_path = path.with_extension("toml.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|source| SessionError::ConfigRead {
            path: lock_path.clone(),
            source,
        })?;
    lock.lock_exclusive()
        .map_err(|source| SessionError::ConfigRead {
            path: lock_path,
            source,
        })?;
    // File drop releases the cross-process lock, including on failure.
    operation(&path)
}

fn atomic_replace(
    path: &Path,
    expected: &str,
    config: &FileConfig,
) -> Result<ModelRoutingDraft, SessionError> {
    let content = toml::to_string_pretty(config)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| SessionError::ConfigRead {
            path: parent.into(),
            source,
        })?;
    tmp.write_all(content.as_bytes())
        .and_then(|_| tmp.as_file().sync_all())
        .map_err(|source| SessionError::ConfigRead {
            path: path.into(),
            source,
        })?;
    if source_revision(read_source(path)?.as_deref()) != expected {
        return Err(invalid("替换前源文件变化，提交取消"));
    }
    tmp.persist(path).map_err(|e| SessionError::ConfigRead {
        path: path.into(),
        source: e.error,
    })?;
    Ok(ModelRoutingDraft {
        revision: rivo_storage::sha256_text(&content),
        config: config.clone(),
    })
}

/// 未经确认只允许非破坏性编辑；发现证据只能由显式 refresh 路径产生。
pub fn commit_model_routing_draft(
    path: &Path,
    revision: &str,
    config: &FileConfig,
) -> Result<ModelRoutingDraft, SessionError> {
    commit_model_routing_change(path, revision, config, None)
}

pub fn commit_model_routing_change(
    path: &Path,
    revision: &str,
    config: &FileConfig,
    confirmation: Option<&str>,
) -> Result<ModelRoutingDraft, SessionError> {
    with_commit_lock(path, |path| {
        let current = read_model_routing_draft(path)?;
        check_revision(&current, revision)?;
        validate_model_routing_config(config)?;
        // 删除本地对象必须使用专用计划路径。移除内置 Model 的本地覆盖
        // 只会恢复内置目录默认值，不会删除模型身份。
        let builtin_models = read_default_catalog()?.models;
        if current
            .config
            .providers
            .keys()
            .any(|id| !config.providers.contains_key(id))
            || current
                .config
                .models
                .keys()
                .any(|id| !config.models.contains_key(id) && !builtin_models.contains_key(id))
            || current
                .config
                .vendors
                .keys()
                .any(|id| !config.vendors.contains_key(id))
        {
            return Err(invalid("删除对象必须使用 delete plan/commit"));
        }
        let plan = plan_model_routing_change(path, revision, config)?;
        if !plan.provider_impacts.is_empty()
            && confirmation != Some(plan.confirmation_token.as_str())
        {
            return Err(invalid("连接变更需要有效的影响计划确认 token"));
        }
        let mut next = config.clone();
        for (id, p) in &mut next.providers {
            let old = current.config.providers.get(id);
            if old
                .map(|old| &old.discovery)
                .unwrap_or(&ModelDiscoveryCache::default())
                != &p.discovery
            {
                return Err(invalid("discovery 是受管证据，只能显式 refresh"));
            }
            if old.is_some_and(|old| {
                provider_connection_fingerprint(id, old) != provider_connection_fingerprint(id, p)
            }) {
                p.discovery = ModelDiscoveryCache::default();
            }
        }
        atomic_replace(path, revision, &next)
    })
}

pub fn delete_model_routing_config(
    path: &Path,
    plan: &ModelRoutingDeletePlan,
    confirmation: &str,
) -> Result<ModelRoutingDraft, SessionError> {
    with_commit_lock(path, |path| {
        let mut current = read_model_routing_draft(path)?;
        check_revision(&current, &plan.revision)?;
        let expected = delete_plan(path, &current, &plan.target_kind, &plan.target_id)?;
        let actual_value = serde_json::to_value(plan).map_err(|_| invalid("删除计划序列化失败"))?;
        let expected_value =
            serde_json::to_value(&expected).map_err(|_| invalid("删除计划序列化失败"))?;
        if confirmation != expected.confirmation_token || actual_value != expected_value {
            return Err(invalid("删除计划或确认 token 无效"));
        }
        match plan.target_kind.as_str() {
            "provider" => {
                current.config.providers.remove(&plan.target_id);
                if plan.clears_default_route {
                    current.config.defaults.provider_id = None;
                }
            }
            "model" => {
                current.config.models.remove(&plan.target_id);
                if plan.clears_default_route {
                    current.config.defaults.model = None;
                }
            }
            "vendor" => {
                if !plan.affected_models.is_empty() {
                    return Err(invalid("Vendor 仍有 Model 引用，必须先逐一删除 Model"));
                }
                current.config.vendors.remove(&plan.target_id);
                if plan.clears_default_route {
                    current.config.defaults.vendor = None;
                    current.config.defaults.model = None;
                }
            }
            _ => return Err(invalid("删除目标类型无效")),
        }
        validate_model_routing_config(&current.config)?;
        atomic_replace(path, &plan.revision, &current.config)
    })
}

/// 唯一可产生 discovery 成功证据的公开入口：执行真实 authenticated discovery。
/// 失败也撤销旧成功状态；网络期间不持有提交锁，返回后按 revision 提交。
pub async fn refresh_provider_models(
    path: &Path,
    revision: &str,
    id: &str,
) -> Result<ModelRoutingDraft, SessionError> {
    let mut draft = read_model_routing_draft(path)?;
    check_revision(&draft, revision)?;
    let provider = draft
        .config
        .providers
        .get(id)
        .ok_or_else(|| invalid("Provider 不存在"))?
        .clone();
    let fingerprint = provider_connection_fingerprint(id, &provider);
    let discovery = match provider_api_key(path, &provider.auth, None) {
        Ok(api_key) => {
            rivo_llm::discover_models(LlmProviderConfig {
                base_url: provider.base_url.clone(),
                api_key,
                timeout_seconds: 120,
                max_stream_event_bytes: 1024 * 1024,
            })
            .await
        }
        Err(error) => {
            let p = draft.config.providers.get_mut(id).expect("checked");
            p.discovery = ModelDiscoveryCache {
                config_fingerprint: Some(fingerprint),
                supported: None,
                last_checked_at: Some(now_seconds()),
                last_error: Some("Provider 认证引用不可用".into()),
                models: Vec::new(),
            };
            with_commit_lock(path, |path| {
                check_revision(&read_model_routing_draft(path)?, revision)?;
                atomic_replace(path, revision, &draft.config)
            })?;
            return Err(error);
        }
    };
    let (models, supported, last_error) = match discovery {
        Ok(models) => (
            filter_discovered_models(&draft.config, models)?,
            Some(true),
            None,
        ),
        // 仅 404 证明发现接口不受支持；认证、传输及响应错误均为未知。
        Err(error) => {
            let message = match error.http_status {
                Some(401 | 403) => {
                    "Provider 认证失败（HTTP 401/403）；请检查 Credential 权限".into()
                }
                Some(404) => "Provider 模型发现接口不支持（HTTP 404）".into(),
                _ => "Provider 模型发现失败；请检查连接与认证".into(),
            };
            (
                Vec::new(),
                (error.http_status == Some(404)).then_some(false),
                Some(message),
            )
        }
    };
    draft
        .config
        .providers
        .get_mut(id)
        .expect("checked")
        .discovery = ModelDiscoveryCache {
        config_fingerprint: Some(fingerprint),
        models,
        supported,
        last_checked_at: Some(now_seconds()),
        last_error,
    };
    with_commit_lock(path, |path| {
        check_revision(&read_model_routing_draft(path)?, revision)?;
        atomic_replace(path, revision, &draft.config)
    })
}

/// 执行一次只读的 authenticated discovery，返回远端原始模型目录。
///
/// 与 `refresh_provider_models` 不同，此入口不会过滤本地目录，也不会
/// 更新 Provider discovery 缓存或写入任何配置文件。
pub async fn discover_provider_models(path: &Path, id: &str) -> Result<Vec<String>, SessionError> {
    let draft = read_model_routing_draft(path)?;
    let provider = draft
        .config
        .providers
        .get(id)
        .ok_or_else(|| invalid("Provider 不存在"))?
        .clone();
    let api_key = provider_api_key(path, &provider.auth, None)?;
    Ok(rivo_llm::discover_models(LlmProviderConfig {
        base_url: provider.base_url,
        api_key,
        timeout_seconds: 120,
        max_stream_event_bytes: 1024 * 1024,
    })
    .await?)
}

fn filter_discovered_models(
    file: &FileConfig,
    discovered: Vec<String>,
) -> Result<Vec<String>, SessionError> {
    let catalog = merged_model_catalog(file)?;
    Ok(discovered
        .into_iter()
        .filter(|id| {
            catalog
                .get(id)
                .is_some_and(|model| model.enabled && !model.supported_protocols.is_empty())
        })
        .collect())
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn provider_api_key(
    path: &Path,
    auth: &AuthConfig,
    runtime_env: Option<&str>,
) -> Result<String, SessionError> {
    if let Some(env) = runtime_env {
        if let Ok(value) = std::env::var(env) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }
    if let Some(name) = &auth.credential {
        return get_credential(path, name)?
            .ok_or_else(|| SessionError::MissingApiKey(name.clone()));
    }
    Ok(String::new())
}
