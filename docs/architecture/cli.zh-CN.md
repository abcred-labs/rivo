# CLI 契约

`rivo run --prompt <TEXT> [OPTIONS]` 与 `rivo run --stdin [OPTIONS]` 是稳定的
headless 入口；顶层 `rivo --prompt <TEXT>` 与 `rivo --stdin` 等价。无参数只显示帮助，
不再支持位置任务参数。

## 输入与输出

- `--prompt <TEXT>` 提供非空任务，`--stdin` 从标准输入读取非空 UTF-8 任务；两者互斥。
- CLI stdout（包括查询的 `text`、`table`、`json`）使用 UTF-8；不根据 TTY、Windows 代码页或 PowerShell 版本改写字节。Windows PowerShell 5.1 的 native 管道使用 `[Console]::OutputEncoding` 解码，调用方若需将查询接到 `Tee-Object`、重定向或 `ConvertFrom-Json`，须同步将它和 `$OutputEncoding` 设为无 BOM UTF-8；宿主产生的乱码不是 CLI 数据模型的可修复输入。
- 默认 `--format text` 只把模型文本写入 stdout，诊断写入 stderr。
- `--format jsonl` 每行输出一个公共 SessionEvent 投影，字段固定为
  `schema_version`、`session_id`、`turn_id`、`sequence`、`timestamp_utc_ms` 和 `event`。
  工具参数只输出规范 JSON 的 SHA-256，工具正文绝不写入 JSONL。
  正常运行由 Session 写入唯一终态；若已识别 Turn 的事件流异常关闭，CLI 会补发一个
  `stopped` 终态投影，使脚本仍能确定结束。
- 退出码：`0` 完成、`1` 运行失败或取消、`2` 参数/配置/启动错误、`3` 授权拒绝。

普通 headless 对话可以由 Agent 选择 `generate_image` 或 `edit_image`；不新增 `rivo image` 子命令或 slash command。文本模式在 stderr 输出 `media ready` 的本地文件路径或短时地址，stdout 仍只包含最终文本；`--format jsonl` 输出 `media_started`、`media_ready`、`media_failed` 结构化事件。只有 `media_ready` 表示资产已落地。

## 帮助与参数错误

CLI 帮助按命令层级组织：`rivo <command> --help` 只列出该层直接子命令，
`rivo <command> <subcommand> --help` 只说明该操作的位置参数、选项和必要示例。
标题统一使用 `Usage`、`Commands`、`Arguments`、`Options` 和 `Examples`，说明文字以中文为主。
`-h` 与 `--help` 等价，成功返回 `0`、写入 stdout 且不写 stderr。

帮助解析必须先于配置、历史数据库、OS keychain、stdin、网络、TTY 和服务启动等操作，
因此查看帮助不会创建、迁移或修改任何本地状态。缺少参数、未知参数和未知命令返回 `2`，
将对应层级的 `Usage` 写入 stderr。`config` 的命令形状和帮助由 clap 生成；配置字段、
引用完整性、原子写入、Credential、TTY 与模型连接等业务规则仍由既有 handler 校验。

## 语言文件管理

`rivo language path|list|show|validate|install|remove|remote-list|download|update|rollback` 只管理 UI 语言文件，不修改主配置、Session、prompt 或 policy。无子命令时等价于 `list`；只有用户显式执行 `remote-list|download|update` 才访问网络，`rollback` 只读取本地备份。

- `path` 显示 `RIVO_LANGUAGE_DIR`，或默认 `<data_root>/rivo/languages`。
- `list` 同时列出内置语言和用户目录中的 `.ftl`；损坏文件显示 `status` 错误但不阻断其他条目。
- `remote-list --registry <HTTPS_URL>` 读取有界 registry；`download <LOCALE> --registry <HTTPS_URL>` 校验长度、SHA-256、FTL metadata 与变量 schema 后原子安装。
- `update <LOCALE> --yes [--registry <HTTPS_URL>]` 从去敏 receipt 或显式覆盖 URL 查询更新；`rollback <LOCALE> --yes` 不联网，验证并恢复本地 `.bak`。
- `show <locale>` 优先显示用户文件；没有用户覆盖时可显示 bundled `en-US`/`zh-CN`。
- `validate <file>` 只读校验 canonical 文件名、UTF-8、2 MiB 上限、metadata、Fluent 语法、control character 和规范变量 schema。
- `install <file>` 从 metadata 取得 canonical locale 并原子安装。目标不存在时直接安装；覆盖必须显式传 `--yes`，覆盖前保留一个 `.bak`。
- `remove <locale> --yes` 只删除用户层文件。`--yes` 仅授权该次本地删除，不影响工具审批或其他命令。

上述命令的 stdout 不包含语言文件正文；错误不得回显恶意 metadata、URL userinfo、query 或 fragment。启动、resume、TUI reload 和后台任务不得隐式访问语言 registry。

## 模型路由入口（schema 6）

当前配置使用 schema 6；Model 决定协议，Provider 保存地址与认证引用。字段与受控重置见 [Provider 地址契约](provider-endpoint-ownership.zh-CN.md)。

- 模型目录/连接入口为 `config vendor|model|provider list|show|set|remove`；`config model list [--vendor <vendor-id>] [--state <enabled|disabled|all>]` 输出 enabled 状态，目标 Vendor 不存在时拒绝，省略或传入 `all` 输出全部；重复、缺失或非法 `--state` 返回退出码 `2`。`config model enable|disable <model-id>...` 按 ID 批量更新状态。批量输入去重，未知 ID 单独报告而其他有效项照常在一次原子提交中保存，命令仍以 `0` 退出。默认选择由 `config set/unset vendor|model|provider_id` 管理。Provider 新建要求 `--base-url`，认证采用已有 `--credential-name` 引用，另可设 `--enabled/--priority`；Provider 不携带协议，传入 `--protocol` 失败并提示改用 `config model set`。协议由 Model `supported_protocols` 有序列表的第一项决定，`config model set <model-id>` 以 `--enable-protocol`（追加）、`--disable-protocol`（移除，不能移除最后一个）或 `--protocols`（整体替换，顺序即偏好）调整，内置模型写为本地覆盖；更新省略的字段保留原值，不按 URL 复用对象。连接变更仍要求影响确认，并使旧 discovery 失效。
- `config provider set` 默认只保存本地配置并提示显式 refresh；仅传入 Provider 专用 `--refresh` 时才在提交后执行认证 discovery。refresh 失败不回滚已保存配置。
- 双向视图为 `config provider models list <id>` 与 `config model providers <model-id>`；展示 exact ID 派生匹配、可用性及拒绝原因，不能写人工映射。`config provider models remote <id>` 可只读查看远程完整模型目录，不做本地协议匹配且不落盘；`config provider models refresh <id>` 更新受管 discovery，并只持久化模型目录中存在且已启用的 exact canonical ID；发现成功但结果为空仍成功并提示没有发现模型目录中的模型。discovery 先以 Bearer 请求 `/v1/models`，遇 401/403 改用 `x-api-key` 与 `anthropic-version` 重试一次。
- `config route --vendor <id> [--model <id>] [--provider-id <id>]` 离线输出 `vendor/model/provider_id/protocol/base_url/priority`。省略 Provider 使用默认选择，无默认时 Auto；未选 Model 在 enabled Model 中按既有 priority/ID 默认规则。任何显式无效或 disabled Model 值拒绝，不换模型或 Provider。
- turn/TUI 只接受 `--vendor/--model/--provider-id` 或 `--auto-provider` 目录选择；后两者互斥，Auto 显式覆盖默认 Provider。`rivo run --api-key-env <name>` 是唯一的运行时认证引用：只读取本次进程外部注入的变量名，非空值优先，缺失时回退 Provider 的 Credential。移除模型 `--provider-profile`、连接协议 `--provider`、`--base-url`、`--credential-name` 运行覆盖及对应 `RIVO_PROVIDER_PROFILE/RIVO_PROVIDER/RIVO_BASE_URL/RIVO_API_KEY_ENV/RIVO_CREDENTIAL_NAME` fallback；这些旧环境变量继续明确拒绝。
- `rivo run --language <BCP47>` 为本次运行选择 UI/默认回复语言；配置默认值通过 `config set locale <BCP47>` 持久化，省略时为 `en-US`。其他管理命令可在显式 `--language <BCP47>` 下本地化帮助；app-server 使用 `runtime_settings.response_language`，机器协议字段和 error code 不随 locale 改变。
- 模型选择优先级为 CLI > `RIVO_VENDOR/RIVO_MODEL/RIVO_PROVIDER_ID` > 配置默认；`--auto-provider` 显式覆盖环境和配置默认的 Provider。旧模型连接环境变量明确拒绝且错误不回显值。业务 `RIVO_PROFILE` 和 policy 环境变量不受此删除影响。
- 删除模型 `provider-profile/binding/endpoint` 命令、旧默认 profile 字段与模型 `config migrate`，帮助只显示新入口；旧命令/flags 返回 `2`，没有弃用期或兼容 handler。基础模式 `--profile/RIVO_PROFILE`、capability/policy profiles、审批绑定、浏览器 Profile 与非模型 endpoint 保留。
- `list/show/validate/route` 离线不写入；结构 validate 无需 discovery 可用。未刷新、失败、disabled、指纹不符或 ID 不匹配时 route/turn 明确拒绝；运行失败不自动切换。

Provider 新建必须提供地址。地址/协议/认证引用变更在未带确认时打印计划并返回错误；核对派生 `affected_models`（无 discovery 标明未知）、revision 与 token 后，重发相同命令并附 `--confirm <confirmation_token>`。`remove` 则先打印不含 token 的删除计划，并在交互终端以 `[y/N]` 确认；仅 `y`/`yes` 执行，回车或其他输入取消，自动化调用须在同一命令显式传入 `--yes`。删除计划的 revision 与完整性 token 仍由共享提交 API 在进程内校验。CLI 不支持 `--dry-turn` 或 `--revision`；缺确认、冲突或写失败不落盘，冲突后重新预览；删除不级联删 Credential。已支持的 `config.toml`、`credentials.toml` 和 `instances.json` 格式按[持久化文件兼容与自动升级契约](persistence-file-compatibility.zh-CN.md)自动升级；损坏、语义无法保留或未来版本零写入拒绝。新候选仍须验证业务错误退出码与所有失败路径。

## 自动化授权

未显式指定时，stdin/stdout 都是 TTY 的 turn 使用 `prompt`，其他 turn 使用
`deny`；即使显式指定 `--approval-mode prompt`，非 TTY 也会立即拒绝而不会等待 stdin。
自动化写入和进程执行必须传入
`--approval-mode manifest --approval-manifest <path>`；不存在全量批准开关。

清单必须是版本 1 的 JSON，且 `workspace` 的 canonical path 必须与本次 turn 相同：

```json
{
  "version": 1,
  "workspace": "D:/repo",
  "rules": [
    {
      "tool_name": "apply_patch",
      "arguments_sha256": "<canonical-json-sha256>",
      "scope": "once",
      "max_calls": 1
    }
  ]
}
```

每条规则只允许匹配的工具、规范参数哈希和作用域执行至 `max_calls` 次；workspace、版本、工具、参数、作用域或调用次数不匹配时拒绝。CLI 清单仅支持 `scope: "once"`，清单只对当前 turn 有效，不写入配置或会话历史。

## TUI 首启

schema 6 首启仅在缺文件或有效配置无 Provider 时提供新建向导；支持的旧文件先自动迁移，旧本地 Model 缺少 `supported_protocols` 时迁移失败且不能当空配置覆盖。先创建 Provider（Base URL、认证引用），再由用户显式执行 discovery，最后选择 Vendor → canonical Model → Auto/Provider；未刷新不能运行。Provider 可以先本地校验后保存以便修复连接，不把 discovery 成功设为所有配置维护的前置。完整首启草稿取消不保存；已单独确认的 Provider/凭据操作不因随后取消模型选择而暗中回滚。

Credential 正文只由既有专用凭据交互处理，绝不进入配置草稿或回显。`env` 读取引用，`local` 沿用本机独立凭据存储；其认证加密仅避免明文落盘，不等同于 OS keychain 或用户口令保护，CI/团队优先使用 `env`。删除或取消 Provider 草稿不得删除已有 Credential。

`rivo tui` 接受可选的 `--workspace <PATH>`，用于覆盖本次 TUI 会话的工作区；已有配置时
不改写持久化的默认工作区。首次配置时，该路径跳过工作区提问并作为初始默认值随配置
保存。TUI 必须在 stdin 和 stdout 都是交互终端时运行。脚本和 CI 应使用
`rivo run --format jsonl`，而不是尝试启动 TUI。

## Workspace-first 会话管理

本地会话的公共归属模型是 `Workspace -> Session -> Turn -> Message/Event`。Workspace
由规范化后的本地绝对目录唯一确定，但 ID 在首次注册时随机生成并在目录移动后保持不变。
首次从目录启动会自动注册 Workspace；`workspace open` 会幂等注册并将其设为默认，
`--workspace` 只覆盖本次运行。

```text
rivo workspace list
rivo workspace add <PATH> [--name <NAME>]
rivo workspace open <PATH|WORKSPACE_ID>
rivo workspace show <PATH|WORKSPACE_ID>
rivo workspace rename <PATH|WORKSPACE_ID> <NAME>
rivo workspace rebind <WORKSPACE_ID> <PATH>
rivo workspace remove <PATH|WORKSPACE_ID> --yes

rivo sessions list [--workspace <PATH|WORKSPACE_ID>]
rivo sessions list --all-workspaces
rivo sessions list --unscoped
rivo sessions attach <SESSION_ID> <PATH|WORKSPACE_ID>
rivo sessions resume <SESSION_ID> (--prompt <TEXT>|--stdin) --workspace <PATH|WORKSPACE_ID>
rivo sessions backup <BACKUP_PATH>
rivo sessions migrate --yes
rivo sessions migrate --backup <BACKUP_PATH> --yes
```

`sessions list` 默认只读取当前 Workspace，三个作用域选项互斥；`--all-workspaces`
不包含 `unscoped` 旧会话。跨 Workspace resume 在创建 Turn、解析模型和调用工具前失败。
schema 17 到 18 不会随普通启动静默执行。`sessions migrate --yes` 会在数据库同目录
创建不覆盖旧文件的自动备份、完成校验和迁移；也可用显式 `--backup` 指定备份。无法
可靠规范化的旧路径保持 `unscoped`，只能通过 `sessions attach` 显式归属。

### 输入与终端恢复

- 首次 Provider 表单中，Enter 前进到下一项，最后一项才确认；Esc/Ctrl+C 取消。启动选择页保留完整按键修饰符，Ctrl+C 取消启动。
- 终端进入失败必须回滚已完成的阶段。Windows 保存并恢复原始输入/输出模式；只有标准流不可用时才临时绑定控制台句柄，并在关闭替代句柄前恢复原标准句柄。
- TUI 工作区只保留一个输入读取线程。正常退出及错误返回都先关闭有界接收队列、停止并回收线程，再恢复终端模式；读取失败必须报告，不能静默结束。
- Windows 输入回归记录 区分已复现缺陷与尚未复现的全键失效现象，不以普通测试通过替代真实终端证据。

## 历史删除

`rivo sessions delete <id>` 和 `rivo sessions clear` 是不可恢复操作：交互终端会要求
再次输入 `yes`。自动化只能在这一个命令上显式传入 `--yes`，该选项不影响 turn 的工具授权，
也不会持久化。

---

[English](cli.md)
