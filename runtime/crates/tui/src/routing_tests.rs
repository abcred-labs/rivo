use super::*;
use rivo_session::ProviderKind;
use rivo_session::{
    merged_model_catalog, model_providers, provider_connection_fingerprint, AuthConfig,
    CapabilityProfile, Intent, ModelCatalogEntry, ModelDiscoveryCache, PolicyProfile,
    ProviderConfig, VendorCatalogEntry,
};
use std::collections::BTreeSet;

/// 全新 schema 5 内存配置；不访问磁盘、凭据或真实 Provider。
pub(super) fn routing_config() -> FileConfig {
    let mut file = FileConfig::default();
    for vendor in ["test-a", "test-b"] {
        file.vendors.insert(
            vendor.into(),
            VendorCatalogEntry {
                display_name: vendor.into(),
            },
        );
    }
    for (id, vendor, priority) in [
        ("alpha-model", "test-a", 100),
        ("second-model", "test-a", 10),
        ("beta-model", "test-b", 100),
    ] {
        file.models.insert(
            id.into(),
            ModelCatalogEntry {
                vendor: vendor.into(),
                display_name: id.into(),
                supported_protocols: Vec::from([ProviderKind::OpenAiResponses]),
                supported_reasoning_efforts: BTreeSet::new(),
                enabled: true,
                priority,
            },
        );
    }
    for (id, priority) in [("alpha", 100), ("zeta", 100), ("low", 1)] {
        let mut provider = ProviderConfig {
            base_url: "https://routing.example/v1".into(),
            auth: AuthConfig::default(),
            enabled: true,
            priority,
            discovery: ModelDiscoveryCache::default(),
        };
        provider.discovery = ModelDiscoveryCache {
            config_fingerprint: Some(provider_connection_fingerprint(id, &provider)),
            supported: Some(true),
            models: vec![
                "alpha-model".into(),
                "second-model".into(),
                "beta-model".into(),
                "gpt-5".into(),
            ],
            last_checked_at: Some(1),
            last_error: None,
        };
        file.providers.insert(id.into(), provider);
    }
    file.defaults.vendor = Some("test-a".into());
    file.defaults.model = Some("alpha-model".into());
    file
}

fn selection(provider: ProviderSelection) -> ModelSelection {
    ModelSelection {
        vendor: "test-a".into(),
        model: "alpha-model".into(),
        provider,
    }
}

fn explicit(id: &str) -> ProviderSelection {
    ProviderSelection::Explicit {
        provider_id: id.into(),
    }
}

fn startup(file: FileConfig, selection: Option<ModelSelection>) -> StartupState {
    StartupState::new(
        file,
        vec!["docs".into(), "code".into()],
        selection,
        TurnOptions::default(),
    )
    .unwrap()
}

#[test]
fn catalog_is_session_projection_including_builtins_and_stable_provider_order() {
    let file = routing_config();
    let catalog = catalog_models(&file).unwrap();
    assert_eq!(catalog.len(), merged_model_catalog(&file).unwrap().len());
    assert!(catalog
        .iter()
        .any(|model| model.id == "gpt-5" && model.vendor == "openai"));
    for model in catalog {
        assert_eq!(
            model
                .providers
                .iter()
                .map(|p| p.id.clone())
                .collect::<Vec<_>>(),
            model_providers(&file, &model.vendor, &model.id)
                .unwrap()
                .iter()
                .map(|r| r.provider_id.clone())
                .collect::<Vec<_>>()
        );
    }
    let mut state = startup(file, None);
    assert_eq!(state.route.as_ref().unwrap().provider_id, "alpha");
    assert_eq!(
        state.submit().unwrap().selection.provider,
        ProviderSelection::Auto
    );
}

#[test]
fn auto_submission_never_freezes_resolved_provider_and_reselects_after_refresh() {
    let mut file = routing_config();
    let mut state = startup(file.clone(), None);
    assert_eq!(
        state.submit().unwrap().selection.provider,
        ProviderSelection::Auto
    );
    file.providers.get_mut("zeta").unwrap().priority = 500;
    state.refresh_config(file).unwrap();
    assert_eq!(state.route.as_ref().unwrap().provider_id, "zeta");
    let submitted = state.submit().unwrap();
    assert_eq!(submitted.selection.provider, ProviderSelection::Auto);
    // 会话内选择用同一提交产物，不从 route.provider_id 构造显式选择。
    let intent = Intent::SelectModel {
        selection: submitted.selection,
    };
    assert!(matches!(
        intent,
        Intent::SelectModel {
            selection: ModelSelection {
                provider: ProviderSelection::Auto,
                ..
            }
        }
    ));
}

#[test]
fn explicit_default_is_preserved_but_an_explicit_auto_intent_overrides_it() {
    let mut file = routing_config();
    file.defaults.provider_id = Some("low".into());
    let mut defaults = startup(file.clone(), None);
    assert_eq!(
        defaults.submit().unwrap().selection.provider,
        explicit("low")
    );
    assert_eq!(defaults.route.as_ref().unwrap().provider_id, "low");
    let mut auto = startup(file, Some(selection(ProviderSelection::Auto)));
    assert_eq!(
        auto.submit().unwrap().selection.provider,
        ProviderSelection::Auto
    );
    assert_eq!(auto.route.as_ref().unwrap().provider_id, "alpha");
}

#[test]
fn disabled_deleted_failed_and_stale_explicit_providers_fail_without_fallback() {
    for unavailable in 0..8 {
        let mut file = routing_config();
        let mut state = startup(file.clone(), Some(selection(explicit("alpha"))));
        let provider = file.providers.get_mut("alpha").unwrap();
        match unavailable {
            0 => provider.enabled = false,
            1 => provider.discovery = ModelDiscoveryCache::default(),
            2 => provider.discovery.supported = Some(false),
            3 => provider.discovery.last_error = Some("refresh failed".into()),
            4 => provider.base_url = "https://changed.example/v1".into(),
            5 => provider.discovery.last_checked_at = None,
            6 => provider.discovery.models.clear(),
            _ => {
                file.providers.remove("alpha");
            }
        }
        state.refresh_config(file).unwrap();
        assert!(state.route.is_none());
        let error = state.submit().unwrap_err();
        assert!(error.contains("显式 Provider alpha"), "{error}");
        assert!(error.contains("不会 fallback"));
        assert_eq!(state.provider, explicit("alpha"));
        assert!(state
            .catalog_models
            .iter()
            .find(|m| m.id == "alpha-model")
            .unwrap()
            .providers
            .iter()
            .all(|p| p.id != "alpha"));
        // 用户明确切回 Auto 才允许使用 zeta。
        state.selected_row = 4;
        state.change_current(true);
        assert_eq!(
            state.submit().unwrap().selection.provider,
            ProviderSelection::Auto
        );
        assert_eq!(state.route.as_ref().unwrap().provider_id, "zeta");
    }
}

#[test]
fn invalid_initial_selection_is_not_sanitized_into_an_available_choice() {
    for invalid in [
        selection(explicit("missing")),
        ModelSelection {
            model: "Alpha-model".into(),
            ..selection(ProviderSelection::Auto)
        },
        ModelSelection {
            vendor: "test-b".into(),
            ..selection(ProviderSelection::Auto)
        },
        ModelSelection {
            model: String::new(),
            ..selection(ProviderSelection::Auto)
        },
    ] {
        let mut state = startup(routing_config(), Some(invalid.clone()));
        assert!(state.submit().is_err());
        assert_eq!(state.selected_vendor.as_ref(), Some(&invalid.vendor));
        assert_eq!(state.model.as_ref(), Some(&invalid.model));
        assert_eq!(state.provider, invalid.provider);
    }
}

#[test]
fn discovery_full_model_id_is_exact_without_case_prefix_suffix_or_space_aliases() {
    for discovered in [
        "Alpha-model",
        "prefix/alpha-model",
        "alpha-model-suffix",
        " alpha-model",
        "alpha-model ",
    ] {
        let mut file = routing_config();
        for provider in file.providers.values_mut() {
            provider.discovery.models = vec![discovered.into()];
        }
        let mut state = startup(file, None);
        assert!(state
            .submit()
            .unwrap_err()
            .contains("精确发现模型 alpha-model"));
        assert!(state
            .catalog_models
            .iter()
            .find(|m| m.id == "alpha-model")
            .unwrap()
            .providers
            .is_empty());
    }
}

#[test]
fn empty_config_keeps_builtin_catalog_and_never_enters_a_legacy_setup() {
    let mut state = startup(FileConfig::default(), None);
    assert!(!state.catalog_models.is_empty());
    assert!(state.catalog_models.iter().all(|m| m.providers.is_empty()));
    assert!(state.submit().unwrap_err().contains("请选择 Vendor"));
    state.selected_row = 2;
    state.change_current(true);
    assert!(state.selected_vendor.is_some());
    assert!(state.submit().is_err());
    let lines = state.lines().join("\n");
    assert!(lines.contains("Provider 选择: Auto"));
    assert!(lines.contains("不能开始"));
    assert!(!lines.contains("Profile"));
}

#[test]
fn default_model_priority_comes_from_session_and_unavailability_does_not_change_model() {
    let mut file = routing_config();
    file.defaults.model = None;
    file.models.get_mut("second-model").unwrap().priority = 500;
    let mut state = startup(file.clone(), None);
    assert_eq!(state.submit().unwrap().selection.model, "second-model");
    for provider in file.providers.values_mut() {
        provider.discovery.models.retain(|id| id != "second-model");
    }
    let mut blocked = startup(file, None);
    assert!(blocked.submit().unwrap_err().contains("second-model"));
    assert!(blocked.route.is_none());
}

#[test]
fn vendor_model_provider_navigation_and_all_option_rows_are_reachable() {
    let mut state = startup(routing_config(), None);
    state.turn_options.policy_profile = PolicyProfile::Strict;
    state.selected_row = 0;
    assert!(!state.change_current(true));
    assert_eq!(
        state.turn_options.capability_profile,
        CapabilityProfile::Research
    );
    state.selected_row = 1;
    assert!(!state.change_current(true));
    assert_eq!(state.turn_options.policy_profile, PolicyProfile::Medium);
    state.selected_row = 4;
    state.change_current(true);
    assert_eq!(state.provider, explicit("alpha"));
    state.selected_row = 3;
    state.change_current(true);
    assert_eq!(state.model.as_deref(), Some("second-model"));
    assert_eq!(state.provider, ProviderSelection::Auto);
    state.selected_row = 2;
    state.change_current(true);
    assert_eq!(state.selected_vendor.as_deref(), Some("test-b"));
    assert_eq!(state.model.as_deref(), Some("beta-model"));
    for row in 5..state.row_count() - 1 {
        state.selected_row = row;
        assert!(!state.change_current(true));
    }
    assert!(
        state.turn_options.network
            && state.turn_options.browser
            && !state.turn_options.memory_recall
            && state.turn_options.trace
    );
    assert_eq!(state.turn_options.mcp_servers, vec!["docs", "code"]);
    assert_eq!(state.row_count(), 12);
    state.move_selection(i16::MAX);
    assert_eq!(state.selected_row, 11);
    assert!(state.change_current(true));
    assert!(state.lines().iter().any(|line| line == "> 开始"));
    state.move_selection(i16::MIN);
    assert_eq!(state.selected_row, 0);
}

#[test]
fn provider_named_auto_remains_distinct_from_auto_mode_in_both_directions() {
    let mut file = routing_config();
    let mut provider = file.providers["alpha"].clone();
    file.providers.clear();
    provider.discovery.config_fingerprint =
        Some(provider_connection_fingerprint("auto", &provider));
    file.providers.insert("auto".into(), provider);
    let mut state = startup(file, None);
    state.selected_row = 4;
    for forward in [true, false] {
        state.change_current(forward);
        assert_eq!(state.submit().unwrap().selection.provider, explicit("auto"));
        assert!(state.lines().join("\n").contains("显式 auto"));
        state.change_current(forward);
        assert_eq!(
            state.submit().unwrap().selection.provider,
            ProviderSelection::Auto
        );
    }
}

#[test]
fn startup_filters_mcp_allowlist_and_draft_cancel_has_no_config_effects() {
    let file = routing_config();
    let before = serde_json::to_value(&file).unwrap();
    let mut state = StartupState::new(
        file.clone(),
        vec!["docs".into()],
        None,
        TurnOptions {
            mcp_servers: vec!["removed".into(), "docs".into()],
            ..TurnOptions::default()
        },
    )
    .unwrap();
    assert_eq!(state.turn_options.mcp_servers, vec!["docs"]);
    state.turn_options.mcp_servers.push("removed-again".into());
    assert_eq!(
        state.submit().unwrap().turn_options.mcp_servers,
        vec!["docs"]
    );
    state.selected_row = 4;
    state.change_current(true);
    drop(state);
    assert_eq!(serde_json::to_value(&file).unwrap(), before);
}

#[test]
fn invalid_config_refresh_cannot_leave_a_submit_capable_old_snapshot() {
    let mut file = routing_config();
    let mut state = startup(file.clone(), None);
    file.defaults.model = Some("missing-model".into());
    assert!(state.refresh_config(file).is_err());
    assert!(state.route.is_none());
    assert!(state.catalog_models.is_empty());
    assert!(state.submit().is_err());
}

#[test]
fn routing_config_draft_preserves_explicit_and_auto_defaults_without_persisting_route() {
    let mut file = routing_config();
    file.defaults.provider_id = Some("low".into());
    let before = serde_json::to_value(&file).unwrap();
    let mut draft = ModelRoutingConfigState::new("revision".into(), file.clone());
    assert_eq!(draft.selected_provider_id.as_deref(), Some("low"));
    assert_eq!(draft.preview.as_ref().unwrap().provider_id, "low");
    draft.set_default_route().unwrap();
    assert_eq!(draft.draft.defaults.provider_id.as_deref(), Some("low"));
    draft.selected_provider_id = None;
    draft.set_default_route().unwrap();
    assert_eq!(draft.preview.as_ref().unwrap().provider_id, "alpha");
    assert_eq!(draft.draft.defaults.provider_id, None);
    assert!(draft.route_lines().join("\n").contains("选择: Auto"));
    draft.selected_provider_id = Some("missing".into());
    assert!(draft
        .set_default_route()
        .unwrap_err()
        .contains("不会 fallback"));
    assert!(draft.preview.is_none());
    assert_eq!(draft.draft.defaults.provider_id, None);
    drop(draft);
    assert_eq!(serde_json::to_value(&file).unwrap(), before);
}

#[test]
fn reducer_keeps_user_selection_separate_from_route_and_rejects_implicit_route_guessing() {
    let file = routing_config();
    let chosen = selection(ProviderSelection::Auto);
    let route = resolve_available_route(&file, None, None, None).unwrap();
    let mut state = TuiState::default();
    state.reduce(AgentEvent::ModelSelected {
        selection: chosen.clone(),
        route: route.clone(),
    });
    assert_eq!(state.selection.as_ref(), Some(&chosen));
    assert_eq!(state.route.as_ref(), Some(&route));
    let text = sidebar_text(&state);
    assert!(text.contains("用户选择 · test-a / alpha-model · Auto"));
    assert!(text.contains("已确认路由 · test-a / alpha-model → alpha"));
    state.reduce(AgentEvent::Started {
        provider: route.protocol,
        model: route.model,
        selection: None,
        route: None,
        turn_id: "turn-1".into(),
        capability_profile: CapabilityProfile::General,
        policy_profile: PolicyProfile::Strict,
    });
    assert_eq!(state.selection.as_ref(), Some(&chosen));
    assert!(state.route.is_none());
    assert_eq!(state.turn_phase, TurnPhase::Running);
    assert!(sidebar_text(&state).contains("未确认（等待 Session 路由）"));
    let route = resolve_available_route(&file, None, None, Some("low")).unwrap();
    state.reduce(AgentEvent::ModelSelected {
        selection: selection(explicit("low")),
        route: route.clone(),
    });
    assert_eq!(state.selection.as_ref().unwrap().provider, explicit("low"));
    assert_eq!(state.route.as_ref(), Some(&route));
    state.reduce(AgentEvent::Error {
        message: "显式 Provider 被禁用".into(),
    });
    assert_eq!(state.selection.as_ref().unwrap().provider, explicit("low"));
}

#[test]
fn started_route_replaces_auto_preview_with_authoritative_provider_and_preserves_explicit() {
    let mut file = routing_config();
    let chosen = selection(ProviderSelection::Auto);
    let initial = resolve_available_route(&file, None, None, None).unwrap();
    let mut state = TuiState::default();
    state.reduce(AgentEvent::ModelSelected {
        selection: chosen.clone(),
        route: initial,
    });
    file.providers.get_mut("zeta").unwrap().priority = 500;
    let next = resolve_available_route(&file, None, None, None).unwrap();
    state.reduce(AgentEvent::Started {
        provider: next.protocol,
        model: next.model.clone(),
        selection: Some(chosen.clone()),
        route: Some(next.clone()),
        turn_id: "auto-next".into(),
        capability_profile: CapabilityProfile::General,
        policy_profile: PolicyProfile::Strict,
    });
    assert_eq!(state.selection.as_ref(), Some(&chosen));
    assert_eq!(state.route.as_ref(), Some(&next));
    let text = sidebar_text(&state);
    assert!(text.contains("用户选择 · test-a / alpha-model · Auto"));
    assert!(text.contains("已确认路由 · test-a / alpha-model → zeta"));
    assert!(!text.contains("→ alpha"));
    assert!(!text.contains("等待 Session 路由"));

    // Started 自身足以更新选择，不依赖前序 ModelSelected。
    let chosen = selection(explicit("low"));
    let route = resolve_available_route(&file, None, None, Some("low")).unwrap();
    state.reduce(AgentEvent::Started {
        provider: route.protocol,
        model: route.model.clone(),
        selection: Some(chosen.clone()),
        route: Some(route.clone()),
        turn_id: "explicit-next".into(),
        capability_profile: CapabilityProfile::General,
        policy_profile: PolicyProfile::Strict,
    });
    assert_eq!(state.selection.as_ref(), Some(&chosen));
    assert_eq!(state.route.as_ref(), Some(&route));
    let text = sidebar_text(&state);
    assert!(text.contains("显式 low"));
    assert!(text.contains("→ low"));
}

#[test]
fn startup_render_distinguishes_selection_and_preview_and_keeps_policy_controls() {
    let state = AppState {
        startup: Some(startup(routing_config(), None)),
        ..AppState::default()
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();
    // TestBackend 的宽字符后含占位空格；比较文字时忽略这些单元格。
    let output = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .replace(' ', "");
    for expected in [
        "能力模式",
        "安全策略",
        "Vendor: test-a",
        "Model: alpha-model",
        "Provider 选择: Auto",
        "路由预览:",
        "alpha",
        "MCP code",
        "开始",
    ] {
        assert!(
            output.contains(&expected.replace(' ', "")),
            "missing {expected}"
        );
    }
    assert!(!output.contains("Profile"));
}

#[test]
fn startup_scroll_keeps_every_selected_control_visible_on_minimum_terminal() {
    let mut startup = startup(routing_config(), None);
    startup
        .enabled_mcp_servers
        .extend((0..15).map(|i| format!("server-{i}")));
    let mut state = AppState {
        startup: Some(startup),
        ..AppState::default()
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
    for row in 0..state.startup.as_ref().unwrap().row_count() {
        state.startup.as_mut().unwrap().selected_row = row;
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(output.contains("> "), "selected row {row} is hidden");
    }
}
