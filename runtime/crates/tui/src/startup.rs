use crate::{CatalogModel, CatalogProvider, StartupSelection};
use rivo_session::{
    merged_model_catalog, merged_vendor_catalog, model_providers, resolve_available_route,
    CapabilityProfile, FileConfig, ModelSelection, PolicyProfile, ProviderSelection, ResolvedRoute,
    TurnOptions,
};
use std::collections::BTreeMap;

/// 纯离线展示投影；可用性、精确 ID 匹配及 Provider 排序只由 Session 决定。
pub fn catalog_models(file: &FileConfig) -> Result<Vec<CatalogModel>, String> {
    merged_model_catalog(file)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(id, model)| {
            let providers = model_providers(file, &model.vendor, &id)
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|route| CatalogProvider {
                    id: route.provider_id,
                    status: "available".into(),
                    priority: route.priority,
                })
                .collect();
            Ok(CatalogModel {
                id,
                vendor: model.vendor,
                display_name: model.display_name,
                providers,
                supported_reasoning_efforts: model.supported_reasoning_efforts,
            })
        })
        .collect()
}

/// 启动及会话内模型选择共用的内存草稿，不读写配置、凭据或网络。
/// 提交只校验该配置快照；宿主须交给 Session 再校验当前配置。
#[derive(Debug, Clone)]
pub struct StartupConfigState {
    config: FileConfig,
    pub catalog_models: Vec<CatalogModel>,
    pub catalog_vendors: Vec<String>,
    pub enabled_mcp_servers: Vec<String>,
    pub selected_vendor: Option<String>,
    pub model: Option<String>,
    pub provider: ProviderSelection,
    pub route: Option<ResolvedRoute>,
    pub turn_options: TurnOptions,
    pub selected_row: usize,
    pub notice: String,
}

impl StartupConfigState {
    pub const FIXED_ROWS: usize = 9;

    /// selection 为 Some 时原样保留（包括无效显式 Provider），绝不静默降级。
    /// None 使用配置默认项；缺默认 Vendor 时等待用户选择。
    pub fn new(
        config: FileConfig,
        enabled_mcp_servers: Vec<String>,
        selection: Option<ModelSelection>,
        mut turn_options: TurnOptions,
    ) -> Result<Self, String> {
        let catalog_models = catalog_models(&config)?;
        let catalog_vendors = merged_vendor_catalog(&config)
            .map_err(|error| error.to_string())?
            .into_keys()
            .collect();
        turn_options
            .mcp_servers
            .retain(|name| enabled_mcp_servers.contains(name));
        let (selected_vendor, model, provider) = match selection {
            Some(selection) => (
                Some(selection.vendor),
                Some(selection.model),
                selection.provider,
            ),
            None => (
                config.defaults.vendor.clone(),
                config.defaults.model.clone(),
                config
                    .defaults
                    .provider_id
                    .clone()
                    .map_or(ProviderSelection::Auto, |provider_id| {
                        ProviderSelection::Explicit { provider_id }
                    }),
            ),
        };
        let mut state = Self {
            config,
            catalog_models,
            catalog_vendors,
            enabled_mcp_servers,
            selected_vendor,
            model,
            provider,
            route: None,
            turn_options,
            selected_row: 0,
            notice: String::new(),
        };
        state.update_preview();
        Ok(state)
    }

    /// 宿主取得新配置后刷新目录；保留草稿选择，禁用/删除显式 Provider 必须失败。
    pub fn refresh_config(&mut self, config: FileConfig) -> Result<(), String> {
        // 先撤销旧预览，避免无效新配置仍显示旧路由可用。
        self.route = None;
        let result = rivo_session::validate_model_routing_config(&config)
            .map_err(|error| error.to_string())
            .and_then(|()| catalog_models(&config))
            .and_then(|models| {
                let vendors = merged_vendor_catalog(&config)
                    .map_err(|error| error.to_string())?
                    .into_keys()
                    .collect();
                Ok((models, vendors))
            });
        self.config = config;
        match result {
            Ok((models, vendors)) => {
                self.catalog_models = models;
                self.catalog_vendors = vendors;
                self.update_preview();
                Ok(())
            }
            Err(error) => {
                self.catalog_models.clear();
                self.catalog_vendors.clear();
                self.notice = error.clone();
                Err(error)
            }
        }
    }

    fn resolve(&self) -> Result<ResolvedRoute, String> {
        rivo_session::validate_model_routing_config(&self.config)
            .map_err(|error| error.to_string())?;
        let vendor = self
            .selected_vendor
            .as_deref()
            .ok_or_else(|| "请选择 Vendor；配置 Provider 并显式刷新模型目录后可运行".to_owned())?;
        // 不让旧 defaults.model 跨 Vendor 泄漏。None 时由 Session 选择最高优先级模型。
        let mut config = self.config.clone();
        config.defaults.model = None;
        resolve_available_route(
            &config,
            Some(vendor),
            self.model.as_deref(),
            self.provider.explicit_id(),
        )
        .map_err(|error| error.to_string())
    }

    fn update_preview(&mut self) {
        match self.resolve() {
            Ok(route) => {
                self.model = Some(route.model.clone());
                self.route = Some(route);
                self.notice.clear();
            }
            Err(error) => {
                self.route = None;
                self.notice = error;
            }
        }
    }

    pub fn row_count(&self) -> usize {
        Self::FIXED_ROWS + self.enabled_mcp_servers.len() + 1
    }

    pub fn move_selection(&mut self, delta: i16) {
        self.selected_row = self
            .selected_row
            .saturating_add_signed(isize::from(delta))
            .min(self.row_count() - 1);
    }

    /// 只有“开始”行返回 true；模式、policy 和所有附加能力始终可操作。
    pub fn change_current(&mut self, forward: bool) -> bool {
        match self.selected_row {
            0 => cycle(
                &mut self.turn_options.capability_profile,
                forward,
                &[
                    CapabilityProfile::General,
                    CapabilityProfile::Research,
                    CapabilityProfile::Plan,
                    CapabilityProfile::Coding,
                    CapabilityProfile::Review,
                ],
            ),
            1 => cycle(
                &mut self.turn_options.policy_profile,
                forward,
                &[
                    PolicyProfile::Strict,
                    PolicyProfile::Medium,
                    PolicyProfile::FullControl,
                ],
            ),
            2 => {
                cycle_optional(&mut self.selected_vendor, forward, &self.catalog_vendors);
                self.model = None;
                self.provider = ProviderSelection::Auto;
            }
            3 => {
                let models = self
                    .catalog_models
                    .iter()
                    .filter(|model| Some(&model.vendor) == self.selected_vendor.as_ref())
                    .map(|model| model.id.clone())
                    .collect::<Vec<_>>();
                cycle_optional(&mut self.model, forward, &models);
                self.provider = ProviderSelection::Auto;
            }
            4 => {
                let providers = self
                    .selected_vendor
                    .as_deref()
                    .zip(self.model.as_deref())
                    .map(|(vendor, model)| model_providers(&self.config, vendor, model))
                    .transpose();
                match providers {
                    Ok(providers) => {
                        // 使用枚举而不是保留字符串 "auto"：Provider ID 也可以叫 auto。
                        let choices = std::iter::once(ProviderSelection::Auto)
                            .chain(providers.into_iter().flatten().map(|route| {
                                ProviderSelection::Explicit {
                                    provider_id: route.provider_id,
                                }
                            }))
                            .collect::<Vec<_>>();
                        cycle(&mut self.provider, forward, &choices);
                    }
                    Err(error) => {
                        self.route = None;
                        self.notice = error.to_string();
                        return false;
                    }
                }
            }
            5 => self.turn_options.network = !self.turn_options.network,
            6 => self.turn_options.browser = !self.turn_options.browser,
            7 => self.turn_options.memory_recall = !self.turn_options.memory_recall,
            8 => self.turn_options.trace = !self.turn_options.trace,
            row if (Self::FIXED_ROWS..self.row_count() - 1).contains(&row) => {
                let name = self.enabled_mcp_servers[row - Self::FIXED_ROWS].clone();
                if let Some(index) = self
                    .turn_options
                    .mcp_servers
                    .iter()
                    .position(|s| s == &name)
                {
                    self.turn_options.mcp_servers.remove(index);
                } else {
                    self.turn_options.mcp_servers.push(name);
                }
            }
            row => return row == self.row_count() - 1,
        }
        self.update_preview();
        false
    }

    pub fn submit(&mut self) -> Result<StartupSelection, String> {
        self.update_preview();
        let route = self.route.as_ref().ok_or_else(|| self.notice.clone())?;
        self.turn_options
            .mcp_servers
            .retain(|name| self.enabled_mcp_servers.contains(name));
        Ok(StartupSelection {
            selection: ModelSelection {
                vendor: route.vendor.clone(),
                model: route.model.clone(),
                provider: self.provider.clone(),
            },
            turn_options: self.turn_options.clone(),
        })
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines_with(|_, fallback| fallback.to_owned())
    }

    /// Render startup copy through the active UI catalog. The legacy `lines()`
    /// method remains code-owned for deterministic unit fixtures; the live
    /// renderer calls this catalog-aware projection.
    pub fn lines_with_messages(&self, messages: &BTreeMap<String, String>) -> Vec<String> {
        self.lines_with(|id, fallback| {
            messages
                .get(id)
                .cloned()
                .unwrap_or_else(|| fallback.to_owned())
        })
    }

    fn lines_with<F>(&self, mut message: F) -> Vec<String>
    where
        F: FnMut(&str, &str) -> String,
    {
        let marker = |row| if row == self.selected_row { ">" } else { " " };
        let provider = match &self.provider {
            ProviderSelection::Auto => message("startup-provider-auto", "Auto"),
            ProviderSelection::Explicit { provider_id } => message_with(
                &mut message,
                "startup-provider-explicit",
                "显式 {$provider}",
                &[("provider", provider_id.clone())],
            ),
        };
        let mut lines = vec![
            message(
                "startup-selection-help",
                "Vendor → Model → Auto / 显式 Provider；Enter/Space 切换，←/→ 反向/正向选择。",
            ),
            String::new(),
            format!(
                "{} {}: {}",
                marker(0),
                message("startup-capability", "能力模式"),
                self.turn_options.capability_profile
            ),
            format!(
                "{} {}: {}",
                marker(1),
                message("startup-policy", "安全策略"),
                self.turn_options.policy_profile
            ),
            format!(
                "{} {}: {}",
                marker(2),
                message("startup-vendor", "Vendor"),
                self.selected_vendor
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| message("startup-unselected", "未选择"))
            ),
            format!(
                "{} {}: {}",
                marker(3),
                message("startup-model", "Model"),
                self.model
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| message("startup-model-default", "自动（目录默认）"))
            ),
            format!(
                "{} {}: {}",
                marker(4),
                message("startup-provider", "Provider 选择"),
                provider
            ),
            format!(
                "{} {}: {}",
                marker(5),
                message("startup-network", "网络"),
                self.turn_options.network
            ),
            format!(
                "{} {}: {}",
                marker(6),
                message("startup-browser", "浏览器"),
                self.turn_options.browser
            ),
            format!(
                "{} {}: {}",
                marker(7),
                message("startup-memory", "记忆召回"),
                self.turn_options.memory_recall
            ),
            format!(
                "{} {}: {}",
                marker(8),
                message("startup-trace", "Trace"),
                self.turn_options.trace
            ),
        ];
        for (index, name) in self.enabled_mcp_servers.iter().enumerate() {
            lines.push(format!(
                "{} {} {name}: [{}]",
                marker(Self::FIXED_ROWS + index),
                message("startup-mcp", "MCP"),
                if self.turn_options.mcp_servers.contains(name) {
                    "x"
                } else {
                    " "
                }
            ));
        }
        lines.push(format!(
            "{} {}",
            marker(self.row_count() - 1),
            message("startup-start", "开始")
        ));
        lines.push(match &self.route {
            Some(route) => message_with(
                &mut message,
                "startup-route-preview",
                "路由预览: {$vendor} / {$model} → {$provider} ({$protocol}, priority={$priority})",
                &[
                    ("vendor", route.vendor.clone()),
                    ("model", route.model.clone()),
                    ("provider", route.provider_id.clone()),
                    ("protocol", route.protocol.to_string()),
                    ("priority", route.priority.to_string()),
                ],
            ),
            None => message("startup-route-unresolved", "路由预览: 未解析；不能开始"),
        });
        if !self.notice.is_empty() {
            lines.push(format!(
                "{}: {}",
                message("startup-notice", "提示"),
                self.notice
            ));
        }
        lines
    }
}

fn message_with<F>(message: &mut F, id: &str, fallback: &str, values: &[(&str, String)]) -> String
where
    F: FnMut(&str, &str) -> String,
{
    let mut rendered = message(id, fallback);
    for (key, value) in values {
        rendered = rendered.replace(&format!("{{${key}}}"), value);
        rendered = rendered.replace(&format!("__RIVO_{key}__"), value);
    }
    rendered
        .chars()
        .filter(|character| *character != '\u{2068}' && *character != '\u{2069}')
        .collect()
}

fn cycle<T: Clone + PartialEq>(current: &mut T, forward: bool, choices: &[T]) {
    if choices.is_empty() {
        return;
    }
    let index = next_index(
        choices.iter().position(|choice| choice == current),
        choices.len(),
        forward,
    );
    *current = choices[index].clone();
}

fn cycle_optional(current: &mut Option<String>, forward: bool, choices: &[String]) {
    if choices.is_empty() {
        *current = None;
        return;
    }
    let index = next_index(
        choices
            .iter()
            .position(|choice| Some(choice) == current.as_ref()),
        choices.len(),
        forward,
    );
    *current = Some(choices[index].clone());
}

fn next_index(current: Option<usize>, count: usize, forward: bool) -> usize {
    match current {
        None => {
            if forward {
                0
            } else {
                count - 1
            }
        }
        Some(index) if forward => (index + 1) % count,
        Some(index) => (index + count - 1) % count,
    }
}
