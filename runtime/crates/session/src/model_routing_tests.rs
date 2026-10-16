use super::*;

fn fixture() -> FileConfig {
    let mut file = FileConfig::default();
    for id in ["a", "b"] {
        let mut provider = ProviderConfig {
            base_url: "https://gateway.example/v1".into(),
            auth: AuthConfig::default(),
            enabled: true,
            priority: 100,
            discovery: ModelDiscoveryCache::default(),
        };
        provider.discovery = ModelDiscoveryCache {
            config_fingerprint: Some(provider_connection_fingerprint(id, &provider)),
            supported: Some(true),
            models: vec!["gpt-5".into(), "gpt-5.2".into()],
            last_checked_at: Some(1),
            last_error: None,
        };
        file.providers.insert(id.into(), provider);
    }

    file.defaults.vendor = Some("openai".into());
    file.defaults.model = Some("gpt-5".into());
    file.defaults.provider_id = Some("a".into());
    file
}

#[test]
fn supported_legacy_flat_config_upgrades_to_current_schema_without_network_access() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
version = 3
provider = "openai_chat_completions"
model = "gpt-5"
base_url = "https://gateway.example/v1"
credential_name = "gateway-prod"
capability_profile = "general"
policy_profile = "workspace"
"#,
    )
    .unwrap();

    let upgraded = upgrade_file_config(&path).unwrap();
    assert_eq!(upgraded.version, CONFIG_VERSION);
    assert_eq!(upgraded.defaults.vendor.as_deref(), Some("openai"));
    assert_eq!(upgraded.defaults.model.as_deref(), Some("gpt-5"));
    assert_eq!(
        upgraded.defaults.provider_id.as_deref(),
        Some("openai_chat_completions")
    );
    assert_eq!(
        upgraded.providers["openai_chat_completions"]
            .auth
            .credential
            .as_deref(),
        Some("gateway-prod")
    );
    // 旧扁平配置的协议选择保留为本地模型协议顺序。
    assert_eq!(
        upgraded.models["gpt-5"].supported_protocols,
        vec![
            ProviderKind::OpenAiChatCompletions,
            ProviderKind::OpenAiResponses
        ]
    );
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(persisted.contains("version = 6"));
    assert!(!persisted.contains("protocol ="));
    assert!(!persisted.contains("credential_name"));
    let once = persisted.clone();
    upgrade_file_config(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), once);
}

#[test]
fn nested_schema3_config_upgrades_without_losing_current_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut legacy = toml::to_string_pretty(&fixture()).unwrap();
    legacy = legacy.replacen("version = 6", "version = 3", 1);
    legacy.push_str("\n[supervision]\nenabled = false\n");
    std::fs::write(&path, &legacy).unwrap();

    let upgraded = upgrade_file_config(&path).unwrap();
    assert_eq!(upgraded.version, CONFIG_VERSION);
    assert_eq!(upgraded.defaults.vendor.as_deref(), Some("openai"));
    assert_eq!(upgraded.defaults.model.as_deref(), Some("gpt-5"));
    assert_eq!(upgraded.providers.len(), 2);
    assert_eq!(
        upgraded.providers["a"].discovery.models,
        vec!["gpt-5", "gpt-5.2"]
    );
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(persisted.contains("version = 6"));
    assert!(!persisted.contains("[supervision]"));
    let once = persisted.clone();
    upgrade_file_config(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), once);
}

#[test]
fn nested_schema3_config_is_compatible_with_read_only_queries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut source = toml::to_string_pretty(&fixture()).unwrap();
    source = source.replacen("version = 6", "version = 3", 1);
    source.push_str("\n[supervision]\nenabled = false\n");
    std::fs::write(&path, &source).unwrap();

    let draft = read_model_routing_draft(&path).unwrap();
    assert_eq!(draft.config.version, CONFIG_VERSION);
    assert_eq!(draft.config.providers.len(), 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
}

#[test]
fn schema5_provider_protocols_are_removed_and_discovery_stays_valid() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let base_url = "https://gateway.example/v1";
    let auth = AuthConfig {
        credential: Some("gateway-prod".into()),
    };
    let mut source = String::from("version = 5\n");
    for (id, protocol) in [
        ("gw-responses", ProviderKind::OpenAiResponses),
        ("gw-messages", ProviderKind::ClaudeMessages),
    ] {
        let old = rivo_storage::sha256_text(
            &serde_json::json!([id, protocol, base_url, auth]).to_string(),
        );
        source.push_str(&format!(
            r#"
[providers.{id}]
protocol = "{protocol}"
base_url = "{base_url}"
[providers.{id}.auth]
credential = "gateway-prod"
[providers.{id}.discovery]
config_fingerprint = "{old}"
supported = true
models = ["gpt-6-sol", "claude-sonnet-5-5"]
last_checked_at = 1
"#
        ));
    }
    source.push_str(
        r#"
[providers.stale]
protocol = "openai_responses"
base_url = "https://stale.example/v1"
[providers.stale.discovery]
config_fingerprint = "outdated"
supported = true
models = ["gpt-6-sol"]
last_checked_at = 1
"#,
    );
    std::fs::write(&path, &source).unwrap();

    // 只读查询不写盘。
    assert_eq!(
        read_model_routing_draft(&path).unwrap().config.version,
        CONFIG_VERSION
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);

    let upgraded = upgrade_file_config(&path).unwrap();
    for id in ["gw-responses", "gw-messages"] {
        assert!(provider_discovery_matches_current_config(
            id,
            &upgraded.providers[id]
        ));
    }
    assert!(!provider_discovery_matches_current_config(
        "stale",
        &upgraded.providers["stale"]
    ));
    // 两个 Provider 的协议来自模型而不是旧 Provider 字段。
    for (model, vendor, protocol) in [
        ("gpt-6-sol", "openai", ProviderKind::OpenAiResponses),
        (
            "claude-sonnet-5-5",
            "anthropic",
            ProviderKind::ClaudeMessages,
        ),
    ] {
        for provider in ["gw-responses", "gw-messages"] {
            let route =
                resolve_available_route(&upgraded, Some(vendor), Some(model), Some(provider))
                    .unwrap();
            assert_eq!(route.protocol, protocol);
        }
    }
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(persisted.contains("version = 6"));
    assert!(!persisted.contains("protocol ="));
    upgrade_file_config(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), persisted);
}

#[test]
fn unsupported_legacy_config_semantics_and_future_versions_are_not_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let unsupported = r#"
provider = "openai_chat_completions"
model = "gpt-5"
base_url = "https://gateway.example/v1"
api_key_env = "RIVO_LEGACY_KEY"
"#;
    std::fs::write(&path, unsupported).unwrap();
    assert!(upgrade_file_config(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), unsupported);

    let future = "version = 7\n";
    std::fs::write(&path, future).unwrap();
    assert!(upgrade_file_config(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), future);

    let unknown_model = r#"
provider = "openai_chat_completions"
model = "unregistered-model"
base_url = "https://gateway.example/v1"
"#;
    std::fs::write(&path, unknown_model).unwrap();
    assert!(upgrade_file_config(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), unknown_model);

    let active_supervision = r#"
version = 3
[supervision]
enabled = true
"#;
    std::fs::write(&path, active_supervision).unwrap();
    assert!(upgrade_file_config(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), active_supervision);
}

fn write_fixture(path: &Path, file: &FileConfig) {
    std::fs::write(path, toml::to_string_pretty(file).unwrap()).unwrap();
}

#[test]
fn runtime_environment_key_has_priority_and_blank_value_falls_back_to_credential() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let credential_name = "runtime-env-fallback";
    let environment_name = "RIVO_RUNTIME_ENV_FALLBACK_TEST_KEY";
    set_credential(&path, credential_name, "credential-value").unwrap();
    let auth = AuthConfig {
        credential: Some(credential_name.into()),
    };

    std::env::set_var(environment_name, "runtime-value");
    assert_eq!(
        provider_api_key(&path, &auth, Some(environment_name)).unwrap(),
        "runtime-value"
    );
    std::env::set_var(environment_name, "   ");
    assert_eq!(
        provider_api_key(&path, &auth, Some(environment_name)).unwrap(),
        "credential-value"
    );
    std::env::remove_var(environment_name);
}

#[test]
fn provider_without_auth_reference_uses_anonymous_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    assert_eq!(
        provider_api_key(&path, &AuthConfig::default(), Some("RIVO_MISSING_TEST_KEY")).unwrap(),
        ""
    );
}

#[test]
fn selection_environment_overrides_defaults_and_cli_overrides_environment() {
    let environment = BTreeMap::from([
        ("RIVO_VENDOR".into(), "openai".into()),
        ("RIVO_MODEL".into(), "gpt-5.2".into()),
        ("RIVO_PROVIDER_ID".into(), "b".into()),
        ("RIVO_PROFILE".into(), "research".into()),
        ("RIVO_POLICY".into(), "strict".into()),
    ]);
    let resolved =
        ResolvedConfig::resolve(fixture(), &environment, ConfigOverrides::default()).unwrap();
    let route = resolved.route.unwrap();
    assert_eq!(
        (
            route.vendor.as_str(),
            route.model.as_str(),
            route.provider_id.as_str()
        ),
        ("openai", "gpt-5.2", "b")
    );
    assert!(resolved.selection_explicit);
    assert_eq!(resolved.capability_profile, CapabilityProfile::Research);
    assert_eq!(resolved.policy_profile, PolicyProfile::Strict);

    let resolved = ResolvedConfig::resolve(
        fixture(),
        &environment,
        ConfigOverrides {
            vendor: Some("openai".into()),
            model: Some("gpt-5".into()),
            provider_id: Some("a".into()),
            ..ConfigOverrides::default()
        },
    )
    .unwrap();
    let route = resolved.route.unwrap();
    assert_eq!(
        (
            route.vendor.as_str(),
            route.model.as_str(),
            route.provider_id.as_str()
        ),
        ("openai", "gpt-5", "a")
    );
}

#[test]
fn stale_default_model_is_reported_clearly_and_does_not_block_an_explicit_selection() {
    let mut file = fixture();
    file.defaults.model = Some("gpt-5.6-luna".into());
    let strict = validate_model_routing_config(&file)
        .unwrap_err()
        .to_string();
    assert!(
        strict.contains("defaults.model gpt-5.6-luna 不在模型目录中"),
        "{strict}"
    );
    assert!(strict.contains("gpt-5"), "{strict}");
    let error = ResolvedConfig::resolve(file.clone(), &BTreeMap::new(), ConfigOverrides::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("defaults.model gpt-5.6-luna 不在模型目录中"),
        "{error}"
    );
    let resolved = ResolvedConfig::resolve(
        file,
        &BTreeMap::new(),
        ConfigOverrides {
            vendor: Some("openai".into()),
            model: Some("gpt-5.2".into()),
            ..ConfigOverrides::default()
        },
    )
    .unwrap();
    assert_eq!(resolved.route.unwrap().model, "gpt-5.2");
}

#[test]
fn environment_can_select_without_defaults_and_auto_overrides_environment_provider() {
    let mut file = fixture();
    file.defaults.vendor = None;
    file.defaults.model = None;
    file.defaults.provider_id = None;
    let environment = BTreeMap::from([
        ("RIVO_VENDOR".into(), "openai".into()),
        ("RIVO_MODEL".into(), "gpt-5".into()),
        ("RIVO_PROVIDER_ID".into(), "b".into()),
    ]);
    let resolved = ResolvedConfig::resolve(file, &environment, ConfigOverrides::default()).unwrap();
    assert_eq!(resolved.route.unwrap().provider_id, "b");
    assert!(resolved.selection_explicit);

    let resolved = ResolvedConfig::resolve(
        fixture(),
        &environment,
        ConfigOverrides {
            auto_provider: true,
            ..ConfigOverrides::default()
        },
    )
    .unwrap();
    assert_eq!(resolved.route.unwrap().provider_id, "a");
    assert_eq!(
        resolved.selection.unwrap().provider,
        ProviderSelection::Auto
    );
    assert!(ResolvedConfig::resolve(
        fixture(),
        &environment,
        ConfigOverrides {
            auto_provider: true,
            provider_id: Some("a".into()),
            ..ConfigOverrides::default()
        }
    )
    .is_err());
}

#[test]
fn invalid_selection_environment_never_falls_back() {
    for key in ["RIVO_VENDOR", "RIVO_MODEL", "RIVO_PROVIDER_ID"] {
        for value in ["", "unknown-id"] {
            let environment = BTreeMap::from([(key.into(), value.into())]);
            assert!(
                ResolvedConfig::resolve(fixture(), &environment, ConfigOverrides::default())
                    .is_err(),
                "{key}"
            );
        }
    }
    let mut file = fixture();
    file.providers.get_mut("b").unwrap().enabled = false;
    let environment = BTreeMap::from([("RIVO_PROVIDER_ID".into(), "b".into())]);
    assert!(ResolvedConfig::resolve(file, &environment, ConfigOverrides::default()).is_err());
}

#[test]
fn removed_connection_environment_is_rejected_without_echoing_values() {
    for key in [
        "RIVO_PROVIDER_PROFILE",
        "RIVO_PROVIDER",
        "RIVO_BASE_URL",
        "RIVO_API_KEY_ENV",
        "RIVO_CREDENTIAL_NAME",
    ] {
        for value in ["", "sensitive-fixture-value"] {
            let environment = BTreeMap::from([(key.into(), value.into())]);
            let error = ResolvedConfig::resolve(
                fixture(),
                &environment,
                ConfigOverrides {
                    auto_provider: true,
                    ..ConfigOverrides::default()
                },
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(key));
            assert!(!error.contains("sensitive-fixture-value"));
        }
    }
}

#[test]
fn builtin_catalog_is_used_by_queries_and_routing() {
    let file = fixture();
    let catalog = read_default_catalog().unwrap();
    assert_eq!(catalog.schema_version, CONFIG_VERSION);
    assert_eq!(catalog.vendors.len(), 5);
    assert_eq!(catalog.models.len(), 21);
    for model in [
        "claude-haiku-4-5",
        "claude-opus-5-5",
        "claude-opus-4-6",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5-5",
        "deepseek-flash",
        "deepseek-v4-flash",
        "deepseek-v4-pro",
        "glm-5.3",
        "glm-5.3-flash",
        "grok-4",
        "gpt-5",
        "gpt-5.1",
        "gpt-5.2",
        "gpt-5.2-chat-latest",
        "gpt-5.3-codex",
        "gpt-6-astra",
        "gpt-6.1-sol",
        "gpt-6-sol",
        "gpt-6-luna",
    ] {
        assert!(catalog.models.contains_key(model), "missing model {model}");
    }
    assert_eq!(
        catalog.models["claude-opus-5-5"].supported_protocols,
        vec![ProviderKind::ClaudeMessages]
    );
    assert_eq!(
        catalog.models["claude-sonnet-5-5"].supported_protocols,
        vec![ProviderKind::ClaudeMessages]
    );
    assert_eq!(
        catalog.models["gpt-6.1-sol"].supported_protocols,
        vec![
            ProviderKind::OpenAiResponses,
            ProviderKind::OpenAiChatCompletions
        ]
    );
    let source = include_str!("defaults.toml");
    for model in catalog.models.keys() {
        let quoted_marker = format!("[models.\"{model}\"]");
        let bare_marker = format!("[models.{model}]");
        let position = source
            .find(&quoted_marker)
            .or_else(|| source.find(&bare_marker))
            .unwrap_or_else(|| panic!("missing source marker for {model}"));
        let evidence = &source[position.saturating_sub(512)..position];
        let verified_at = evidence
            .rsplit("# verified_at = ")
            .next()
            .filter(|_| evidence.contains("# verified_at = "))
            .and_then(|tail| tail.get(..10))
            .unwrap_or_else(|| panic!("missing verified_at for {model}"));
        assert!(
            evidence.contains("# official_model = https://")
                && is_iso_date(verified_at)
                && verified_at >= "2026-09-23",
            "missing official evidence comment for {model}"
        );
    }
    validate_default_catalog(&catalog).unwrap();
    // 墓碑必须覆盖历史上移除过的 ID，防止旧模型被无意重新加入。
    for removed in [
        "claude-opus-4-1",
        "deepseek-chat",
        "deepseek-reasoner",
        "deepseek-coder",
        "glm-4.5-air",
        "grok-3",
        "gpt-4.1",
        "gpt-image-2",
        "claude-fable-5",
        "claude-opus-4-5",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-5",
        "gpt-5.3-chat-latest",
        "gpt-5.3-codex-spark",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "glm-4.5",
        "glm-4.7",
        "glm-5",
        "glm-5.1",
    ] {
        assert!(
            catalog.retired_models.contains_key(removed),
            "retired model {removed} needs a tombstone"
        );
    }
    for removed in catalog.retired_models.keys() {
        assert!(
            !catalog.models.contains_key(removed),
            "stale model {removed}"
        );
    }
    assert!(!catalog.vendors.contains_key("google"));
    assert_eq!(merged_vendor_catalog(&file).unwrap(), catalog.vendors);
    assert_eq!(merged_model_catalog(&file).unwrap(), catalog.models);
    assert_eq!(
        model_catalog_sources(&file).unwrap()["gpt-5"],
        CatalogSource::Builtin
    );
    assert_eq!(
        vendor_catalog_sources(&file).unwrap()["openai"],
        CatalogSource::Builtin
    );
    assert_eq!(provider_models(&file, "a").unwrap().len(), 2);
    let routes = model_providers(&file, "openai", "gpt-5").unwrap();
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].provider_id, "a");
    assert_eq!(
        routes[0],
        resolve_available_route(&file, Some("openai"), Some("gpt-5"), None).unwrap()
    );
}

fn verified_provider(id: &str, models: &[&str], priority: u16) -> ProviderConfig {
    let mut provider = ProviderConfig {
        base_url: format!("https://{id}.example/v1"),
        auth: AuthConfig::default(),
        enabled: true,
        priority,
        discovery: ModelDiscoveryCache::default(),
    };
    provider.discovery = ModelDiscoveryCache {
        config_fingerprint: Some(provider_connection_fingerprint(id, &provider)),
        supported: Some(true),
        models: models.iter().map(|model| (*model).into()).collect(),
        last_checked_at: Some(1),
        last_error: None,
    };
    provider
}

#[test]
fn one_provider_serves_each_model_over_its_preferred_protocol() {
    let mut file = FileConfig::default();
    file.providers.insert(
        "gateway".into(),
        verified_provider(
            "gateway",
            &[
                "gpt-6-sol",
                "gpt-6.1-sol",
                "claude-sonnet-5-5",
                "deepseek-flash",
            ],
            1,
        ),
    );
    for (vendor, model, protocol) in [
        ("openai", "gpt-6-sol", ProviderKind::OpenAiResponses),
        ("openai", "gpt-6.1-sol", ProviderKind::OpenAiResponses),
        (
            "anthropic",
            "claude-sonnet-5-5",
            ProviderKind::ClaudeMessages,
        ),
        (
            "deepseek",
            "deepseek-flash",
            ProviderKind::OpenAiChatCompletions,
        ),
    ] {
        let route = resolve_available_route(&file, Some(vendor), Some(model), None).unwrap();
        assert_eq!(route.provider_id, "gateway");
        assert_eq!(route.protocol, protocol, "{model}");
        assert_eq!(model_providers(&file, vendor, model).unwrap().len(), 1);
    }
    assert_eq!(provider_models(&file, "gateway").unwrap().len(), 4);
}

#[test]
fn local_override_reorders_adds_or_disables_builtin_protocols() {
    let builtin = read_default_catalog().unwrap().models["glm-5.3"].clone();
    let mut file = FileConfig::default();
    file.providers.insert(
        "glm-coding-plan".into(),
        verified_provider("glm-coding-plan", &["glm-5.3"], 1),
    );
    let route = |file: &FileConfig| {
        resolve_available_route(file, Some("zhipu"), Some("glm-5.3"), None).map(|r| r.protocol)
    };
    assert_eq!(
        route(&file).unwrap(),
        builtin.supported_protocols[0],
        "builtin order applies without an override"
    );

    // 编码套餐只提供 Chat Completions：用户在模型上禁用其他协议。
    let mut chat_only = builtin.clone();
    chat_only.supported_protocols = vec![ProviderKind::OpenAiChatCompletions];
    file.models.insert("glm-5.3".into(), chat_only);
    assert_eq!(
        merged_model_catalog(&file).unwrap()["glm-5.3"].supported_protocols,
        vec![ProviderKind::OpenAiChatCompletions]
    );
    assert_eq!(
        model_catalog_sources(&file).unwrap()["glm-5.3"],
        CatalogSource::BuiltinOverride
    );
    assert_eq!(route(&file).unwrap(), ProviderKind::OpenAiChatCompletions);

    let mut reordered = builtin.clone();
    reordered.supported_protocols = vec![
        ProviderKind::ClaudeMessages,
        ProviderKind::OpenAiChatCompletions,
    ];
    file.models.insert("glm-5.3".into(), reordered);
    assert_eq!(route(&file).unwrap(), ProviderKind::ClaudeMessages);

    let mut added = read_default_catalog().unwrap().models["glm-5.3-flash"].clone();
    added
        .supported_protocols
        .insert(0, ProviderKind::OpenAiResponses);
    file.models.insert("glm-5.3-flash".into(), added);
    assert!(validate_model_routing_structure(&file).is_ok());

    for invalid_protocols in [
        Vec::new(),
        vec![
            ProviderKind::OpenAiChatCompletions,
            ProviderKind::OpenAiChatCompletions,
        ],
    ] {
        let mut invalid = builtin.clone();
        invalid.supported_protocols = invalid_protocols;
        file.models.insert("glm-5.3".into(), invalid);
        assert!(validate_model_routing_structure(&file).is_err());
    }

    let mut renamed = builtin;
    renamed.vendor = "openai".into();
    file.models.insert("glm-5.3".into(), renamed);
    assert!(merged_model_catalog(&file).is_err());
}

#[test]
fn unconfirmed_deepseek_ids_are_not_routable_and_old_local_protocols_are_not_guessed() {
    let catalog = read_default_catalog().unwrap();
    assert!(!catalog.models.contains_key("deepseek-chat"));
    assert!(!catalog.models.contains_key("deepseek-reasoner"));

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let content = r#"version = 4

[vendors.local]
display_name = "Local"

[models.local-model]
vendor = "local"
display_name = "Local"
"#;
    std::fs::write(&path, content).unwrap();
    assert!(upgrade_file_config(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
}

#[test]
fn builtin_identity_cannot_be_redefined_but_priority_can() {
    let mut file = fixture();
    let mut entry = merged_model_catalog(&file).unwrap()["gpt-5"].clone();
    entry.priority = 1000;
    file.models.insert("gpt-5".into(), entry);
    validate_model_routing_config(&file).unwrap();
    assert_eq!(
        model_catalog_sources(&file).unwrap()["gpt-5"],
        CatalogSource::BuiltinOverride
    );
    file.models.get_mut("gpt-5").unwrap().vendor = "unknown".into();
    assert!(validate_model_routing_config(&file).is_err());
    file.models.clear();
    file.vendors.insert(
        "openai".into(),
        VendorCatalogEntry {
            display_name: "Other".into(),
        },
    );
    assert!(validate_model_routing_config(&file).is_err());
}

#[test]
fn disabled_models_are_skipped_by_auto_and_rejected_when_selected() {
    let mut file = fixture();
    assert!(read_default_catalog().unwrap().models["gpt-5"].enabled);
    let mut disabled = merged_model_catalog(&file).unwrap()["gpt-5"].clone();
    disabled.enabled = false;
    file.models.insert("gpt-5".into(), disabled.clone());

    assert!(resolve_available_route(&file, None, None, None).is_err());
    let mut fallback = merged_model_catalog(&file).unwrap()["gpt-5.2"].clone();
    fallback.priority = 1000;
    file.models.insert("gpt-5.2".into(), fallback);
    file.defaults.model = None;
    assert_eq!(
        resolve_available_route(&file, None, None, None)
            .unwrap()
            .model,
        "gpt-5.2"
    );
    assert!(resolve_available_route(&file, Some("openai"), Some("gpt-5"), None).is_err());

    disabled.enabled = true;
    file.models.insert("gpt-5".into(), disabled);
    file.defaults.model = Some("gpt-5".into());
    assert_eq!(
        resolve_available_route(&file, None, None, None)
            .unwrap()
            .model,
        "gpt-5"
    );
}

#[test]
fn exact_ids_and_all_unavailable_states_reject_explicit_provider() {
    for model in [
        "GPT-4.1",
        "prefix/gpt-4.1",
        "gpt-4.1-suffix",
        " gpt-4.1",
        "gpt-4.1 ",
    ] {
        let mut file = fixture();
        file.providers.get_mut("a").unwrap().discovery.models = vec![model.into()];
        assert!(resolve_available_route(&file, None, None, Some("a")).is_err());
        assert_eq!(
            resolve_available_route(&file, None, None, None)
                .unwrap()
                .provider_id,
            "b"
        );
    }
    for state in 0..7 {
        let mut file = fixture();
        let p = file.providers.get_mut("a").unwrap();
        match state {
            0 => p.enabled = false,
            1 => p.discovery = ModelDiscoveryCache::default(),
            2 => p.discovery.supported = Some(false),
            3 => p.discovery.last_error = Some("failed".into()),
            4 => p.discovery.config_fingerprint = Some("stale".into()),
            5 => p.discovery.last_checked_at = None,
            _ => p.discovery.models.clear(),
        }
        assert!(resolve_available_route(&file, None, None, Some("a")).is_err());
        assert_eq!(
            resolve_available_route(&file, None, None, None)
                .unwrap()
                .provider_id,
            "b"
        );
    }
}

#[test]
fn read_validate_route_and_plan_do_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write_fixture(&path, &fixture());
    let before = std::fs::read(&path).unwrap();
    let draft = read_model_routing_draft(&path).unwrap();
    validate_model_routing_config(&draft.config).unwrap();
    resolve_available_route(&draft.config, None, None, None).unwrap();
    plan_model_routing_delete(&path, "provider", "a").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert_eq!(
        draft.revision,
        rivo_storage::sha256_text(std::str::from_utf8(&before).unwrap())
    );
}

#[test]
fn revision_is_bound_to_raw_bytes_not_normalized_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write_fixture(&path, &fixture());
    let draft = read_model_routing_draft(&path).unwrap();
    let changed = format!(
        "# concurrent editor\n{}",
        std::fs::read_to_string(&path).unwrap()
    );
    std::fs::write(&path, &changed).unwrap();
    assert!(commit_model_routing_draft(&path, &draft.revision, &draft.config).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
}

#[test]
fn nested_missing_path_keeps_plan_identity_and_lock_through_create_and_update() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing/nested/config/config.toml");
    let identity = identity_path(&path).unwrap();
    let draft = read_model_routing_draft(&path).unwrap();
    let mut config = fixture();
    for provider in config.providers.values_mut() {
        provider.discovery = ModelDiscoveryCache::default();
    }
    let plan = plan_model_routing_change(&path, &draft.revision, &config).unwrap();
    assert!(!path.parent().unwrap().exists());
    with_commit_lock(&path, |locked_path| {
        assert_eq!(locked_path, identity);
        assert_eq!(
            token(locked_path, &draft.revision, "change", &config)?,
            plan.confirmation_token
        );
        Ok(())
    })
    .unwrap();
    let saved = commit_model_routing_change(
        &path,
        &draft.revision,
        &config,
        Some(&plan.confirmation_token),
    )
    .unwrap();
    assert_eq!(identity_path(&path).unwrap(), identity);
    assert_eq!(
        token(&path, &draft.revision, "change", &config).unwrap(),
        plan.confirmation_token
    );

    let canonical = std::fs::canonicalize(&path).unwrap();
    with_commit_lock(&path, |locked_path| {
        assert_eq!(locked_path, identity_path(&canonical)?);
        let same_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(canonical.with_extension("toml.lock"))
            .unwrap();
        assert!(FileExt::try_lock_exclusive(&same_lock).is_err());
        Ok(())
    })
    .unwrap();

    let mut changed = saved.config;
    changed.providers.get_mut("a").unwrap().base_url = "https://other.example/v1".into();
    let plan = plan_model_routing_change(&path, &saved.revision, &changed).unwrap();
    let canonical_plan = plan_model_routing_change(&canonical, &saved.revision, &changed).unwrap();
    assert_eq!(plan.confirmation_token, canonical_plan.confirmation_token);
    let updated = commit_model_routing_change(
        &path,
        &saved.revision,
        &changed,
        Some(&plan.confirmation_token),
    )
    .unwrap();
    assert_eq!(
        updated.config.providers["a"].base_url,
        changed.providers["a"].base_url
    );
}

#[test]
fn change_tokens_cannot_be_reused_for_another_nested_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("first/nested/config.toml");
    let other = dir.path().join("second/nested/config.toml");
    let mut config = fixture();
    for provider in config.providers.values_mut() {
        provider.discovery = ModelDiscoveryCache::default();
    }
    let plan = plan_model_routing_change(&path, "missing", &config).unwrap();
    let other_plan = plan_model_routing_change(&other, "missing", &config).unwrap();
    assert_ne!(plan.confirmation_token, other_plan.confirmation_token);
    let saved =
        commit_model_routing_change(&path, "missing", &config, Some(&plan.confirmation_token))
            .unwrap();
    let other_saved = commit_model_routing_change(
        &other,
        "missing",
        &config,
        Some(&other_plan.confirmation_token),
    )
    .unwrap();
    assert_eq!(saved.revision, other_saved.revision);
    config.providers.get_mut("a").unwrap().base_url = "https://other.example/v1".into();
    let plan = plan_model_routing_change(&path, &saved.revision, &config).unwrap();
    let before = std::fs::read(&other).unwrap();
    assert!(commit_model_routing_change(
        &other,
        &saved.revision,
        &config,
        Some(&plan.confirmation_token),
    )
    .is_err());
    assert_eq!(std::fs::read(&other).unwrap(), before);
    commit_model_routing_change(
        &path,
        &saved.revision,
        &config,
        Some(&plan.confirmation_token),
    )
    .unwrap();
}

#[test]
fn provider_change_requires_confirmation_invalidates_discovery_and_is_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write_fixture(&path, &fixture());
    let draft = read_model_routing_draft(&path).unwrap();
    let mut changed = draft.config.clone();
    changed.providers.get_mut("a").unwrap().base_url = "https://other.example/v1".into();
    let plan = plan_model_routing_change(&path, &draft.revision, &changed).unwrap();
    assert_eq!(plan.provider_impacts.len(), 1);
    assert_eq!(plan.provider_impacts[0].affected_models.len(), 2);
    assert!(plan.provider_impacts[0].impact_complete);
    assert!(commit_model_routing_draft(&path, &draft.revision, &changed).is_err());
    assert!(commit_model_routing_change(&path, &draft.revision, &changed, Some("wrong")).is_err());
    let saved = commit_model_routing_change(
        &path,
        &draft.revision,
        &changed,
        Some(&plan.confirmation_token),
    )
    .unwrap();
    assert_eq!(
        saved.config.providers["a"].discovery,
        ModelDiscoveryCache::default()
    );
    assert_eq!(saved.config.providers["b"], draft.config.providers["b"]);
    assert_eq!(
        resolve_available_route(&saved.config, None, None, None)
            .unwrap()
            .provider_id,
        "b"
    );
}

#[test]
fn drafts_cannot_forge_discovery_or_delete_objects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write_fixture(&path, &fixture());
    let draft = read_model_routing_draft(&path).unwrap();
    let mut changed = draft.config.clone();
    changed
        .providers
        .get_mut("a")
        .unwrap()
        .discovery
        .models
        .push("forged".into());
    assert!(commit_model_routing_draft(&path, &draft.revision, &changed).is_err());
    changed = draft.config.clone();
    changed.providers.remove("a");
    assert!(commit_model_routing_draft(&path, &draft.revision, &changed).is_err());
    assert_eq!(
        read_model_routing_draft(&path).unwrap().revision,
        draft.revision
    );
}

#[test]
fn delete_plan_is_bound_to_path_revision_and_impact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let other = dir.path().join("other.toml");
    write_fixture(&path, &fixture());
    write_fixture(&other, &fixture());
    let plan = plan_model_routing_delete(&path, "provider", "a").unwrap();
    assert!(plan.clears_default_route);
    assert!(delete_model_routing_config(&other, &plan, &plan.confirmation_token).is_err());
    let mut tampered = plan.clone();
    tampered.affected_models.clear();
    assert!(delete_model_routing_config(&path, &tampered, &plan.confirmation_token).is_err());
    let saved = delete_model_routing_config(&path, &plan, &plan.confirmation_token).unwrap();
    assert!(saved.config.defaults.provider_id.is_none());
    assert_eq!(saved.config.defaults.model.as_deref(), Some("gpt-5"));
    assert!(saved.config.providers.contains_key("b"));
    assert!(delete_model_routing_config(&path, &plan, &plan.confirmation_token).is_err());
}

#[test]
fn missing_or_stale_discovery_reports_incomplete_impact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut file = fixture();
    file.providers
        .get_mut("a")
        .unwrap()
        .discovery
        .config_fingerprint = None;
    write_fixture(&path, &file);
    let plan = plan_model_routing_delete(&path, "provider", "a").unwrap();
    assert!(!plan.impact_complete);
    assert_eq!(plan.affected_models.len(), 2);
    file.providers.get_mut("a").unwrap().discovery = ModelDiscoveryCache::default();
    write_fixture(&path, &file);
    let plan = plan_model_routing_delete(&path, "provider", "a").unwrap();
    assert!(!plan.impact_complete);
    assert!(plan.affected_models.is_empty());
}

#[test]
fn vendor_delete_does_not_silently_cascade_models() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut file = fixture();
    file.vendors.insert(
        "local".into(),
        VendorCatalogEntry {
            display_name: "Local".into(),
        },
    );
    file.models.insert(
        "local-model".into(),
        ModelCatalogEntry {
            vendor: "local".into(),
            display_name: "Local".into(),
            supported_protocols: Vec::from([ProviderKind::OpenAiResponses]),
            supported_reasoning_efforts: BTreeSet::new(),
            enabled: true,
            priority: 0,
        },
    );
    write_fixture(&path, &file);
    let plan = plan_model_routing_delete(&path, "vendor", "local").unwrap();
    assert!(delete_model_routing_config(&path, &plan, &plan.confirmation_token).is_err());
    assert!(read_file_config(&path)
        .unwrap()
        .models
        .contains_key("local-model"));
    assert!(plan_model_routing_delete(&path, "model", "gpt-4.1").is_err());
}

#[test]
fn invalid_fields_and_atomic_write_failure_leave_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write_fixture(&path, &fixture());
    let before = std::fs::read(&path).unwrap();
    let draft = read_model_routing_draft(&path).unwrap();
    let mut invalid = draft.config.clone();
    invalid.providers.get_mut("a").unwrap().priority = 1001;
    assert!(commit_model_routing_draft(&path, &draft.revision, &invalid).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // 不可替换目录目标；同目录临时文件在错误时由 RAII 清理。
    let target = dir.path().join("directory");
    std::fs::create_dir(&target).unwrap();
    assert!(atomic_replace(&target, "missing", &draft.config).is_err());
    assert!(target.is_dir());
}

#[test]
fn old_connection_objects_and_selection_payloads_are_rejected() {
    for body in [
        "version = 4\n[providers.a]\nprotocol = 'openai_responses'\nendpoint = 'old'",
        "version = 4\n[providers.a]\nprotocol = 'openai_responses'\nbase_url = 'https://example.test'\nrequest_model_id = 'alias'",
        "version = 4\n[models.x]\nvendor = 'openai'\ndisplay_name = 'x'\nbinding_id = 'old'",
    ] {
        assert!(parse_file_config(body).is_err());
    }
    assert!(serde_json::from_value::<Intent>(
        serde_json::json!({"type":"select_provider_profile", "name":"a"})
    )
    .is_err());
    assert!(serde_json::from_value::<AgentEvent>(serde_json::json!({"type":"provider_profile_selected", "name":"a", "provider":"openai_responses", "model":"gpt-4.1"})).is_err());
    let selection = ModelSelection {
        vendor: "openai".into(),
        model: "gpt-4.1".into(),
        provider: ProviderSelection::Auto,
    };
    assert!(serde_json::from_value::<Intent>(
        serde_json::json!({"type":"select_model", "selection":selection, "persist":true})
    )
    .is_err());
}

#[test]
fn route_serialization_exposes_only_public_identity_and_priority() {
    let route = resolve_available_route(&fixture(), None, None, None).unwrap();
    let json = serde_json::to_value(route).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 6);
    for key in [
        "vendor",
        "model",
        "provider_id",
        "protocol",
        "base_url",
        "priority",
    ] {
        assert!(json.get(key).is_some());
    }
}

#[test]
fn production_turn_and_retry_snapshots_freeze_but_next_turn_and_restore_revalidate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut file = fixture();
    file.defaults.provider_id = None;
    write_fixture(&path, &file);
    let config = ResolvedConfig::resolve_at(
        file.clone(),
        &BTreeMap::new(),
        ConfigOverrides {
            workspace: Some(dir.path().into()),
            ..ConfigOverrides::default()
        },
        path.clone(),
    )
    .unwrap();
    let active = build_active_llm(config.clone()).unwrap();
    let session = Session {
        config,
        active_llm: Arc::new(Mutex::new(Some(active))),
        session_id: "frozen-test".into(),
        storage: None,
        workspace_lease: None,
        session_messenger: Mutex::new(None),
    };
    let first = session.prepare_next_turn().unwrap().unwrap();
    let old_route = first.config.route.clone().unwrap();
    assert_eq!(old_route.provider_id, "a");
    let mut changed = read_model_routing_draft(&path).unwrap();
    changed.config.providers.get_mut("b").unwrap().priority = 200;
    commit_model_routing_draft(&path, &changed.revision, &changed.config).unwrap();
    let retry = active_llm_snapshot(&session.active_llm).unwrap().unwrap();
    assert_eq!(retry.config.route.as_ref(), Some(&old_route));
    assert!(Arc::ptr_eq(&retry.client, &first.client));
    let second = session.prepare_next_turn().unwrap().unwrap();
    assert_eq!(second.config.route.as_ref().unwrap().provider_id, "b");
    assert_eq!(first.config.route.as_ref(), Some(&old_route));
    let historical = stored_model_route(&first.config).unwrap();
    let restored = first
        .config
        .with_model_selection(historical.selection)
        .unwrap();
    assert_eq!(restored.route.unwrap().provider_id, "b");
    let explicit = ModelSelection {
        provider: ProviderSelection::Explicit {
            provider_id: "a".into(),
        },
        ..first.config.selection.clone().unwrap()
    };
    let plan = plan_model_routing_delete(&path, "provider", "a").unwrap();
    delete_model_routing_config(&path, &plan, &plan.confirmation_token).unwrap();
    assert!(first.config.with_model_selection(explicit).is_err());
    assert_eq!(retry.config.route.unwrap(), old_route);
}

fn mock_models_server(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
    // 401/403 会触发一次 Anthropic 风格认证重试，因此同一响应需要服务两次。
    let connections = if status.starts_with("401") || status.starts_with("403") {
        2
    } else {
        1
    };
    mock_http_server_n(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()), connections)
}

fn mock_http_server(response: String) -> (String, std::thread::JoinHandle<String>) {
    mock_http_server_n(response, 1)
}

/// 依次服务 `connections` 个连接，返回第一个请求。
fn mock_http_server_n(
    response: String,
    connections: usize,
) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..connections {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            socket.write_all(response.as_bytes()).unwrap();
            requests.push(String::from_utf8(request).unwrap());
        }
        requests.swap_remove(0)
    });
    (address, handle)
}

#[tokio::test]
async fn explicit_refresh_uses_auth_reference_and_failure_revokes_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let (address, server) = mock_models_server(
        "200 OK",
        r#"{"data":[{"id":"gpt-5"},{"id":"unknown-catalog-model"}]}"#,
    );
    let mut file = fixture();
    let provider = file.providers.get_mut("a").unwrap();
    provider.base_url = address;
    provider.auth.credential = Some("mock-discovery-key".into());
    set_credential(&path, "mock-discovery-key", "test-only-value").unwrap();
    write_fixture(&path, &file);
    let draft = read_model_routing_draft(&path).unwrap();
    let refreshed = refresh_provider_models(&path, &draft.revision, "a")
        .await
        .unwrap();
    let request = server.join().unwrap();
    assert!(request.starts_with("GET /v1/models "));
    assert!(request
        .to_ascii_lowercase()
        .contains("authorization: bearer test-only-value"));
    assert_eq!(
        refreshed.config.providers["a"].discovery.supported,
        Some(true)
    );
    assert_eq!(
        refreshed.config.providers["a"].discovery.models,
        vec!["gpt-5"]
    );
    assert!(!merged_model_catalog(&refreshed.config)
        .unwrap()
        .contains_key("unknown-catalog-model"));
    assert!(resolve_available_route(&refreshed.config, None, None, Some("a")).is_ok());
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("test-only-value"));
    let (address, server) = mock_models_server(
        "500 Internal Server Error",
        r#"{"error":"sensitive-provider-response"}"#,
    );
    let mut file = refreshed.config;
    file.providers.get_mut("a").unwrap().base_url = address;
    file.providers.get_mut("a").unwrap().auth = AuthConfig::default();
    write_fixture(&path, &file);
    let revision = read_model_routing_draft(&path).unwrap().revision;
    let failed = refresh_provider_models(&path, &revision, "a")
        .await
        .unwrap();
    server.join().unwrap();
    assert_eq!(failed.config.providers["a"].discovery.supported, None);
    assert!(failed.config.providers["a"].discovery.models.is_empty());
    assert!(resolve_available_route(&failed.config, None, None, Some("a")).is_err());
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("sensitive-provider-response"));
}

#[tokio::test]
async fn remote_discovery_returns_all_models_without_persisting_or_filtering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let (address, server) = mock_models_server(
        "200 OK",
        r#"{"data":[{"id":"unknown-remote"},{"id":"gpt-5"},{"id":"unknown-remote"},{"id":"chat-only"}]}"#,
    );
    let mut file = fixture();
    file.providers.get_mut("a").unwrap().base_url = address;
    file.providers.get_mut("a").unwrap().auth.credential = Some("remote-key".into());
    set_credential(&path, "remote-key", "test-only-value").unwrap();
    write_fixture(&path, &file);
    let before = std::fs::read(&path).unwrap();

    let models = discover_provider_models(&path, "a").await.unwrap();

    assert_eq!(models, vec!["unknown-remote", "gpt-5", "chat-only"]);
    assert!(server.join().unwrap().starts_with("GET /v1/models "));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let persisted = read_model_routing_draft(&path).unwrap();
    assert_eq!(
        persisted.config.providers["a"].discovery,
        file.providers["a"].discovery
    );
}

#[tokio::test]
async fn refresh_persists_only_exact_catalog_models_in_discovery_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let (address, server) = mock_models_server(
        "200 OK",
        r#"{"data":[
            {"id":"local-compatible"},
            {"id":"gpt-5"},
            {"id":"local-compatible"},
            {"id":"gpt-5.3-codex"},
            {"id":"local-chat-only"},
            {"id":"GPT-5"},
            {"id":"gpt-5-preview"},
            {"id":"unknown-catalog-model"}
        ]}"#,
    );
    let mut file = fixture();
    file.models.insert(
        "local-compatible".into(),
        ModelCatalogEntry {
            vendor: "openai".into(),
            display_name: "Local compatible".into(),
            supported_protocols: Vec::from([ProviderKind::OpenAiResponses]),
            supported_reasoning_efforts: BTreeSet::new(),
            enabled: true,
            priority: 0,
        },
    );
    file.models.insert(
        "local-chat-only".into(),
        ModelCatalogEntry {
            vendor: "openai".into(),
            display_name: "Local chat only".into(),
            supported_protocols: Vec::from([ProviderKind::OpenAiChatCompletions]),
            supported_reasoning_efforts: BTreeSet::new(),
            enabled: true,
            priority: 0,
        },
    );
    file.providers.get_mut("a").unwrap().base_url = address;
    write_fixture(&path, &file);
    let draft = read_model_routing_draft(&path).unwrap();

    let refreshed = refresh_provider_models(&path, &draft.revision, "a")
        .await
        .unwrap();
    assert!(server.join().unwrap().starts_with("GET /v1/models "));
    assert_eq!(
        refreshed.config.providers["a"].discovery.models,
        vec![
            "local-compatible",
            "gpt-5",
            "gpt-5.3-codex",
            "local-chat-only"
        ]
    );
}

#[tokio::test]
async fn refresh_with_no_compatible_local_models_is_successful_but_unroutable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let (address, server) = mock_models_server(
        "200 OK",
        r#"{"data":[{"id":"unknown-catalog-model"},{"id":"claude-opus-5"}]}"#,
    );
    let mut file = fixture();
    file.providers.get_mut("a").unwrap().base_url = address;
    write_fixture(&path, &file);
    let draft = read_model_routing_draft(&path).unwrap();

    let refreshed = refresh_provider_models(&path, &draft.revision, "a")
        .await
        .unwrap();
    assert!(server.join().unwrap().starts_with("GET /v1/models "));
    let discovery = &refreshed.config.providers["a"].discovery;
    assert_eq!(discovery.supported, Some(true));
    assert!(discovery.last_error.is_none());
    assert!(discovery.models.is_empty());
    assert!(resolve_available_route(&refreshed.config, None, None, Some("a")).is_err());
}

fn assert_refresh_failure(
    path: &Path,
    previous: &ModelRoutingDraft,
    failed: &ModelRoutingDraft,
    supported: Option<bool>,
) {
    let provider = &failed.config.providers["a"];
    assert_eq!(provider.discovery.supported, supported);
    assert!(provider.discovery.models.is_empty());
    assert!(provider
        .discovery
        .last_error
        .as_ref()
        .is_some_and(|s| !s.is_empty()));
    assert!(provider
        .discovery
        .last_checked_at
        .is_some_and(|time| time > 1));
    assert!(provider_discovery_matches_current_config("a", provider));
    assert!(resolve_available_route(&failed.config, None, None, Some("a")).is_err());
    assert_eq!(failed.config.providers["b"], previous.config.providers["b"]);
    assert_ne!(failed.revision, previous.revision);
    let persisted = read_model_routing_draft(path).unwrap();
    assert_eq!(persisted.revision, failed.revision);
    assert_eq!(persisted.config.providers["a"], *provider);
    let content = std::fs::read_to_string(path).unwrap();
    assert!(!content.contains("sensitive-provider-response"));
}

#[tokio::test]
async fn refresh_http_and_invalid_response_failures_preserve_unknown_except_404() {
    for (status, body, supported) in [
        (
            "404 Not Found",
            r#"{"error":"sensitive-provider-response"}"#,
            Some(false),
        ),
        (
            "401 Unauthorized",
            r#"{"error":"sensitive-provider-response"}"#,
            None,
        ),
        (
            "403 Forbidden",
            r#"{"error":"sensitive-provider-response"}"#,
            None,
        ),
        (
            "429 Too Many Requests",
            r#"{"error":"sensitive-provider-response"}"#,
            None,
        ),
        (
            "500 Internal Server Error",
            r#"{"error":"sensitive-provider-response"}"#,
            None,
        ),
        ("200 OK", "sensitive-provider-response", None),
        ("200 OK", r#"{"data":"sensitive-provider-response"}"#, None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (address, server) = mock_models_server(status, body);
        let mut file = fixture();
        file.defaults.vendor = Some("deepseek".into());
        file.defaults.model = Some("deepseek-flash".into());
        let provider = file.providers.get_mut("a").unwrap();
        provider.base_url = address;
        provider.discovery.models = vec!["deepseek-flash".into()];
        provider.discovery.config_fingerprint =
            Some(provider_connection_fingerprint("a", provider));
        assert!(resolve_available_route(&file, None, None, Some("a")).is_ok());
        write_fixture(&path, &file);
        let draft = read_model_routing_draft(&path).unwrap();
        // 网络失败仍返回已保存的草稿；调用方依据 last_error 判定失败。
        let failed = refresh_provider_models(&path, &draft.revision, "a")
            .await
            .unwrap();
        assert!(server.join().unwrap().starts_with("GET /v1/models "));
        assert_refresh_failure(&path, &draft, &failed, supported);
    }
}

#[tokio::test]
async fn refresh_transport_failure_is_unknown_and_revokes_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // 接收请求后直接断开，不依赖外网、端口竞争或长超时。
    let (address, server) = mock_http_server(String::new());
    let mut file = fixture();
    let provider = file.providers.get_mut("a").unwrap();
    provider.base_url = address;
    provider.discovery.config_fingerprint = Some(provider_connection_fingerprint("a", provider));
    write_fixture(&path, &file);
    let draft = read_model_routing_draft(&path).unwrap();
    let failed = refresh_provider_models(&path, &draft.revision, "a")
        .await
        .unwrap();
    server.join().unwrap();
    assert_refresh_failure(&path, &draft, &failed, None);
}

#[tokio::test]
async fn refresh_missing_credential_reference_is_unknown_and_still_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let credential_name = format!(
        "missing-auth-{}",
        rivo_storage::sha256_text(&dir.path().to_string_lossy())
    );
    let mut file = fixture();
    let provider = file.providers.get_mut("a").unwrap();
    provider.auth.credential = Some(credential_name.clone());
    provider.base_url = "http://127.0.0.1:1/v1".into();
    provider.discovery.config_fingerprint = Some(provider_connection_fingerprint("a", provider));
    write_fixture(&path, &file);
    let draft = read_model_routing_draft(&path).unwrap();
    assert!(matches!(
        refresh_provider_models(&path, &draft.revision, "a").await,
        Err(SessionError::MissingApiKey(name)) if name == credential_name
    ));
    let failed = read_model_routing_draft(&path).unwrap();
    assert_refresh_failure(&path, &draft, &failed, None);
    assert_eq!(
        failed.config.providers["a"].discovery.last_error.as_deref(),
        Some("Provider 认证引用不可用")
    );
}

#[test]
fn injected_config_parses_current_schema_without_touching_disk() {
    let content = toml::to_string_pretty(&fixture()).unwrap();
    let draft = parse_injected_config(&content).unwrap();
    assert_eq!(draft.revision, rivo_storage::sha256_text(&content));
    assert_eq!(draft.config.defaults.provider_id.as_deref(), Some("a"));
    assert!(draft.config.providers.contains_key("b"));
}

#[test]
fn injected_config_rejects_schemas_that_would_require_a_file_upgrade() {
    for content in [
        r#"
version = 3
provider = "openai_chat_completions"
model = "gpt-5"
base_url = "https://gateway.example/v1"
"#,
        r#"
[defaults]
model = "gpt-5"
"#,
    ] {
        assert!(matches!(
            parse_injected_config(content),
            Err(SessionError::InvalidConfig(_))
        ));
    }
}

#[test]
fn injected_config_error_does_not_echo_the_injected_document() {
    let secret = "fixture-injected-canary-key";
    let content = format!("version = 6\n[defaults]\nunknown_field = \"{secret}\"\n");
    let error = parse_injected_config(&content).unwrap_err().to_string();
    assert!(
        !error.contains(secret),
        "error leaked injected content: {error}"
    );
}

fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
        && (1..=12).contains(&value[5..7].parse::<u8>().unwrap())
        && (1..=31).contains(&value[8..10].parse::<u8>().unwrap())
}

#[test]
fn retired_local_models_and_default_model_produce_catalog_warnings() {
    let mut file = fixture();
    assert!(catalog_warnings(&file).unwrap().is_empty());

    // 用户配置里残留的上游已下线条目仍可读取，但会被标记为需要清理。
    file.models.insert(
        "gpt-5.6-luna".into(),
        ModelCatalogEntry {
            vendor: "openai".into(),
            display_name: "gpt-5.6-luna".into(),
            supported_protocols: [ProviderKind::OpenAiResponses].into_iter().collect(),
            supported_reasoning_efforts: BTreeSet::new(),
            enabled: true,
            priority: 0,
        },
    );
    file.defaults.model = Some("gpt-5.6-luna".into());
    validate_model_routing_config(&file).unwrap();
    let catalog = read_default_catalog().unwrap();
    assert_eq!(
        model_lifecycle(&catalog, "gpt-5.6-luna"),
        (ModelLifecycleStatus::Retired, Some("gpt-6-luna".into()))
    );
    assert_eq!(
        model_lifecycle(&catalog, "gpt-6-luna"),
        (ModelLifecycleStatus::Active, None)
    );
    let warnings = catalog_warnings(&file).unwrap();
    assert_eq!(
        warnings
            .iter()
            .map(|w| (w.code, w.model.as_str(), w.replaced_by.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("retired_local_model", "gpt-5.6-luna", Some("gpt-6-luna")),
            ("default_model_retired", "gpt-5.6-luna", Some("gpt-6-luna")),
        ]
    );

    // 删除本地条目后 defaults.model 指向缺失模型，严格校验给出替代建议。
    file.models.clear();
    let warnings = catalog_warnings(&file).unwrap();
    assert_eq!(warnings[0].code, "default_model_missing");
    assert_eq!(warnings[0].replaced_by.as_deref(), Some("gpt-6-luna"));
    let error = validate_model_routing_config(&file)
        .unwrap_err()
        .to_string();
    assert!(error.contains("建议改用 gpt-6-luna"), "{error}");
}

#[test]
fn default_catalog_lifecycle_rules_reject_inconsistent_entries() {
    let mut catalog = read_default_catalog().unwrap();
    catalog
        .retired_models
        .insert("gpt-5".into(), ModelLifecycleNote::default());
    assert!(validate_default_catalog(&catalog).is_err());

    let mut catalog = read_default_catalog().unwrap();
    catalog
        .deprecated_models
        .insert("gpt-4o".into(), ModelLifecycleNote::default());
    assert!(validate_default_catalog(&catalog).is_err());

    let mut catalog = read_default_catalog().unwrap();
    catalog.deprecated_models.insert(
        "gpt-5".into(),
        ModelLifecycleNote {
            replaced_by: Some("gpt-5.6-luna".into()),
        },
    );
    assert!(validate_default_catalog(&catalog).is_err());

    let mut catalog = read_default_catalog().unwrap();
    catalog.deprecated_models.insert(
        "gpt-5".into(),
        ModelLifecycleNote {
            replaced_by: Some("gpt-6-luna".into()),
        },
    );
    validate_default_catalog(&catalog).unwrap();
    assert_eq!(
        model_lifecycle(&catalog, "gpt-5"),
        (ModelLifecycleStatus::Deprecated, Some("gpt-6-luna".into()))
    );
}
