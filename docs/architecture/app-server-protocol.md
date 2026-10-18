# App-server stdio protocol

`rivo app-server --stdio` is a local process bridge. V1 also supports `rivo app-server --pipe`, which exposes a current-user Windows Named Pipe derived from `RIVO_DATA_DIR`; the pipe transport accepts bounded JSON-lines requests and never exposes credentials or arbitrary PID control. A host is responsible for authentication, transport encryption, relay tokens and user identity.

Every stdout line is one UTF-8 JSON-RPC 2.0 object. Requests must have an `id`; state-changing requests must also have a bounded non-empty `idempotency_key`. The fixed handshake version is `rivo-app-server-v2`.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"rivo-app-server-v2","client":{"name":"host","version":"1.0"}}}
```

The successful `initialize` result has this schema:

```json
{
  "server":{"name":"rivo","version":"<local-version>"},
  "capabilities":{"supports_resume":true,"supports_interrupt":true,"supports_history_replay":true,"emits_tool_events":true,"emits_media_events":true,"supports_media_refs":true,"supports_media_import":true,"supports_image_input":true,"supports_approval":true,"supports_subagents":true,"model_routing_config":{"read":true,"preview":true,"write":true,"credential_upsert":true},"memory":{"list":true,"confirm":true,"reject":true,"delete":true}},
  "limits":{"max_request_bytes":65536,"max_string_bytes":32768,"idempotency_cache_entries":256},
  "supported_runtime_settings":{"vendor":true,"model":true,"provider_id":true,"auto_provider":true,"collaboration_mode":["plan","code"],"permission_preset":["read_only","workspace_write","full_access"],"network":true,"mcp_servers":true,"mcp_toolsets":true,"reasoning_effort":["default","low","medium","high"],"subagents_enabled":true,"max_subagents":{"min":1,"max":32}}
}
```

`health` has no parameters and returns `app_server`, `session_storage`, `provider_configuration`, and `active_turns`, each as `ok`, `degraded`, or `unavailable`. It does not return paths, credentials, Provider errors, prompts or tool payloads.

Instance-control extensions use the same JSON-RPC framing. Read methods are `health/read`, `instance/list`, `instance/read`, `workspace/list`, `workspace/read`, `session/list`, `session/read`, `turn/list`, and `turn/read`; `project/list` and `project/read` remain compatibility aliases for one deprecation cycle. `workspace/*` and `session/*` read local SQLite metadata. `session/list` accepts an optional `workspace_id`; without it the bridge returns all scoped Workspaces, while unscoped legacy sessions are not implicitly attached. Metadata projections expose only Workspace/Session IDs, timestamps, counters, lifecycle state, event kind and message role; metadata methods never expose workspace paths, prompt/transcript text, event/message bodies, tool arguments or tool results. `session/resume` restores a Session only when the requested working directory matches its Workspace; a mismatch fails before model or tool execution. `turn/interrupt` interrupts its active Turn, `session/send` appends a message through the local Session Intent channel, and `session/fork` allocates a new child Session with a parent reference. `session/subscribe` accepts `session_ref` plus an optional `from_seq` cursor and returns a bounded replay from in-process event history before live broadcast. `session/replay` accepts the same cursor and reconstructs durable SQLite `AgentEvent` journal entries and user messages into the event notification envelope; each response is bounded by the JSON-lines size and returns `next_seq` for pagination. Its sequence counts projected notifications, so one stored journal entry may produce multiple events; persisted user prompts are represented as `user_message` events and all replay text remains bounded and redacted. Lifecycle mutations require the confirmation and idempotency fields. `thread/*`, `thread_ref` and protocol version `rivo-app-server-v1` are unsupported and must be rejected rather than aliased.

LAN requests use `POST /rivo-a2a-lan-v1/instance-control` with JSON-RPC method `instance/control`. A paired A2A peer must explicitly grant the `process-control` Skill and use a `LanControlEnvelope` containing `peer_id`, `target_instance_id`, `method`, bounded `params`, a unique nonce/request id, a bounded idempotency key, and an expiry no more than five minutes in the future. The listener atomically rejects a reused `(peer, nonce)` until expiry, then forwards the validated request through the local Named Pipe client. The A2A profile for `instance_control` is strict-only and cannot enable Web, Browser, or MCP capabilities.

## Session and turn requests

The public hierarchy is `Workspace -> Session -> Turn -> (Message, Event)`. A Session is a durable multi-interaction container bound to exactly one Workspace. A Turn is one accepted prompt through its unique terminal event. `seq` is strictly increasing within a Session and does not reset for a new Turn. Step and subagent Task are internal work units within a Turn, not Session or Turn aliases.

```json
{"method":"session/start","params":{"working_directory":"<configured-local-workspace>","runtime_settings":{},"idempotency_key":"<key>"}}
{"method":"session/resume","params":{"session_ref":"<sqlite-session-id>","working_directory":"<configured-local-workspace>","runtime_settings":{},"idempotency_key":"<key>"}}
```

Both return `{"session_ref":"...","accepted_runtime_settings":{...}}`. The supplied directory must canonically match the workspace from local rivo configuration; this bridge never permits a host to select an arbitrary local directory. `session/start` persists its SQLite session row before returning, even before the first turn. Resume never creates a replacement session: unavailable or expired history returns `session_not_found` or `session_resume_unavailable`.

```json
{"method":"turn/start","params":{"session_ref":"...","prompt":"...","media_refs":[],"runtime_settings":{},"idempotency_key":"<key>"}}
{"method":"session/send","params":{"session_ref":"...","text":"把刚才的猫改成油画","media_refs":[],"idempotency_key":"<key>"}}
{"method":"turn/interrupt","params":{"session_ref":"...","turn_id":"...","idempotency_key":"<key>"}}
{"method":"interaction/respond","params":{"session_ref":"...","interaction_id":"...","decision":"allow|deny|respond","selected_option_id":"<optional>","freeform_text":"<optional>","idempotency_key":"<key>"}}
{"method":"session/close","params":{"session_ref":"...","idempotency_key":"<key>"}}
```

`media/import` is a local stdio-only helper for a host-selected file. It accepts `session_ref`, a transient `source_path`, and an idempotency key, validates PNG/JPEG/WebP magic bytes, size and dimensions, copies the image into the Session-scoped media directory, and returns only a `MediaRef`. The source path and image bytes are never persisted or returned. `turn/start` and `session/send` load validated references into ephemeral `ContentPart::Image` values; history stores text and stable references only. A route without verified image-input capability returns `unsupported_capability` without contacting a Provider.

`turn/start` returns `{"turn_id":"...","accepted_runtime_settings":{...}}` before its terminal result. A session has at most one running or interaction-waiting turn; another start returns `turn_in_progress`. Interrupt returns `{"accepted":true,"turn_id":"..."}` and the eventual `turn_complete` reports the actual race winner. Close is idempotent, cancels its active turn, and retains the bridge session just long enough to publish cancellation terminal events.

For confirmation interactions, `decision` must be `allow` or `deny`; allow is always translated to `ApprovalScope::Once`. For user-input interactions, `decision` is `respond` and must carry a supplied option id or permitted freeform text. An interaction without any legal response is rejected as `unsupported_capability` and cancelled rather than sent to the host.

## 本机跨会话消息（FEAT-037 P1）

同一 app-server 进程托管的 Session 之间可以按本节契约互发消息。`runtime_settings.session_messaging=true`（会话或 turn 级，默认 `false`，`accepted_runtime_settings` 回显）时，该 Turn 的根 Agent 才暴露 `session_list_peers` 与 `session_send`；子代理不继承。`session_send` 走 `Operation::SessionMessaging`：`read_only` 拒绝，`workspace_write` 每次以 `interaction_request` 确认，`full_access` 允许。

```json
{"method":"session/peers","params":{}}
{"method":"session/message/list","params":{"session_ref":"...","state":"held"}}
{"method":"session/message/respond","params":{"session_ref":"...","message_id":"...","approve":true,"confirmation":true,"idempotency_key":"<key>"}}
{"method":"session/message/configure","params":{"session_ref":"...","inbound":"held|auto|reject","confirmation":true,"idempotency_key":"<key>"}}
```

- 接收方入站模式默认 `held`：消息需宿主经 `session/message/respond` 批准后才投递；`auto` 在接收方空闲时直接投递，但接收方 policy 比发起方更宽时仍强制 `held`；`reject` 拒收。模式只保存在进程内，resume 后恢复为 `held`。
- 消息只在接收方 Turn 结束（空闲）时作为新 Turn 投递，FIFO；投递使用接收方会话默认 `runtime_settings`，正文以 `<session-message from="..." id="..." kind="note">` 包装并声明为不可信数据。
- 事件 `session_message_received`、`session_message_held` 只含 `message_id`、`from_session_id`、`kind`、`state`、`bytes`，不含正文；`session_message_delivered` 携带新 `turn_id`。正文只能经 `session/message/list` 读取，按会话脱敏并截断。
- `respond` 与 `configure` 需要 `confirmation` 与幂等键；非 `held` 消息返回 `message_not_held`，不属于该会话返回 `message_not_found`。消息单条上限 64 KiB、30 分钟过期、每个接收方最多 32 条待处理/待批准。

## Memory management requests

Memory management is a local, explicit host capability. It is scoped through an existing `session_ref`; the server resolves that Session's canonical workspace before reading or mutating the runtime Store. The host cannot supply a workspace path or access another workspace by guessing an ID.

```json
{"method":"memory/list","params":{"session_ref":"...","mode":"research","limit":20}}
{"method":"memory/confirm","params":{"session_ref":"...","candidate_id":"...","body_sha256":"...","idempotency_key":"<key>"}}
{"method":"memory/reject","params":{"session_ref":"...","candidate_id":"...","idempotency_key":"<key>"}}
{"method":"memory/delete","params":{"session_ref":"...","memory_id":"...","idempotency_key":"<key>"}}
```

`memory/list` returns bounded, redacted memory/candidate metadata and bodies for the explicitly requested local management view. Candidate confirmation remains hash-bound; reject and delete are scoped to the Session workspace and update the runtime's existing SQLite/FTS tables. Cross-workspace targets return `ownership_mismatch`, stale or missing targets return `memory_not_found`, and storage failures return `storage_unavailable`. State-changing methods use the same bounded idempotency cache as other local mutations; these methods never accept credential, URL, or arbitrary database parameters.

`runtime_settings.vendor/model/provider_id/auto_provider` 仅选择本机有效目录中的 Vendor、canonical Model 和 Provider，不持久化全局配置。局部设置省略字段沿用宿主会话默认；`provider_id: null` 与省略等价，不能清除既有选择。切回 Auto 必须用 `auto_provider: true`，且不能同时给非空 `provider_id`；显式 ID 则选择该 Provider。宿主转换为 Session 的 `ModelSelection { vendor, model, provider }`，其中 provider wire 为 `{"mode":"auto"}` 或 `{"mode":"explicit","provider_id":"<id>"}`。集成时须验证对应发布版本。Session 在接受 turn 前用唯一 resolver 校验完整选择，不可用返回 `unsupported_setting`，不得回退旧模型或其他 Provider。`collaboration_mode` 仍映射 plan/code 到 plan/coding，`permission_preset` 仍映射 strict/medium/full_control。

FEAT-023 内部子代理：`runtime_settings.subagents_enabled`（bool）与 `max_subagents`（1–32）可在会话或 turn 级设置，省略时沿用本机配置 `defaults.subagents_enabled`/`max_subagents`（配置值超过 32 时按 32 生效）；`accepted_runtime_settings` 回显实际值，越界返回 `unsupported_setting`。关闭时根 Agent 不暴露 `delegate_task`。

`mcp_servers` is an array of enabled local allowlist names and `mcp_toolsets` maps each selected server to exact configured toolset names. They are selection references only: the bridge rejects an unknown/disabled server, a toolset whose server is not selected, or an unknown toolset with `unsupported_capability`. The request schema has no field for MCP URL, transport, header, credential, arbitrary remote tool, Web or Browser. Unsupported `reasoning_effort` and enum values fail with `unsupported_setting`.

## 模型配置 RPC（schema 6）

字段遵循 [Provider 地址契约](provider-endpoint-ownership.md)。配置使用显式 `version = 6`，Model 必须携带逐 ID 的非空、有序 `supported_protocols` 列表，路由使用第一项决定协议；Provider 对象不含 `protocol`；其余仍为 Vendor/Model/Provider 与独立 Credential 后备引用。运行期环境变量只通过 `rivo run --api-key-env <name>` 提供。已支持的旧 Profile/Binding/Endpoint 文件表示由本机加载入口按[持久化文件兼容与自动升级契约](persistence-file-compatibility.md)转换；旧本地 Model 缺少协议集合时不能安全推断，整体拒绝且零写入。迁移后的未知字段或不安全语义整体拒绝；不新增旧 payload、兼容 RPC 或双格式运行路径。握手为 `rivo-app-server-v2`；配置 schema 版本与握手版本独立，宿主须同步更新字段，不以老 payload 偷渡新契约。

下表为模型域目标方法；沿用既有传输授权、请求大小和幂等边界，不因字段变化给 Named Pipe/A2A/LAN 新增配置能力。所有变更请求仍需 `idempotency_key`；表中省略该公共字段。`read/preview/validate/delete_plan` 离线且不落盘，调用配置 RPC 不隐式 refresh。

| 方法 | 请求 | 响应/行为 |
| --- | --- | --- |
| `model_routing/read` | 空对象 | `revision/config/secrets_redacted`；只返回 schema 6，Model 返回有序 `supported_protocols`，Provider 自有 `base_url` 且不含 `protocol`，认证仅引用，发现是受管快照 |
| `model_routing/preview` | `vendor`、可选 `model/provider_id` | 唯一 resolver 的 `vendor/model/provider_id/protocol/base_url/priority`，`protocol` 取所选 Model 的首选协议；无 binding 或请求 ID 重写字段，显式不可用即拒绝 |
| `model_routing/validate` | `revision/config`（Provider 不含 `protocol`） | 验证 schema、目录身份、引用与 URL 等结构，不要求 discovery 可用；返回 `valid/revision` 与受影响更新的确认计划，计划包含 `affected_models`、影响完整性、摘要及 token |
| `model_routing/commit` | `revision/config`（Provider 不含 `protocol`），有受影响更新时必带 `confirmation_token` | 锁内重读、验证计划摘要和 revision 后原子提交；返回新 `revision/config/secrets_redacted`，不得写入客户端伪造的 discovery |
| `model_routing/delete_plan` | `target_kind`、`target_id` | kind 仅 `vendor/model/provider`；返回 revision、目标、`affected_models`、`impact_complete`、`clears_default_route`、`confirmation_token` |
| `model_routing/delete_commit` | `plan`、`confirmation_token` | 重新验证目标、源 revision、派生影响与 token，原子删除；不级联删 Credential |
| `catalog/read` | 空对象 | 有效目录（内置 + 本地）：`catalog_revision`（内置 `defaults.toml` 的 SHA-256，宿主据此判断升级 runtime 后是否刷新）；`vendors[{id, display_name, source}]`；`models[{id, source, builtin, entry, status, replaced_by}]`，`source` 为 `builtin/local/builtin_override`，`status` 为 `active/deprecated/retired`（`retired` 仅出现在本地残留的已下线 ID 上）；`protocols[{id, default_base_url}]`（仅描述性目录信息，不表示 Provider 字段）；`reasoning_efforts`；`warnings[{code, model, replaced_by}]`，code 为 `retired_local_model/default_model_missing/default_model_retired/default_model_deprecated`。只读、不联网、不落盘；生命周期规则见[模型目录生命周期](model-catalog-lifecycle.md) |

`affected_models` 固定为 `{vendor, model}` 列表，Provider 影响来自有效目录与发现完整 ID 的交集；没有 discovery 或无法完整判定影响时 `impact_complete=false`，不能把空列表当零影响。只读返回的 discovery 可回传不变，连接字段变更由服务端标失效；客户端改变发现结果拒绝。validate 生成计划不改变持久状态，也不证明 Provider 可用。草稿不能修改无关非模型设置；完整配置回传时这些字段须保持，revision 仍覆盖全文件。

无效请求、无法安全升级的文件或未来文件格式返回既有 `invalid_request`；不可用的选择返回 `unsupported_setting`；目标新增稳定业务码 `config_conflict`（须重新读取预览）、`confirmation_required`（未确认不提交）。存储不支持的会话 schema 返回 `storage_unavailable`，具体诊断提示受控重置，不隐式 DDL 或清空。原有错误封装、脱敏与 retryable 规则不变。

`health.provider_configuration` 通过 schema 校验与同一 resolver 离线判断 readiness，不再检查 `provider_profile` 是否存在；无默认可用路由不得报 ok。health 不联网、不返回地址/凭据/上游错误正文，也不把结构校验成功当 discovery 成功。

`session/start`、`turn/start` 和 `runtime_settings` 均不得提供任意 URL、协议、认证 header、环境变量值或 Credential 正文。活动 turn 与 retry 保持冻结快照，后续 turn 与恢复重新校验；传输失败不运行中 failover。

`credential/upsert` 仍仅允许既有授权的 Desktop 本机 stdio 子进程；Named Pipe 返回 `credential_transport_forbidden`，A2A/LAN 不得转发。普通 stdio 握手或客户端自报 name 不能代替已有授权。Credential 正文不得进入配置草稿、预览、事件或响应。

## Events

The server emits JSON-RPC notifications only through `event`:

```json
{
  "jsonrpc":"2.0",
  "method":"event",
  "params":{
    "session_ref":"...",
    "turn_id":"...",
    "seq":42,
    "timestamp_utc_ms":1735689600000,
    "type":"message_delta",
    "payload":{}
  }
}
```

`seq` is strictly increasing per live session. Event payloads are bounded and have no arbitrary `serde_json::Value` forwarding:

- `status`: `{"status":"idle|running|waiting_interaction|stopped|error"}`；运行开始的投影可附脱敏 `selection/route`，用于报告本次 turn 的真实路由，而非从上次 Auto 结果猜测。
- `message_start`: `{"message_id":"<turn-stable-id>"}`.
- `user_message` (durable replay only): `{"text":"<bounded-redacted-prompt>","truncated":false,"original_bytes":12}`.
- `message_delta`: `{"message_id":"<turn-stable-id>","delta":{"text":"...","truncated":false,"original_bytes":12}}`.
- `message_end`: `{"message_id":"<turn-stable-id>","content":{"text":"...","truncated":false,"original_bytes":12}}`.
- `tool_call`: `{"tool_call_id":"...","name":"...","status":"requested|running|completed|failed|waiting_interaction","summary":{"truncated":false,"original_bytes":0}}`.失败时附带稳定 `error_code`。每个工具调用必须以 `completed` 或 `failed` 结束，包括审批后执行失败或被重试的调用。确认流程的顺序为 `requested → running → waiting_interaction`，允许后依次发出 `tool_call{status:"running"}` 与 `status{status:"running"}`，再以 `completed|failed` 结束。用户拒绝或 policy 直接禁止时，拒绝作为工具错误交还模型：先发出 `status{status:"running"}`（仅确认拒绝时），再以 `tool_call{status:"failed","error_code":"policy_denied"}` 结束该调用，turn 继续，由模型说明原因或改用允许的方式，通常以 `completed` 结束。取消确认以 `error_code:"cancelled"` 结束该调用并终止 turn。子代理确认被拒绝时只终止该子任务。
- `media_started`: `{"tool_call_id":"...","operation":"generate|edit"}`.
- `media_ready`: `{"tool_call_id":"...","asset":{"asset_id":"...","session_id":"...","media_type":"image/png|image/jpeg|image/webp","width":null,"height":null},"delivery":{"kind":"local_path|signed_url",...}}`; delivery is transient and is never persisted as history.
- `media_failed`: `{"tool_call_id":"...","operation":"generate|edit","message":"<bounded-redacted-message>"}`.
- `interaction_request`: `{"interaction_id":"...","tool_call_id":"...","type":"command_approval|file_change_approval|permission_request|user_input","response":{"decisions":["allow","deny"]},"summary":{"truncated":false,"original_bytes":0}}`.确认类交互的 `tool_call_id` 指向等待审批的工具调用；`user_input` 交互不带该字段。子代理发起的确认另带 `agent_id`、`task_id`，其 `interaction_id` 形如 `{task_id}:{request_id}`，且不会发出 `status:waiting_interaction`（根 turn 继续运行）。
- `subagent`: `{"agent_id":"...","task_id":"...","tool_call_id":"<root delegate_task call>","status":"queued|running|completed|failed|cancelled|interrupted"}`；queued 附有界 `label`，终态附 `truncated`、`usage`、`evidence_count`，从不包含子代理摘要或 handoff。子代理内部工具调用使用 `tool_call`，`tool_call_id` 为 `{task_id}:{call_id}` 并附 `agent_id`、`task_id`；任务结束时仍未终结的子代理工具调用以 `{"status":"failed","error_code":"cancelled"}` 收尾。Turn 结束（含取消、提供方失败）时，bridge 在 `turn_complete` 之前为所有仍未终结的 `tool_call`（包括根 `delegate_task`）补发同样的 `failed`/`cancelled`；补发事件保留该调用此前投影的 `name`、`agent_id` 与 `task_id`。
- `turn_complete`: `{"outcome":"completed|interrupted|failed"}`. There is exactly one terminal event for an accepted turn. `completed` 另附 `usage`：`{"input_tokens":n,"output_tokens":n,"total_tokens":n}`，提供方上报时再附 `reasoning_tokens`、`cache_read_input_tokens`；计数为本 Turn 全部模型调用之和（含工具轮次与被重试的空响应），不含子代理。`interrupted`/`failed` 不保证带 `usage`，宿主须按可选字段处理。
- `error`: `{"code":"<stable-code>","retryable":false,"message":"<redacted-message>"}`.

Session 的成功选择事件为 `ModelSelected { selection, route }`（serde type 为 `model_selected`），Intent 为 `SelectModel`（`select_model`）；不是新增 JSON-RPC method。当前 bridge 不直接转发该事件为同名 notification，而在运行开始的 `status` 中投影实际 `selection/route`；宿主不得等待一个未提供的选择通知。它不改变 `seq` 或 `turn_complete` 的唯一终态规则。旧模型选择事件不解码；新会话恢复规则见 [Session 与交互面](interaction-surface.md)。

## Errors and data boundary

Every error has `code`, `message`, and `retryable`. Stable codes include `invalid_request`, `unsupported_protocol_version`, `unsupported_method`, `unsupported_capability`, `unsupported_setting`, `session_not_found`, `session_resume_unavailable`, `turn_in_progress`, `no_active_turn`, `interaction_not_found`, `interaction_stale`, `runtime_timeout`, `runtime_unavailable`, `storage_unavailable`, `provider_failed`, `tool_failed`, and `internal_error`. Turn-level `error` events use `provider_failed` for model provider/stream failures (including empty or inconsistent model responses after one retry) and `tool_failed` for agent/tool failures; `runtime_unavailable` is reserved for the local runtime itself being unavailable.

Idempotency de-duplicates by `session_ref + method + idempotency_key` in a bounded cache. A repeated identical request returns its original success **or stable error** result; the same key with different request parameters returns `invalid_request`.

bridge 不序列化 Provider 原始帧、凭据正文、header、cookie、环境变量值、完整 prompt、完整 transcript、任意工具参数/输出或本地工作区路径。媒体请求只接受当前 Session 的 `MediaRef`；`media_ready` 的本地路径或短时 URL 仅用于当前事件，不进入历史、日志或 Agent 上下文。公共文本和 policy 摘要在离开进程前，按已解析 Provider 的已知密钥值（包括运行期 `--api-key-env` 来源）脱敏并受大小上限约束；删除 Profile 不得删除此保护。
