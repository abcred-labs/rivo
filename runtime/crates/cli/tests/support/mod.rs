use rivo_session::{
    provider_connection_fingerprint, AuthConfig, FileConfig, ModelCatalogEntry,
    ModelDiscoveryCache, ProviderConfig, VendorCatalogEntry,
};
use std::collections::BTreeSet;
use std::path::Path;

/// 隔离测试目录中的 schema3 路由夹具，不读取本机配置或凭据。
pub fn write_routing_fixture(
    path: &Path,
    workspace: &Path,
    base_url: &str,
    model: &str,
    _key_env: &str,
) {
    let mut file = FileConfig::default();
    file.vendors.insert(
        "fixture".into(),
        VendorCatalogEntry {
            display_name: "Fixture".into(),
        },
    );
    file.models.insert(
        model.into(),
        ModelCatalogEntry {
            vendor: "fixture".into(),
            display_name: model.into(),
            supported_protocols: Vec::from([rivo_llm::ProviderKind::OpenAiChatCompletions]),
            supported_reasoning_efforts: BTreeSet::new(),
            enabled: true,
            priority: 0,
        },
    );
    let mut provider = ProviderConfig {
        base_url: base_url.into(),
        auth: AuthConfig { credential: None },
        enabled: true,
        priority: 0,
        discovery: ModelDiscoveryCache::default(),
    };
    provider.discovery = ModelDiscoveryCache {
        config_fingerprint: Some(provider_connection_fingerprint("fixture", &provider)),
        supported: Some(true),
        models: vec![model.into()],
        last_checked_at: Some(1),
        last_error: None,
    };
    file.providers.insert("fixture".into(), provider);
    file.defaults.vendor = Some("fixture".into());
    file.defaults.model = Some(model.into());
    file.defaults.workspace = Some(workspace.into());
    std::fs::write(
        path,
        toml::to_string_pretty(&file).expect("fixture serializes"),
    )
    .expect("write isolated fixture");
}
