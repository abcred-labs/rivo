# Provider 地址归属与模型协议契约（schema 6）

- 当前格式：schema 6，协议只由 Model 决定，Provider 不再携带协议。
- 本文规定共享模型路由与地址边界；平台验收见[发布流程](../runbooks/releasing.zh-CN.md)。
- 模型领域仅保留 Vendor、Model、Provider 与独立 Credential。本文不改变模式/policy profile、审批绑定、快捷键或 MCP/A2A/instance-control 的非模型 endpoint；中心监督已退出本仓库交付。

## 1. 地址与对象生命周期

Provider 自有 `base_url`、`auth`、`enabled`、`priority` 与 `discovery`，不携带协议。Model 保存 canonical ID、`vendor`、`display_name`、按具体 Model ID 核验且按偏好排序的 `supported_protocols` 与既有 `priority`；Vendor 只保存发布方身份与展示元数据。多个 Model 是否可由同一 Provider 服务由完整发现 ID 精确匹配派生，不保存人工映射对象。

- 不保留模型 ProviderProfile、ModelBinding 或独立 Endpoint 的运行时对象、alias 或请求模型 ID 重写。已支持的历史文件格式只可在加载时按[持久化文件兼容与自动升级契约](persistence-file-compatibility.zh-CN.md)转换为当前对象；迁移完成后不保留双格式运行路径。
- 相同 URL 的 Provider 是独立对象；更新或删除 A 不改动 B。共享只意味着选择同一 Provider ID，不创建共享地址资源。
- Credential 独立安全存储，Provider 配置仅允许可选 `auth.credential` 引用。环境变量名不持久化：仅 `rivo run --api-key-env <name>` 在本次执行读取外部注入的变量；非空 env 优先，缺失、空白或不可读取时使用 Provider Credential 后备。未配置 Credential 时允许无认证调用；已配置但缺失、损坏或不可解密时明确失败。远端认证或传输失败不切换来源；无认证请求不得发送空认证头。
- 删除 Provider 即使移除最后一个引用也不删除 Credential；只有独立、明确授权的凭据操作能管理正文。
- `base_url` 必填且非空，沿用既有 URL、协议与路径拼接校验；它是协议请求路径前缀，不是完整 `/responses`、`/chat/completions` 或 `/messages` 地址。拒绝 userinfo 和 URL 中的认证秘密，不套用 Web fetch 的私网禁用规则。

## 2. 模型配置 schema 6

`version = 6` 是模型配置格式版本，不是产品 V3。非模型配置字段与安全语义不变。schema 5 引入逐模型 `supported_protocols`；schema 6 将其改为有序列表并移除 Provider `protocol`。协议不能从 Vendor、模型前缀、Provider 或 discovery 结果推断。模型字段如下：

| 位置 | 字段与约束 |
| --- | --- |
| 顶层 | 必须显式 `version = 6`；模型域仅 `vendors`、`models`、`providers` 及 `defaults` 中的模型选择 |
| `vendors.<id>` | `display_name`；不携带连接信息 |
| `models.<canonical-id>` | `vendor`、`display_name`、非空且无重复的有序 `supported_protocols`（第一项为路由协议）、`priority`；ID 为完整且区分大小写的 canonical ID |
| `providers.<id>` | `base_url`、`auth`、`enabled`、`priority`、`discovery`；不得存在 `protocol` 或 `endpoint` 字段 |
| `defaults` | `vendor`、`model`、可选 `provider_id`；省略 `provider_id` 表示 Auto；删除旧模型 `profile` 选择字段，基础模式仍用既有 capability/profile 语义 |
| `auth` | 可选 `credential`，不得含正文；本次运行的 env 名只通过 `rivo run --api-key-env` 提供 |
| `discovery` | `models: string[]`、`supported: bool?`、`last_checked_at: u64?`、`last_error: string?`、`config_fingerprint: string?`；`models` 只保存经认证发现、与本地合并目录 exact ID 匹配、已启用且至少有一个协议的 canonical ID；沿用既有刷新时间单位（Unix 秒），只有显式 refresh 更新，普通配置草稿或只读 remote 查询不能伪造成功证据 |

缺省 discovery 为 `models=[]`、其余字段为空；`supported=true` 也必须无最近刷新错误且有匹配指纹和 exact ID，才可作为候选。远端发现成功但没有本地已启用目录 Model 时仍写入 `supported=true` 与空列表，明确区分连接成功和无可选模型。失败刷新写入脱敏错误并撤销可用性，不能只保留旧成功状态；新增或启用本地 Model 后必须再次 refresh，旧缓存不会自动改写；调整模型协议列表不影响 discovery。`enabled` 缺省 true、Model/Provider `priority` 缺省 0；协议与地址不从旧连接环境变量推断。协议标识固定沿用 `openai_chat_completions/openai_responses/claude_messages`。

Provider 与 Model 的 priority 均在 `0..=1000`。缺省值沿用现有定义，不改变默认 Model 选择规则。以下是目标结构示意，不是已验证的连接配置；未给 discovery 的 Provider 不可运行：

```toml
version = 6

[vendors.example]
display_name = "示例发布方"

[models."example-model-1"]
vendor = "example"
display_name = "示例模型"
supported_protocols = ["openai_responses", "openai_chat_completions"]
priority = 100

[providers.gateway]
base_url = "https://gateway.example/v1"
enabled = true
priority = 100
[providers.gateway.auth]
credential = "gateway-prod"

[defaults]
vendor = "example"
model = "example-model-1"
```

当前对象严格校验未知字段和安全语义。缺少、旧版 `version`、历史 `profiles/endpoints/bindings` 与旧模型默认字段必须先由[持久化文件兼容与自动升级契约](persistence-file-compatibility.zh-CN.md)中的确定性迁移链转换；迁移后仍有未知、冲突或无法保留语义的字段才整体拒绝。未知未来版本始终拒绝且零写入。错误指向字段和迁移状态，不打印原始配置正文；失败时不联网、不回退官方地址，也不将旧文件当作空配置覆盖。

## 3. 唯一目录与自动路由

CLI、TUI、Session、app-server、双向可用性查询共用唯一 resolver 和有效目录（版本内置目录 + 本地合法补充/协议列表与 priority 覆盖）。内置目录以接入程序的 defaults 数据为唯一来源，不维护另一份硬编码目录或脱离运行路径的缓存。列表标明内置/本地/覆盖来源；本地不得重定义内置 Vendor 或 Model 的 `vendor`/`display_name`，发现响应不得创建 Vendor/Model。

协议只由 Model 决定，与 Provider 无关。单个可路由候选必须同时满足：

```text
Model 有效 supported_protocols 非空（路由协议 = 第一项）
    ∧ Provider authenticated discovery exact model IDs 含该 Model
```

有效 `supported_protocols` = 内置目录列表，或本地 `[models.<id>]` 覆盖后的完整列表；顺序即偏好，路由始终取第一项，不在 Provider 之间或协议之间 runtime failover。discovery 只证明该 Provider 入口当前返回了该完整 ID，不证明该入口支持哪个协议；Rivo 不探测、不记录 Provider 协议能力。

内置偏好顺序：OpenAI 多协议模型 Responses 优先；DeepSeek、GLM 等多协议模型 Chat Completions 优先；Anthropic 模型仅 Messages。用户可通过 `config model set <id> --enable-protocol/--disable-protocol/--protocols` 增加、禁用或重排协议，结果作为本地覆盖保存。Provider 产品或套餐限制同样在模型上表达，例如 Z.AI Coding Plan 只提供 Chat Completions 时，在 `glm-5.3` 上禁用 Messages/Responses；该覆盖对所有 Provider 生效。

同一 `base_url` 需要服务全部所需协议（推荐以 `/v1` 结尾，三协议分别拼接 `/chat/completions`、`/responses`、`/messages`）。若同一发布方的不同协议位于不同路径前缀，应拆分为不同 Provider，并在模型上禁用该入口不可用的协议。

当前内置目录的核验摘要如下；它是目录更新证据的最小记录，不允许按 Vendor 或名称前缀批量扩展：

默认目录只登记官方公开、稳定且可直接作为 API `model` 参数使用的主流模型；预览、实验、内部别名、未来编号和第三方网关专用名称不进入默认目录。

新增、弃用和下线内置模型的操作步骤见[模型目录生命周期](model-catalog-lifecycle.zh-CN.md)。

下表“是”的列按 `defaults.toml` 中的列表顺序排列偏好；首选协议见上文内置偏好顺序。

| Model ID | Chat Completions | Responses | Claude Messages |
| --- | ---: | ---: | ---: |
| `claude-haiku-4-5`、`claude-opus-4-6`、`claude-opus-5-5`、`claude-sonnet-4-5`、`claude-sonnet-4-6`、`claude-sonnet-5-5` | 否 | 否 | 是 |
| `deepseek-flash`、`deepseek-v4-flash`、`deepseek-v4-pro` | 是 | 是 | 是 |
| `gpt-5`、`gpt-5.1`、`gpt-5.2`、`gpt-5.2-chat-latest` | 是 | 是 | 否 |
| `gpt-5.3-codex` | 否 | 是 | 否 |
| `gpt-6-astra`、`gpt-6.1-sol`、`gpt-6-sol`、`gpt-6-luna` | 是 | 是 | 否 |
| `grok-4` | 是 | 待确认 | 否 |
| `glm-5.3` | 是 | 是 | 是 |
| `glm-5.3-flash` | 是 | 待确认 | 待确认 |

证据来源已逐模型固化在 [`defaults.toml`](../../runtime/crates/session/src/defaults.toml) 注释中；Vendor 级入口为 OpenAI [Models](https://platform.openai.com/docs/models)、Anthropic [Models](https://docs.anthropic.com/en/docs/about-claude/models)、DeepSeek [Models & Pricing](https://api-docs.deepseek.com/quick_start/pricing)、[Responses API](https://api-docs.deepseek.com/guides/responses_api)、xAI [Models](https://docs.x.ai/docs/models)、Z.AI [GLM-5.3](https://docs.z.ai/guides/llm/glm-5.3)。本次目录收缩移除未能按稳定官方目录确认的旧条目；移除不等于为本地自定义模型增加限制。

2026-10-10 核验：Arai `pgsql` 中 `claude-opus-5-5`、`claude-sonnet-5-5` 和 `gpt-6.1-sol` 均为启用且未过期，并分别提供 Claude Messages 或 OpenAI Chat Completions/Responses 路由；查询审计为 `3a50b4327fc8473b814003705c85c5a4`。官方目录存在只证明发布方的公开身份与文档存在，不证明本地 Provider discovery 或 live smoke 已通过；后两者仍按 Provider 和候选环境单独验证。

1. 使用显式或已配置默认 Vendor/Model。只有未选 Model 时，按该 Vendor 的 Model priority 降序、Model ID 升序确定一个模型；不能因它不可用而换低优先级模型。
2. 校验 Model 属于所选 Vendor。将该完整 canonical ID 与 Provider 认证 discovery 的每个完整 ID 精确比较；不忽略大小写、不移除前后缀、不 alias。
3. Provider 必须 enabled、发现 supported、最近一次刷新成功、配置指纹匹配且含 exact ID。未刷新、刷新失败、禁用、ID 缺失、指纹不符均拒绝；旧成功缓存不能覆盖最近失败。
4. 显式 Provider 只检查该 ID，不可用即拒绝。Auto 按 Provider priority 降序、Provider ID 升序选择一次，不用 Binding ID 排序。
5. 请求直接使用 `model`；公共路由包含 `vendor/model/provider_id/protocol/base_url/priority`，其中 `protocol` 取自 Model 有效协议列表第一项；不含 `binding_id` 或独立 `request_model_id`，也不含 `auth` 正文。`priority` 指选中 Provider 的优先级，Model priority 是目录元数据，不用旧复合 score 改变排序。

配置指纹包含 Provider 身份、有效 base_url 与认证引用，不含协议；连接字段变化立即使 discovery 失效，调整模型协议列表不使 discovery 失效。discovery 先以 Bearer 认证请求 `/v1/models`，仅在 401/403 时以 `x-api-key` + `anthropic-version` 重试一次；实际请求的认证头仍按路由协议选择。优先级变化不证明重新验证了连接。刷新时间只用于呈现，本次不增加墙钟 TTL 规则，不能宣称已有 TTL 过期实现。`list/show/validate/route` 均离线且不写入，只有显式 `provider models refresh` 请求目录。

## 4. 变更、删除与运行快照

全部获授权配置写入口共用 revision、计划确认、提交锁和原子替换，不给 CLI/TUI/RPC 任何覆盖优先权。revision 是完整配置原始字节的 SHA-256，而非 mtime/界面计数；计划 token 绑定文件身份、revision、操作和计划摘要，不替代授权。

- 新建 Provider 必须给地址；更新省略字段保留原值。配置维护不要求 discovery 已成功，不因连接暂不可用阻止修复。
- 2026-09-09，CLI `provider set` 已实现更新省略字段保留原值，CLISchema3 测试已通过，覆盖省略保留、同 URL Provider 隔离、部分连接更新确认及无效更新不落盘。使用说明按 [当前 CLI 接口与差异](cli.zh-CN.md) 执行；此项通过不代表全量候选验收完成，整体仍保持 `in_progress`。
- 地址/认证引用变更及删除先生成 `target_kind/target_id`、`affected_models`、`impact_complete`、`clears_default_route` 与 revision 的影响计划。`config provider|model|vendor remove` 只向 CLI 用户显示不含 `confirmation_token` 的计划，并以 `[y/N]` 或本命令 `--yes` 确认；提交时仍在内部校验 token。`affected_models` 是 `{vendor, model}` 列表，由有效目录与该 Provider 当前发现 ID 的交集计算，不是 bindings 清单；失效缓存只能用于标注影响，不能据此认定可用。没有发现信息或无法完整判定时 `impact_complete=false`，不宣称“零影响”。
- 删除 Provider 不删除 Model/Vendor；只清除指向它的默认 Provider 选择并明确提示下次为 Auto，不能在删除过程中启动 turn。删除 Vendor/Model 仍需完整性检查，禁止留下孤立目录引用或静默级联删除无关配置。
- 提交锁内重读并比较 revision，重新校验计划，使用同目录临时文件原子替换。冲突、缺确认、无效字段或写失败不部分落盘；外部编辑器不遵守锁，替换前再次核对源字节。取消草稿不落盘。
- 活动 turn 与其 retry 持有冻结的连接身份、协议、base_url、model 和认证引用快照，不因配置变更或传输失败重新解析。后续 turn 重新解析；恢复新会话不把历史快照当作当前可用性证明。
- 路由、计划、日志、持久事件与 RPC 不保存 Credential 正文、认证 header 或密钥环境变量值。内部执行快照与公开路由分离。

## 5. 文件格式自动升级与会话重置边界

`config.toml` 与 `credentials.toml` 的支持历史格式必须在启动时自动升级，不建立双格式运行时或永久旧模型对象。格式版本、迁移原子性、Credential 保密性、未来版本零写入和验收规则以[持久化文件兼容与自动升级契约](persistence-file-compatibility.zh-CN.md)为准。

模型语义迁移只能应用已登记的确定性映射：

1. 映射必须保留稳定 ID、Credential 名称与 Provider 地址；不能凭旧名字猜 Model、复制 discovery 成功状态、产生 alias，或静默改变默认选择。schema 4/5 的 Provider `protocol` 在升级到 schema 6 时移除（协议改由 Model 决定）；仅当旧 discovery 指纹与旧连接身份一致时按新指纹重新绑定。旧扁平格式的单一协议选择转为该内置 Model 的本地协议顺序覆盖。
2. 迁移后的 Provider、Model 与 Credential 引用继续满足本契约的 schema 6 校验。新 Provider 的地址、协议、Model 与 Credential 引用仍须按既有确认规则提交；显式 refresh 后才能运行。
3. 无法安全映射、损坏或来自未来版本的文件明确失败并保留原始文件，不得改为受控重置或清空。
4. SQLite 旧会话 schema 与旧模型事件不在本自动升级范围内，仍须先定位实际表和外键后受控重置会话归属数据；保留 Credential、记忆、MCP、policy、A2A 身份/信任及其他非会话数据，混合数据库不得整库删除。

## 6. 验收边界

| 验收 ID | 必须验证 |
| --- | --- |
| ROUTING-01 | 新 schema 独立配置、首启、查询、运行、TUI、RPC、重启恢复；每个支持旧配置格式自动升级、幂等并保留语义，损坏/未来格式零写入拒绝 |
| ROUTING-02 | 一模型多 Provider、多 Vendor 共用 Provider、同一 Provider 按各 Model 首选协议服务、本地协议覆盖、exact ID 与大小写/前后缀负例、稳定排序、默认 Model 不变 |
| ROUTING-03 | 显式 Provider 不可用拒绝；未刷新/失败/禁用/指纹不符拒绝；不 alias、不 runtime failover |
| ROUTING-04 | 同 URL 隔离、派生影响、未知影响、revision 冲突、确认 token、取消及原子写失败 |
| ROUTING-05 | 活动 turn/retry 固定快照，后续 turn 与恢复重新校验；新会话不生成模型 profile 列或旧事件 |
| ROUTING-06 | 查询离线；Credential 不泄漏；配置与凭据升级原子且无网络副作用；会话受控重置保留非会话数据；Named Pipe/stdio/A2A/LAN 权限不扩大 |

阶段 A 只验证文档契约与链接。实现阶段需全量质量门、Windows ConPTY、三协议三个 target（1/1/1）和 A2A 同候选回归。历史 evidence 保留审计用途，不能重标新候选 passed。

---

[English](provider-endpoint-ownership.md)
