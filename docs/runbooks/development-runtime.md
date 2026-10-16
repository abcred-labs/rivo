# 开发模式运行手册

本手册面向从本仓库源码运行 Rivo 的开发者和试用者。模型示例按 2026-09-15 的 schema 5 CLI 接口编写；FEAT-029 仍为 `in_progress`，未运行的候选验收不因文档更新而通过，不应视为生产就绪产品。

已打开 TUI Session 可按 F6 或执行 `/settings`。面板默认是 Browse 表单：方向键或
Tab 移动字段，Enter 进入单项编辑，枚举用方向键预览，文本/数字直接输入，多选用
Space 切换 `[x]`/`[ ]`，布尔字段在 Browse 中可用 Space 快速切换；Enter 确认字段，
Esc 撤销字段或关闭面板，Ctrl+Enter 原子应用当前 Session 后续 Turn 的全部设置。
`/model`、`/provider` 无参数会定位到对应字段。已安装二进制的运行说明见[二进制包模式运行手册](binary-runtime.md)。

## 1. 前提与工作目录

需要已安装 Rust 工具链（项目声明的最低版本为 Rust 1.88）。所有 Cargo 命令从仓库的 `runtime/` 目录执行：

```powershell
cd runtime
cargo --version
rustc --version
cargo run -p rivo -- --version
cargo run -p rivo -- --help
```

开发模式中，下文的 `rivo` 命令均应替换为：

```text
cargo run -p rivo --
```

例如：

```powershell
cargo run -p rivo -- config path
```

## 2. 配置、Credential 与优先级

模型选择使用 CLI `--vendor/--model/--provider-id` 或配置默认，`--auto-provider` 可覆盖默认显式 Provider；不依赖模型连接或模型选择环境变量。协议只由所选 Model 的 `supported_protocols` 第一项决定；`base_url` 与认证只从所选 Provider 解析。业务 `--profile/RIVO_PROFILE`、policy 和 workspace 等非模型配置保留原优先级。主配置可用 `RIVO_CONFIG` 指向其他位置；Credential 文件始终与主配置文件同目录。自动化测试可用 `RIVO_CONFIG_CONTENT` 注入整份主配置 TOML 原文，免写临时文件：注入后不读写磁盘配置文件，内容必须显式 `version = 6`，所有配置写入入口返回只读错误并零写入。

主配置必须显式 `version = 6`。每个本地 Model 还必须显式声明非空、有序的 `supported_protocols`（路由使用第一项）；旧 schema 4 本地 Model 缺少该字段时无法安全推断，升级必须零写入拒绝。已支持的历史 `config.toml`、`credentials.toml` 与 `instances.json` 格式必须按[持久化文件兼容与自动升级契约](../architecture/persistence-file-compatibility.md)迁移为当前格式；损坏、无法保留语义或未来版本明确失败且零写入。旧会话 schema 不在该范围内，仍在列明实际路径、表和影响后按[受控重置边界](../architecture/provider-endpoint-ownership.md)处理，保留 Credential、记忆、MCP、A2A 信任及其他非会话数据，不删除整个目录或数据库。FEAT-029 当前为 `in_progress`，现有二进制在迁移器交付前可能继续拒绝历史配置。

以下命令是路径的权威来源：

```powershell
cargo run -p rivo -- config path
cargo run -p rivo -- config credentials path
```

Windows 默认路径通常为：

```text
C:\Users\<user>\AppData\Roaming\rivo\config.toml
C:\Users\<user>\AppData\Roaming\rivo\credentials.toml
```

Credential 文件使用本地混淆加密保存 API Key，但不是系统密钥环或用户口令保护。不要把 API Key 写进 `config.toml`、命令行、提交的文件或日志。

中心监督服务及运行时接线已移除，`rivo supervision` 与 `[supervision]` 不再是受支持接口；不要按旧文档执行注册或添加监督配置。FEAT-024 仅保留未来设计参考，不承诺 V2 实现。本机/局域网 instance-control、A2A 和本机 app-server 继续保留。

已有配置若包含 `[supervision]`，即使 `enabled = false` 也会按未知字段拒绝。请先备份主配置，再手动移除该配置段并运行 `config validate`；无需重置其他配置或会话数据。本次代码清理不会访问或删除旧监督 keychain 身份及 outbox 文件。

创建一个可复用 Credential（名称仅可含 ASCII 字母、数字、`_`、`-`）：

```powershell
cargo run -p rivo -- config credentials set <credential-name>
cargo run -p rivo -- config credentials show
```

`set` 会要求输入两次 API Key，并以 `*` 遮罩输入内容。日常管理命令：

```powershell
cargo run -p rivo -- config credentials migrate
cargo run -p rivo -- config credentials unset <credential-name>
```

`credentials show` 只列出 Credential 名称和保存状态，不会显示 API Key 正文。`unset` 会删除指定名称的本地 Credential；删除前请确认没有仍在引用它的 Provider。删除 Provider 不删除 Credential；`credentials migrate` 仅管理独立凭据的加密格式，不是模型配置或旧会话迁移入口。

Provider 只保存可选 `auth.credential` 后备引用。配置时使用 `--credential-name <credential-name>`；运行时以 `rivo run --api-key-env <env-name>` 读取外部注入的变量，非空值优先，缺失或空白时回退 Credential。环境变量名和实际值均不写入普通配置、命令历史或脚本。

检查解析后的安全配置和配置有效性：

```powershell
cargo run -p rivo -- config show
cargo run -p rivo -- config validate
```

`config show` 不会显示 API Key 正文。`config set/unset` 的模型默认键为 `vendor`、`model`、`provider_id`；业务 `profile`、`policy`、`workspace`、预算和日志字段保持独立。连接只用 `config provider set` 管理，不能写回旧顶层连接字段。`config validate` 只验证结构，不证明 discovery 或远程可用。

### Workspace 与本地会话

本地 Session 按 Workspace 隔离。首次从一个有效目录启动时会自动注册 Workspace；也可以
使用 `rivo workspace add/open` 管理目录，`open` 会同时更新默认 Workspace。会话列表默认
只显示当前 Workspace：

```powershell
rivo workspace list
rivo workspace open <PATH|WORKSPACE_ID>
rivo sessions list --workspace <PATH|WORKSPACE_ID>
rivo sessions list --all-workspaces
rivo sessions list --unscoped
rivo sessions attach <SESSION_ID> <PATH|WORKSPACE_ID>
```

恢复必须在所属 Workspace 中进行；跨 Workspace 请求会在模型和工具执行前拒绝。
schema 17 到 18 的归属迁移不会随普通启动静默执行。推荐使用一条命令自动备份并迁移：

```powershell
rivo sessions migrate --yes
```

如需自行指定备份位置，仍可使用：

```powershell
rivo sessions backup <BACKUP_PATH>
rivo sessions migrate --backup <BACKUP_PATH> --yes
```

迁移无法确定归属的旧 Session 保持 `unscoped`，不会自动归入当前目录。`workspace remove`
要求 `--yes`，会级联删除该 Workspace 的会话数据和本地会话媒体索引。

## 3. 配置 LLM Provider

模型域为 Vendor、canonical Model、Provider 和独立 Credential。Provider 自有地址与认证引用、不携带协议；Model 决定协议，不保存连接或人工映射；完整 canonical ID 与认证 discovery 区分大小写、前后缀精确比较。相同 URL 不合并 Provider，一个 Provider 可以同时服务多个 Vendor、不同协议的模型，不必为每个模型或协议复制连接。

### OpenAI 快速配置

以下命令从 `runtime/` 执行。先查看有效目录，只有目标不存在时才执行 Vendor/Model `set`；内置模型只可经 `config model set` 写入协议等本地覆盖。所有 `<...>` 必须替换；示例 Provider ID 应为新建对象。Credential 只通过隐藏交互输入，后续仅引用名称。

```powershell
cargo run -p rivo -- config credentials set openai-api-key
cargo run -p rivo -- config vendor list
cargo run -p rivo -- config model list
cargo run -p rivo -- config model list --vendor <vendor> --state enabled --format table
cargo run -p rivo -- config model disable <canonical-model-id> [<canonical-model-id>...]
cargo run -p rivo -- config model enable <canonical-model-id> [<canonical-model-id>...]
cargo run -p rivo -- config vendor set <vendor> --display-name <vendor-display-name>
cargo run -p rivo -- config model set <canonical-model-id> --vendor <vendor> --display-name <model-display-name> --protocols openai_responses,openai_chat_completions --priority 100

cargo run -p rivo -- config provider set openai `
  --base-url https://api.openai.com/v1 --credential-name openai-api-key

cargo run -p rivo -- config validate
cargo run -p rivo -- config provider models refresh openai
cargo run -p rivo -- config model providers <canonical-model-id>
cargo run -p rivo -- config route --vendor <vendor> --model <canonical-model-id> --provider-id openai
cargo run -p rivo -- config set vendor <vendor>
cargo run -p rivo -- config set model <canonical-model-id>
cargo run -p rivo -- config set provider_id openai
```

- `config provider set` 仅配置连接（`--base-url`、`--credential-name`、`--enabled`、`--priority`），不接受 `--protocol`，传入时失败并提示改用 `config model set`；`--credential-name` 必须引用已有 Credential，更新省略字段保留原值。连接变更先显示 `affected_models/impact_complete` 等计划，核对后以同样参数加 `--confirm <confirmation_token>` 提交；过时 revision/token 拒绝。默认保存后提示显式 refresh，追加 Provider 专用 `--refresh` 才会在提交后立即执行 discovery，失败不回滚配置。`config provider|model|vendor remove <id>` 显示不含 token 的影响计划后以 `[y/N]` 确认，自动化调用使用 `--yes`。URL/认证引用变化撤销旧 discovery 可用性。
- `config unset provider_id` 回到默认 Auto。Auto 按 Provider priority 降序、ID 升序选择；显式 Provider 不可用直接失败，不切换 Provider 或模型。Model 未指定时按其 priority 降序、ID 升序选定，不为可用性换模型。
- `config model list --vendor <vendor> --state <enabled|disabled|all>` 仅列出该已存在 Vendor 的对应状态 Model，并在所有输出格式中显示 `enabled`；省略 `--state` 等同 `all`。`config model disable|enable <model-id>...` 以 canonical Model ID 批量管理状态；禁用 Model 不参与默认选择，显式或默认选择它直接失败。未知 ID 只报告 `missing`，不阻断其他 ID 的单次原子保存，命令仍以 `0` 退出。
- `list/show/validate/route` 离线只读；配置维护不要求 discovery 成功，`validate` 不证明远程权限。
- 只有显式 `provider models refresh` 访问认证目录。未刷新、最近刷新失败、unsupported、disabled、指纹不符或缺少 exact ID 均不可运行；目录 404 不能绕过。不得手填成功缓存，不新增墙钟 TTL。

快速配置后的常用核验与排障命令：

```powershell
cargo run -p rivo -- config credentials show
cargo run -p rivo -- config provider list
cargo run -p rivo -- config provider show openai
cargo run -p rivo -- config provider models list openai
cargo run -p rivo -- run --vendor <vendor> --model <canonical-model-id> --provider-id openai --trace --prompt "回复：ok"
```

最后一条发送最小真实请求，按需执行。查询输出为 Provider 的 `base_url/discovery`，不再有 profile 的 `request_url` 投影。失败摘要须脱敏；修复连接后重新 refresh，再用 `config route` 预检。

### 模型协议与请求路径

协议由 Model 的 `supported_protocols` 第一项决定。内置 OpenAI 多协议模型（如 `gpt-5`、`gpt-5.1`、`gpt-5.2`）优先 `openai_responses`，其次 `openai_chat_completions`；DeepSeek/GLM 多协议模型优先 `openai_chat_completions`；Anthropic 模型仅 `claude_messages`；`gpt-5.3-codex`、`gpt-6-sol`、`gpt-6-luna`、`gpt-6-astra` 仅 Responses。推荐 Base URL 以 `/v1` 结尾：

| 模型首选协议 | 鉴权 | 请求地址 |
| --- | --- | --- |
| `openai_chat_completions` | `Authorization: Bearer` | `<base-url>/chat/completions` |
| `openai_responses` | `Authorization: Bearer` | `<base-url>/responses` |
| `claude_messages` | `x-api-key` 和 `anthropic-version: 2023-06-01` | Base URL 以 `/v1` 结尾时为 `<base-url>/messages`，否则为 `<base-url>/v1/messages` |

目录 discovery 先以 Bearer 请求 `/v1/models`，遇 HTTP 401/403 时改用 `x-api-key` 与 `anthropic-version` 重试一次。一个网关只需一个 Provider，例如 Base URL 为 `https://host/v1` 的网关可同时以 Responses 服务 `gpt-6-sol`、以 Messages 服务 `claude-sonnet-5-5`：

```powershell
cargo run -p rivo -- config provider set chat-gateway `
  --base-url <base-url> `
  --credential-name <credential-name>
```

调整模型协议（内置模型同样适用，写为本地覆盖；省略 `--vendor/--display-name` 时沿用内置条目）：

```powershell
# 追加协议
cargo run -p rivo -- config model set <model-id> --enable-protocol <protocol>[,<protocol>]
# 移除协议；不能移除最后一个
cargo run -p rivo -- config model set <model-id> --disable-protocol <protocol>[,<protocol>]
# 整体替换；顺序即偏好。例如网关只实现 Chat Completions 时：
cargo run -p rivo -- config model set gpt-5 --protocols openai_chat_completions
```


以 Chat Provider 为例刷新并选择默认连接（默认 Vendor/Model 须已设置）：

```powershell
cargo run -p rivo -- config provider list
cargo run -p rivo -- config provider show chat-gateway
cargo run -p rivo -- config provider models refresh chat-gateway
cargo run -p rivo -- config provider models list chat-gateway
cargo run -p rivo -- config set provider_id chat-gateway
```

自建网关的 Base URL 是否需要包含 `/v1` 取决于其 API 文档。例如，若完整接口是 `<host>/v1/chat/completions`，则 Base URL 应为 `<host>/v1`。一个网关也不一定同时兼容三种标准协议。

## 4. 运行任务与 TUI

设置默认 Vendor/Model/Provider 并成功发现后，运行最小 headless 请求：

```powershell
cargo run -p rivo -- --prompt "用一句话介绍当前项目"
```

也可显式选择路由并打印追踪摘要：

```powershell
cargo run -p rivo -- `
  --vendor <vendor> --model <canonical-model-id> --provider-id chat-gateway `
  --trace `
  "用一句话介绍当前项目"
```

常用运行参数：

```powershell
cargo run -p rivo -- `
  --vendor <vendor> --model <canonical-model-id> --provider-id chat-gateway `
  --profile general `
  --policy medium `
  --workspace <workspace-path> `
  --no-memory-recall `
  "任务"
```

基础模式为 `general`、`research`、`plan`、`coding`、`review`；MCP 是每次 turn 的附加能力，不是基础 profile。通常由用户显式选择；只有明确提出查库、查数据源、核验、纠偏或补充事实时，才会为该 turn 补全已配置的 `read_only` toolset。安全档位为 `strict`、`medium`、`full_control`，默认分别是 `general` 与 `medium`；`workspace` 仅是旧配置兼容别名。`medium` 允许任意本机只读，工作区内变更和进程执行仍需确认。headless 模式遇到需要确认的操作时会自动拒绝；需要交互确认时启动 TUI：

```powershell
cargo run -p rivo -- tui
```

TUI 启动页依次选择 Vendor、Model、Auto 或显式 Provider。工作台 `:provider` 查看连接，`:provider <provider-id>` 显式选择，`:provider auto` 回到 Auto，`/model <canonical-model-id>` 选择目录模型并重新校验当前 Provider 选择。入口提交 `SelectModel { selection }`，成功事件为 `ModelSelected { selection, route }`；活动 turn/retry 保持快照，后续 turn 与恢复重新解析。取消草稿不保存，失败不回退其他模型或 Provider。

使用 `/exit`（或 `/quit`）退出 TUI；活动 turn 中执行退出命令会先取消，收到终态后再次执行。`/cancel` 或 `Ctrl+C` 只取消活动 turn，空闲时 `Ctrl+C` 仍可直接退出。

Web 和 MCP 是按 turn 启用的独立能力：`--network` 仅为当前 turn 启用 Web，`--mcp-server <name>` 和 `--mcp-toolset <server>/<toolset>` 选择已配置 allowlist。使用 `rivo mcp install http|stdio` 安装并发现 MCP，且只能以环境变量引用 HTTP header；本手册不展开其配置，详见根 README 和[工具运行时文档](../architecture/tool-runtime.md)。

## 5. 会话、诊断与数据

会话历史、日志和诊断信息属于本地敏感数据。常用命令：

```powershell
cargo run -p rivo -- sessions path
cargo run -p rivo -- sessions backup <backup-path>
cargo run -p rivo -- sessions reset --backup <backup-path> --yes
cargo run -p rivo -- sessions list
cargo run -p rivo -- sessions show <session-id>
cargo run -p rivo -- sessions resume <session-id> --prompt "后续任务"
cargo run -p rivo -- sessions delete <session-id> [--yes]
cargo run -p rivo -- sessions clear [--yes]
cargo run -p rivo -- diagnostics create <destination-path> --yes
```

诊断包包含经脱敏的用户会话、工具调用/结果、启动参数、有效配置、SQLite health 和错误上下文；完整 system prompt、持久指令正文和密钥不会写入数据库、日志或诊断包。schema 不兼容时 journal 会省略，但 health 与日志仍可导出。`sessions reset` 只接受 schema 14，必须使用已校验的备份并显式传入 `--yes`；它不会自动执行，也不会删除媒体、配置或备份。请将导出包与备份视为高敏感文件。

## 6. 验证与故障排查

先验证配置，再发送最小请求：

```powershell
cargo run -p rivo -- config validate
cargo run -p rivo -- config provider show <provider-id>
cargo run -p rivo -- config route --vendor <vendor> --model <canonical-model-id> --provider-id <provider-id>
cargo run -p rivo -- run --vendor <vendor> --model <canonical-model-id> --provider-id <provider-id> --trace --prompt "回复：ok"
```

- **404**：核对 provider 类型和 Base URL 是否重复或遗漏 `/v1`，并确认完整请求路径与服务文档一致。
- **401/403**：核对本次 `--api-key-env` 的值或 Provider 的 `auth.credential` 后备引用；使用 Credential 时运行 `config credentials show` 核对名称，并确认权限。
- **模型不支持或 400**：核对 canonical Model 完整 ID、Vendor 归属和 exact discovery，并确认网关支持所选协议与工具调用格式。

开发质量检查：

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
cargo xtask check
```

普通测试不会访问真实 LLM 服务。覆盖率检查需要安装 `cargo-llvm-cov`：

```powershell
cargo install cargo-llvm-cov --locked
cargo xtask coverage
```

本地预览验收前从仓库根目录的 `live-test.example.toml` 复制本地配置，为三个 target（Chat Completions 一个、Responses 两个、Messages 两个）设置模型、Base URL 和 Credential 引用或 `api_key_env`，再执行：

```powershell
cargo xtask release-test --live-config ../live-test.toml --report-dir ../artifacts/test-reports
```


历史报告只证明原候选，不用于 schema 5 新候选，也不得改写报告的提交或结果。完整流程见 [源码配置与测试指南](../../SOURCE_CONFIGURATION_AND_TESTING.md)；本轮文档清理不等于已运行 Rust、ConPTY、三协议三个 target 或 A2A 验收。
