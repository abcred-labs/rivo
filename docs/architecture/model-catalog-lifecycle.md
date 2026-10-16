# 模型目录生命周期

内置模型目录唯一来源是 [`defaults.toml`](../../runtime/crates/session/src/defaults.toml)，编译进 `rivo` 二进制。CLI、TUI 和 app-server 的 `catalog/read` 共用它，Desktop 不维护自己的模型列表。因此：

> **在 `defaults.toml` 中新增模型并发布新版本 rivo 后，客户端无需改动即可看到并配置该模型。**

收录标准见 [Provider 地址契约 §3](provider-endpoint-ownership.md#3-唯一目录与自动路由)：只登记官方公开、稳定、可直接作为 API `model` 参数的 ID，逐模型核验，不按前缀批量扩展。

## 新增模型

1. 在 `[models]` 中按字母序添加条目。前面必须写两行证据注释：
   ```toml
   # official_model = https://<官方模型页>
   # verified_at = YYYY-MM-DD
   [models."<id>"]
   vendor = "<已有 vendor>"
   display_name = "<id 或官方显示名>"
   supported_protocols = ["openai_responses"]
   # 仅在官方确认时填写；留空表示由 Provider 默认处理
   # supported_reasoning_efforts = ["low", "medium", "high"]
   ```
2. 更新 [provider-endpoint-ownership.md](provider-endpoint-ownership.md) 的协议矩阵，并在 `model_routing_tests.rs` 的 `builtin_catalog_is_used_by_queries_and_routing` 中更新模型数量和 ID 列表。
3. 若该模型取代旧模型，同时按下文把旧模型标记为弃用。
4. 运行 `cargo test -p rivo-session --lib model_routing` 与 `cargo test -p rivo --test cli_black_box e2e_app_server_config_management`。
5. 发布 rivo。下游 Desktop 需要更新 `rivo-runtime.lock` 并重新打包；runtime 的 `catalog_revision` 变化后，宿主应重新读取目录。

已有 `[models."<id>"]` 本地条目的用户：本地条目身份（vendor、display_name、supported_protocols）必须与新内置条目一致，否则配置读取会报“不允许重定义内置身份”。新增前应确认该 ID 未被常见网关作为自定义模型使用；若不可避免，在发布说明中提示用户删除本地条目。

## 弃用模型（计划下线）

1. 模型保留在 `[models]` 中，在 `[deprecated_models]` 登记：
   ```toml
   [deprecated_models]
   "<old-id>" = { replaced_by = "<new-id>" }
   ```
2. `replaced_by` 必须是未弃用的内置模型；没有明确后继时写 `{}`。
3. 效果：`catalog/read` 中该模型 `status = "deprecated"`；若用户 `defaults.model` 指向它，返回 `default_model_deprecated` 警告。路由行为不变。
4. 至少保留一个发布版本，给用户迁移时间。

## 下线模型

1. 从 `[models]` 删除条目，同时从 `[deprecated_models]` 移到 `[retired_models]`（保留 `replaced_by`）。
2. **墓碑永不删除。** 它阻止旧 ID 被无意重新加入（测试会检查），并让客户端识别用户配置中的残留条目。
3. 效果：
   - 用户未覆盖的条目直接从目录消失。
   - 用户在 `config.toml` 中有本地条目时，它作为本地模型继续可用（仍需 Provider discovery 命中），但 `catalog/read` 标记 `status = "retired"` 并返回 `retired_local_model` 警告，宿主应提示清理。
   - `defaults.model` 指向已下线模型时，返回 `default_model_missing` 或 `default_model_retired` 警告；严格校验的错误信息会带上替代建议。

## 校验

`validate_default_catalog` 在测试中保证：

- 弃用项仍在 `[models]` 中；
- 墓碑不在 `[models]` 中；
- `replaced_by` 指向未弃用的内置模型；
- 每个模型都有 `official_model` 与合法的 `verified_at` 日期。
