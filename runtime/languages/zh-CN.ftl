### rivo-language-schema: 1
### locale: zh-CN
### display-name: 简体中文
### revision: 2026-09-11.1
### min-rivo-version: 0.1.0

common-ok = 确定
common-cancel = 取消
common-back = 返回
common-reload = 重新加载
language-current = 当前语言：{ $locale }
language-reloaded = 已重新加载语言：{ $locale }
error-language-invalid = 不支持的语言：{ $locale }
error-language-file-invalid = 语言文件无效：{ $reason }
error-language-message-missing = 缺少消息：{ $message }
error-language-reload-failed = 无法重新加载语言：{ $reason }
phase-idle = 空闲
phase-running = 运行中
phase-waiting = 待确认
phase-cancelling = 取消中
phase-finished = 完成
phase-error = 异常
settings-title = Session 设置
approval-title = 需要确认
help-title = 帮助
startup-title = Rivo · 启动配置
startup-tagline = 让智能体在你的终端里工作 · 选择配置后按 Enter 开始
startup-selection-help = Vendor → Model → Auto / 显式 Provider；Enter/Space 切换，←/→ 反向/正向选择。
startup-capability = 能力模式
startup-policy = 安全策略
startup-vendor = Vendor
startup-model = Model
startup-provider-auto = Auto
startup-provider-explicit = 显式 { $provider }
startup-unselected = 未选择
startup-model-default = 自动（目录默认）
startup-provider = Provider 选择
startup-network = 网络
startup-browser = 浏览器
startup-memory = 记忆召回
startup-trace = Trace
startup-mcp = MCP
startup-start = 开始
startup-route-preview = 路由预览：{ $vendor } / { $model } → { $provider }（{ $protocol }，priority={ $priority }）
startup-route-unresolved = 路由预览：未解析；不能开始
startup-notice = 提示
sidebar-title = ● 状态
drawer-title = ● 活动 / 错误 · Esc 返回
input-placeholder = › 输入任务或 /help
input-title = › 输入 · Enter 提交 · Ctrl+J 换行
command-palette-title = ✻ 命令面板
settings-scope = 当前修改只影响本 Session 的后续 Turn。
settings-status = 状态
settings-hint = 提示
workspace-empty = 暂无已注册 Workspace
help-keyhint = ↑/↓ 滚动 · Esc 关闭帮助
palette-keyhint = ↑/↓ 选择 · Tab 补全 · Enter 执行 · Esc 关闭
settings-keyhint = 方向键/Tab 浏览 · Enter 编辑 · Space 切换 · Ctrl+Enter 应用 · Esc 撤销
workspace-keyhint = ↑/↓ 选择 · Enter 打开 · Esc 关闭
confirm-keyhint = y 接受 · n 拒绝 · Enter 确认 · Esc 关闭
interaction-keyhint = ↑/↓ 选择 · 1-9 快捷选择 · Enter 提交
diagnostic-keyhint = r 重试 · Enter/Esc 返回
default-keyhint = / 命令 · F6 设置 · Tab 切换焦点 · ↑/↓ 滚动 · ? 帮助 · Esc 清空
notice-plan-approved = 任务计划已批准
notice-plan-rejected = 任务计划已拒绝
notice-plan-started = 任务计划开始执行
notice-step-started = 计划步骤开始
notice-step-completed = 计划步骤完成
notice-step-blocked = 计划步骤阻塞
notice-plan-completed = 任务计划已完成
notice-goal-completed = Goal 已完成
notice-goal-blocked = Goal 已阻塞
notice-tool-completed = 工具完成
notice-media-ready = 媒体已就绪
notice-media-failed = 媒体处理失败
notice-context-compressed = 上下文已压缩，省略消息
notice-subagent-queued = 子代理已排队
notice-subagent-completed = 子代理完成
notice-confirmation-required = 等待确认
notice-interaction-required = 需要交互
notice-memory-saved = 已保存记忆
notice-memory-rejected = 已拒绝记忆候选
notice-memory-deleted = 已删除记忆
notice-retry-available = 可重试步骤
help-body = /exit /cancel /retry /sessions /new /errors /permissions /language /settings /provider /model /network /browser /mcp /a2a /subagents /trace /memory
language-switched = 已切换 UI catalog
error-language-runtime-missing = 当前命令上下文没有语言 runtime
error-language-switch-failed = 语言切换失败，继续使用旧 catalog
notice-plan-updated = 任务计划已更新
notice-goal-updated = Goal 已更新
notice-goal-continuation = Goal 将继续执行
notice-goal-audit = Goal 审计
goal-audit-complete = 已满足
goal-audit-continue = 继续
notice-tool-requested = 工具请求
notice-media-started = 媒体处理中：
notice-subagent-confirmation = 子代理请求确认
notice-subagent-started = 子代理开始执行
notice-model-selected = 模型选择
notice-prompt-prepared = 提示词已准备
notice-session-settings-applied = Session 设置已应用；下一 Turn 将使用新参数
tool-summary-awaiting-policy = 等待策略评估
tool-summary-needs-confirmation = 需要用户确认
notice-memory-candidate = 记忆候选
notice-memory-recalled = 召回记忆
notice-memory-injected = 注入记忆
memory-candidate-actions = 按 y 保存或 n 拒绝
diagnostic-recovery = 查看诊断详情后重试或新建会话
error-workspace-switch-blocked = 当前存在待确认交互，结束后才能切换 Workspace
error-settings-blocked = 当前存在待确认交互，结束后才能打开 Session 设置
error-route-unresolved = 当前 Session 尚未解析模型路由
confirmation-no-side-effects = 无额外副作用
confirmation-actions-session = y 允许一次 · a 本会话允许 · n/Esc 拒绝
confirmation-actions-once = y 允许一次 · n/Esc 拒绝
memory-candidate-title = 记忆候选
label-source-agent = 来源
label-task = 任务
label-tool = 工具
label-summary = 摘要
label-side-effects = 副作用
label-candidate = 候选
label-summary-hash = 摘要哈希
error-terminal-too-small = 终端尺寸不足：
label-minimum = 最小尺寸
terminal-resize-hint = 请放大终端后继续。
label-errors = 错误
settings-mode-browse = 浏览模式 · Enter 编辑 · Space 切换
settings-mode-edit-field = 单项编辑 · 方向键预览 · Enter 确认 · Esc 撤销
settings-mode-edit-text = 文本编辑 · 方向键移动光标 · Enter 确认 · Esc 撤销
settings-mode-multiselect = 多选编辑 · Space 切换 · Enter 确认 · Esc 撤销
workspace-title = ✻ Workspace
diagnostic-title = 错误详情
label-stage = 阶段
label-error-code = 错误码
label-retryable = 可重试
label-recovery = 恢复
label-message = 消息
label-full-cause = 完整原因
label-freeform = 自由输入
interaction-detail-keyhint = ↑/↓ 选择 · 1-9 快捷选择 · Enter 提交 · Esc 需要明确响应
interaction-title = ✻ Interaction · 需要响应
label-budget = 预算
label-steps = 步骤
label-none = 无
label-targets = 目标
label-program = 命令
label-capability = 能力
label-policy = Policy
label-constraints = 约束
label-read = 读取
label-write = 写入
label-network = 网络
label-timeout = 超时
label-output-limit = 输出上限
label-resolved-provider = 已解析 Provider
label-persistent-instructions = 持久指令
label-truncated = （已截断）
sidebar-session = ── 会话 ──
sidebar-user-selection = 用户选择
label-unselected = 未选择
sidebar-resolved-route = 已确认路由
sidebar-route-pending = 未确认（等待 Session 路由）
label-model = 模型
label-runtime-status = 运行状态
sidebar-policy = ── 策略 ──
label-capability-mode = 能力模式
label-safety-policy = 安全策略
sidebar-runtime = ── 运行时 ──
label-browser = 浏览器
label-off = 关闭
label-memory = 记忆
sidebar-budget = ── 预算 ──
conversation-collapsed = … 已折叠 · Enter 展开
settings-group-routing = 模型路由
settings-group-runtime = 运行时
settings-group-context = 上下文
settings-group-agents = Agent
settings-edit-number = 输入数字后按 Enter 确认
settings-no-options = 暂无可选项；可先通过对应命令建立配置
cli-language-help = 管理本地 UI 语言文件。

    用法：rivo language <COMMAND>

    命令：path、list、show、validate、install、remove、remote-list、download、update、rollback

    选项：-h、--help  显示帮助
cli-sessions-help = 管理本地 Workspace 会话历史。

    用法：rivo sessions [COMMAND]

    命令：path、backup、reset、migrate、list、show、resume、attach、delete、clear

    选项：-h、--help  显示帮助
cli-workspace-help = 管理本地 Workspace。

    用法：rivo workspace list|add|open|show|rename|rebind|remove

    选项：-h、--help  显示帮助
cli-diagnostics-help = 创建脱敏诊断包。

    用法：rivo diagnostics create <PATH> [--yes]

    选项：--yes 跳过确认；-h、--help 显示帮助
cli-instance-help = 查看本机 Rivo 实例。

    用法：rivo instance [COMMAND]

    命令：list、endpoint、skill

    选项：-h、--help 显示帮助
cli-mcp-help = 管理已配置的 MCP server。

    用法：rivo mcp <COMMAND>

    命令：list、show、inspect、login、install、enable、disable、remove

    选项：-h、--help 显示帮助
cli-a2a-help = 管理 A2A peer、委派和本机 A2A server。

    用法：rivo a2a <COMMAND>

    命令：discover、pair、peers、inspect、revoke、delegate、control、cancel、recover、serve、pairings、approvals、provision

    选项：-h、--help  显示帮助
cli-a2a-discover-help = 发现局域网 A2A 候选。

    用法：rivo a2a discover [--timeout-seconds <1..=300>]
cli-a2a-pair-help = 配对 A2A peer。

    用法：rivo a2a pair <LOCAL_IDENTITY_LABEL> --endpoint <HTTPS_URL> --spki-sha256 <SHA256> [--transport <mtls|psk>] [--secret-stdin]
cli-a2a-peers-help = 列出已配对的 A2A peer。

    用法：rivo a2a peers
cli-a2a-inspect-help = 显示 A2A peer 详情。

    用法：rivo a2a inspect <PEER_ID>
cli-a2a-revoke-help = 撤销 A2A peer。

    用法：rivo a2a revoke <PEER_ID> [--yes]
cli-a2a-delegate-help = 向 A2A peer 委派任务；正文只从 stdin 读取。

    用法：rivo a2a delegate <PEER_ID> <SKILL_ID> --message-stdin --yes [--stream] [--transport psk --secret-stdin]
cli-a2a-control-help = 调用 peer 上已批准的实例控制操作。

    用法：rivo a2a control <PEER_ID> <TARGET_INSTANCE_ID> <METHOD> --params-stdin [--yes]
cli-a2a-cancel-help = 取消远端 A2A 任务。

    用法：rivo a2a cancel <PEER_ID> <TASK_ID> --yes [--transport psk --secret-stdin]
cli-a2a-recover-help = 恢复远端 A2A 任务。

    用法：rivo a2a recover <PEER_ID> <TASK_ID> [--transport psk --secret-stdin]
cli-a2a-serve-help = 启动本机 A2A server。

    用法：rivo a2a serve [<SERVER_PROFILE>] --config <OWNER_A2A_SERVER_TOML>
cli-a2a-pairings-help = 管理本机 A2A 配对请求。

    用法：rivo a2a pairings list --config <OWNER_A2A_SERVER_TOML>
    rivo a2a pairings <approve|reject> <REQUEST_ID> --config <OWNER_A2A_SERVER_TOML>
cli-a2a-approvals-help = 管理本机 A2A 审批。

    用法：rivo a2a approvals list --config <OWNER_A2A_SERVER_TOML>
    rivo a2a approvals <approve|reject> <APPROVAL_ID> --config <OWNER_A2A_SERVER_TOML>
cli-a2a-provision-help = 管理本机 A2A 测试身份材料。

    用法：rivo a2a provision --yes --bind <PRIVATE_IP:PORT> --server-identity-ref <REF> --issuer-ref <REF>
    rivo a2a provision <check|cleanup> --server-identity-ref <REF> --issuer-ref <REF> [--yes]
cli-a2a-no-pending-approvals = 没有待本机裁决的 A2A 审批请求
cli-a2a-test-identities-cleaned = 测试 A2A identity 已清理
cli-a2a-peer-revoked = 已撤销 {$id}
cli-a2a-peer-grant-revoked = peer grant {$id} 已撤销
cli-a2a-approval-decided = approval {$id} 已{$operation}一次
cli-a2a-storage-worker-interrupted = A2A 本地存储 worker 已中断
cli-a2a-provision-usage = 用法：rivo a2a provision --yes --bind <private-ip:port> --server-identity-ref a2a-test-... --issuer-ref a2a-test-...
cli-a2a-server-identity-missing = 缺少 --server-identity-ref
cli-a2a-issuer-missing = 缺少 --issuer-ref
cli-a2a-provision-confirm-required = provision 需要显式 --yes
cli-a2a-test-identity-required = provision 只允许 a2a-test- 前缀 identity
cli-a2a-bind-format-invalid = bind 必须为 IP:port
cli-a2a-bind-address-invalid = bind 必须为 RFC1918/ULA 地址；仅 RIVO_A2A_LOCAL_TEST=1 可使用 loopback
cli-a2a-test-identity-exists = 测试 identity 已存在；为避免覆盖，请换用新的引用
cli-a2a-spki-compute-failed = 无法从生成的 Server 证书计算 SPKI
cli-a2a-server-keychain-write-failed = 无法写入 Server TLS bundle 到 OS keychain
cli-a2a-client-ca-write-failed = 无法写入 profile Client CA 到 OS keychain
cli-a2a-cleanup-confirm-required = provision cleanup 需要显式 --yes
cli-a2a-flag-missing = 缺少 {$flag}
cli-a2a-cleanup-identity-required = cleanup 只允许 a2a-test- 前缀 identity
cli-a2a-check-identity-required = check 只允许 a2a-test- 前缀 identity
cli-a2a-server-keychain-read-failed = 无法读取 Server v2 keychain 条目
cli-a2a-client-ca-read-failed = 无法读取 Client CA v2 keychain 条目
cli-a2a-spki-parse-failed = Server 证书 SPKI 无法解析
cli-a2a-inspect-usage = 用法：rivo a2a inspect <peer-id>
cli-a2a-peer-not-found = A2A peer 不存在
cli-a2a-revoke-usage = 用法：rivo a2a revoke <peer-id> [--yes]
cli-a2a-peer-not-paired = A2A peer 未配对或不存在
cli-a2a-identity-missing = A2A peer 缺少本机 mTLS 身份
cli-a2a-task-not-found = 本机没有该 task 的持久化映射
cli-a2a-task-peer-mismatch = A2A task 映射 peer 不匹配
cli-a2a-transport-mismatch = A2A peer transport 与请求不匹配
cli-a2a-top-usage = 用法：rivo a2a <COMMAND>
cli-a2a-discovery-interrupted = A2A discovery task 已中断
cli-a2a-empty-message = A2A 委派标准输入不能为空
cli-a2a-control-json-invalid = A2A control 的标准输入必须是 JSON object
cli-a2a-control-json-object = A2A control 参数必须是 JSON object
cli-a2a-recover-failed = A2A task 恢复失败；未重发业务委派
notice-locale-pending-turn = 当前 Turn 结束或 retry 完成后可应用
notice-settings-applying = 正在应用 Session 设置
notice-cancel-in-progress = 正在取消当前 Turn；再次按 q 退出
notice-history-delete-cancelled = 已取消历史删除确认
error-no-retry-step = 当前错误没有可重试步骤
error-history-expired-resume = 所选历史会话已过期，无法恢复；请新建会话
error-history-expired-delete = 所选历史会话已过期，无法删除；请使用 CLI 查看留存状态
notice-session-switched = 已切换会话 {$id}
notice-diagnostics-cleared = 已清空本机诊断记录
error-diagnostics-clear = 清空诊断失败：{$error}
notice-no-persistent-grants = 当前会话没有持久授权
notice-persistent-history-disabled = 当前会话未启用持久历史
notice-turn-cancel-first = 当前 Turn 尚未结束；请先 /cancel 后再继续
notice-workspace-empty = 暂无 Workspace
error-workspace-read = 读取 Workspace 失败：{$error}
notice-session-not-ready = 会话尚未就绪
notice-model-required = 请先选择模型
error-model-invalid = 模型不在有效目录中
notice-no-retry = 没有可重试步骤
notice-mcp-disabled = 当前 Turn MCP 已关闭
notice-a2a-cleared = 当前 Turn A2A peer 已清空
notice-memory-next-turn = 记忆候选通过事件确认；召回仅作用于下一 Turn
error-subagents-positive = 最大并发数必须是大于 0 的整数
notice-unknown-command-help = 未知命令，使用 /help 查看帮助
notice-interaction-select-or-input = 请先选择选项或输入内容
notice-interaction-response-required = Interaction 仍需明确响应
notice-session-approval-unsupported = 当前确认不支持本会话授权；请使用 y 或 n
notice-routing-draft-incompatible = 模型变化使 Provider 或 reasoning 不兼容，已仅更新本地草稿
notice-language-catalog-status = 当前 UI catalog={$current}；可选：{$available}
notice-tui-cancel-exit = 正在取消当前 Turn，请收到终态后再次执行 /exit 退出
notice-error-filter = 错误筛选：{$filter}
notice-persistent-grants = 当前会话授权：{$grants}
notice-revoke-grants = 已撤销 {$count} 项会话授权
notice-session-delete-confirm = 将永久删除历史会话 {$id}。请再次输入 /sessions delete {$id} 确认；可按 Esc 或执行其他删除命令取消。
notice-session-deleted = 已删除历史会话 {$id}
error-session-delete = 删除历史会话失败：{$error}
notice-history-help = 历史：↑/↓ 选择，Enter 恢复，Delete 删除（需再次确认），/new 新建会话
notice-provider-settings-open = 当前 provider={$provider}；已打开 Session Settings
error-route-not-ready = 会话尚未就绪，无法校验路由
notice-provider-switching = 正在切换 Provider：{$provider}
notice-model-settings-open = 当前 model={$model}；已打开 Session Settings；可选：{$models}
notice-model-switching = 正在切换模型：{$model}
notice-network-state = 当前 Turn network={$enabled}
notice-browser-state = 当前 Turn browser={$enabled}；页面内容不会写入会话历史
notice-browser-status = 当前 Turn browser={$enabled}；浏览器需由用户以专用 CDP Profile 启动
notice-mcp-server-list = MCP server 未配置或未启用：{$server}；可选：{$available}
notice-mcp-server-selected = 当前 Turn MCP server={$server}
notice-mcp-server-disabled = MCP server 未配置或未启用：{$server}；可选：{$available}
error-a2a-selection = A2A peer 不可选择：{$error}
notice-a2a-selected = 当前 Turn 已选择 A2A peer={$peer}
notice-memory-delete = 请求删除记忆：{$id}
notice-subagents-state = 当前 Turn subagents={$enabled}
notice-subagents-max = 当前 Turn max_subagents={$max}
notice-subagents-status = 当前 Turn subagents={$enabled}，max_subagents={$max}
notice-trace-state = 当前 Turn trace={$enabled}
error-session-stream-ended = Session 事件流意外结束
notice-memory-state = 当前 Turn memory_recall={$enabled}

    用法：rivo a2a <COMMAND>

    命令：discover、pair、peers、inspect、revoke、delegate、control、cancel、recover、serve、pairings、approvals、provision

    选项：-h、--help 显示帮助
cli-config-help = 管理 Rivo Vendor、Model、Provider 和 Credential。

    用法：rivo config [COMMAND]

    命令：path、show、validate、set、unset、credentials、route、vendor、model、provider

    选项：-h、--help 显示帮助
cli-run-help = 运行一个 Rivo 任务。

    用法：rivo run --prompt <TEXT> [OPTIONS]
          rivo run --stdin [OPTIONS]

    选项：--language <BCP47> --vendor <ID> --model <ID> --provider-id <ID> --format <text|jsonl> --approval-mode <prompt|deny|manifest>

    任务必须通过 --prompt 或 --stdin 提供。
cli-root-help = Rivo —— 通用 Agent CLI

    用法：
      rivo <COMMAND>
      rivo run --prompt <TEXT> [OPTIONS]
      rivo run --stdin [OPTIONS]

    命令：
      run          运行任务
      tui          打开交互式终端工作区
      config       管理 Vendor、Model、Provider 和 Credential
      language     管理本地 UI 语言文件
      sessions     查看、恢复或删除本地会话
      mcp          管理已配置的 MCP server
      a2a          管理 A2A peer 和本机 A2A server
      instance     查看本机 Rivo 实例
      diagnostics  创建脱敏诊断包
      upgrade      检查或应用平台更新
      app-server   启动本机 JSON-RPC bridge

    选项：
      -h, --help     显示帮助
      -V, --version  显示版本

    常用任务选项：--vendor、--model、--provider-id、--auto-provider、--format 和 --approval-mode。
    任务必须通过 --prompt 或 --stdin 显式提供。运行 `rivo <COMMAND> --help` 查看命令帮助。
cli-tui-help = 打开交互式终端工作区。

    用法： rivo tui [OPTIONS]

    选项：
          --workspace <PATH>  覆盖本次 TUI 会话的工作区
      -h, --help              显示帮助

    示例：
      rivo tui
      rivo tui --workspace C:\\path\\to\\project

cli-tui-terminal-required = rivo tui 需要交互式终端；请在本地终端运行，自动化请使用 `rivo run --format jsonl`
cli-tui-current-directory-missing = 无法确定当前工作目录
cli-tui-startup-cancelled = 已取消启动 TUI
cli-tui-startup-save-failed = 无法保存启动配置：{$error}
cli-tui-first-launch-terminal-required = 尚未配置 Provider；请在交互终端运行 `rivo tui`，或使用 `rivo config provider set ...`
cli-tui-first-launch-workspace-invalid = 工作区不存在或不是目录：{$path}
cli-tui-first-launch-provider-saved = Provider 已保存并完成显式发现；下一步选择 Vendor 和 Model。
cli-tui-first-launch-provider-id = Provider ID
cli-tui-first-launch-base-url = Base URL
cli-tui-first-launch-credential = 已有 Credential 名称
cli-tui-first-launch-discover = 保存并立即发现模型（yes/no）
cli-tui-first-launch-title = Rivo · 首次配置
cli-tui-first-launch-workspace = 工作区：{$path}
cli-tui-first-launch-help = Enter 下一项/最后一项确认 · Tab/↑↓ 切换 · Esc 取消
cli-tui-first-launch-cancelled = 已取消首次配置；配置未保存
cli-tui-first-launch-not-confirmed = 未确认保存和发现；配置未保存
cli-tui-first-launch-discovery-failed = Provider 已保存，但模型发现失败；请检查认证引用并显式 refresh
cli-tui-first-launch-provider-exists = Provider 已存在；请通过受控配置变更更新

cli-workspace-added = workspace_id={$id}
    name={$name}
    path={$path}
cli-workspace-opened = 已打开工作区 {$name}（{$path}）
cli-workspace-renamed = 已重命名工作区 {$id}
cli-workspace-rebound = 已将工作区 {$id} 重新绑定到 {$path}
cli-workspace-removed = 已移除工作区 {$id} 及其会话
cli-workspace-storage-worker-interrupted = 本地会话存储 worker 已中断
cli-workspace-id = ID
cli-workspace-name = 名称
cli-workspace-path = 路径
cli-workspace-default = 默认
cli-workspace-last-opened = 最近打开
cli-workspace-rename-usage = 用法：rivo workspace rename <PATH|WORKSPACE_ID> <NAME>
cli-workspace-rebind-usage = 用法：rivo workspace rebind <WORKSPACE_ID> <PATH>
cli-workspace-remove-usage = 用法：rivo workspace remove <PATH|WORKSPACE_ID> --yes
cli-workspace-usage = 用法：rivo workspace list|add|open|show|rename|rebind|remove
cli-workspace-format-value = --format 缺少值
cli-workspace-list-arg = 未知 workspace list 参数：{$value}
cli-workspace-format-invalid = 无效输出格式：{$value}
cli-workspace-add-usage = 用法：rivo workspace add <PATH> [--name <NAME>]
cli-workspace-one-arg-usage = 用法：rivo workspace {$command} <PATH|WORKSPACE_ID>
cli-diagnostics-created = 已创建 {$path}
cli-diagnostics-usage = 用法：rivo diagnostics create <path> [--yes]
cli-diagnostics-confirm = 诊断包可能包含脱敏后的用户业务内容。继续导出？ [y/N]
cli-diagnostics-cancelled = 已取消诊断导出；自动化调用请显式传入 --yes
cli-diagnostics-storage-worker-interrupted = 本地会话存储 worker 已中断
cli-instance-data-directory-error = 无法确定实例数据目录
cli-instance-skill-missing = 默认 process-control Skill 未加载
cli-instance-usage = 用法：rivo instance [list|endpoint|skill]
cli-instance-list-help = 以 JSON 列出已登记的本机实例。

    用法：rivo instance list
cli-instance-endpoint-help = 显示本机控制 endpoint。

    用法：rivo instance endpoint
cli-instance-skill-help = 显示内置 process-control Skill。

    用法：rivo instance skill
cli-upgrade-help = 检查或应用当前平台的最新 CLI 归档。

    用法：rivo upgrade [选项]

    选项：
          --check  仅比较版本，不下载或安装
          --apply  下载、校验并替换当前安装
          --yes    跳过升级确认
      -h、--help   显示帮助
cli-app-server-help = 启动本机 JSON-RPC bridge。

    用法：rivo app-server <--stdio|--pipe>

    选项：
          --stdio  通过标准输入和标准输出传输 JSON-RPC
          --pipe   通过 Windows Named Pipe 提供本机服务
      -h、--help   显示帮助
cli-language-confirm-rollback = 回滚语言文件需要显式传入 --yes
cli-language-confirm-install = 语言文件已存在：{$path}；覆盖需要显式传入 --yes
cli-language-confirm-remove = 删除语言文件需要显式传入 --yes
cli-language-confirm-update = 更新语言文件需要显式传入 --yes
cli-language-registry-missing = registry 中没有语言 {$locale}
cli-language-destination-missing = 语言安装目标路径缺失
cli-language-async-command = 远程 language 命令必须通过异步入口执行
cli-upgrade-current-version = 当前版本：{$version}
cli-upgrade-latest-version = 最新版本：{$version}
cli-upgrade-current = rivo 已是最新版本。
cli-upgrade-available = 有可用更新。
cli-upgrade-downgrade = 远端版本 {$remote} 低于当前版本 {$current}；已拒绝降级
cli-upgrade-installed = 已安装 Rivo {$version}。请重新启动新的 CLI/TUI 进程。
cli-upgrade-platform-unsupported = rivo upgrade 不支持当前平台：{$platform}
cli-upgrade-arch-unsupported = rivo upgrade 不支持当前架构：{$arch}
cli-upgrade-http-client-failed = 创建升级 HTTP 客户端失败
cli-upgrade-manifest-read-failed = 读取 latest manifest 失败：{$url}
cli-upgrade-checksums-read-failed = 读取 SHA256SUMS 失败：{$url}
cli-upgrade-http-request-failed = HTTP 请求失败
cli-upgrade-http-response-read-failed = 读取 HTTP 响应失败
cli-upgrade-json-parse-failed = 解析 JSON 响应失败
cli-upgrade-schema-unsupported = 不支持的 release manifest schema：{$schema}
cli-upgrade-product-mismatch = release manifest product 不匹配：{$product}
cli-upgrade-target-mismatch = release manifest 平台不匹配：{$manifest_platform}/{$manifest_arch}；当前为 {$platform}/{$arch}
cli-upgrade-artifact-missing = latest manifest 缺少当前平台安装包
cli-upgrade-artifact-path-invalid = 安装包文件名不得包含路径分隔符：{$file}
cli-upgrade-checksum-invalid = 安装包 checksum 不是有效 SHA-256：{$file}
cli-upgrade-checksum-entry-missing = SHA256SUMS 未声明 manifest 中的安装包：{$file}
cli-upgrade-download-failed = 下载安装包失败
cli-upgrade-download-http-failed = 下载安装包返回 HTTP {$status}
cli-upgrade-package-read-failed = 读取安装包失败
cli-upgrade-checksum-mismatch = 安装包 checksum 不匹配：expected {$expected}，got {$actual}
cli-upgrade-temp-write-failed = 写入临时安装包失败：{$path}
cli-upgrade-semver-invalid = 版本号不是受支持的 SemVer：{$value}
cli-sessions-path-help = 显示本地会话数据库路径。

    用法：rivo sessions path
cli-sessions-backup-help = 备份当前数据库，不修改源文件。

    用法：rivo sessions backup <target>
cli-sessions-reset-help = 验证备份后，将 schema 14 会话重建为 schema 18。

    用法：rivo sessions reset --backup <target> --yes
cli-sessions-migrate-help = 备份、校验并将 schema 17 迁移为 schema 18 Workspace 历史。

    用法：rivo sessions migrate --yes
    rivo sessions migrate --backup <target> --yes
cli-sessions-list-help = 列出本地会话。

    用法：rivo sessions list
cli-sessions-show-help = 显示会话事件和消息。

    用法：rivo sessions show <SESSION_ID>
cli-sessions-resume-help = 在匹配的 Workspace 中恢复会话并提交新任务。

    用法：rivo sessions resume <SESSION_ID> (--prompt <TEXT>|--stdin) [--workspace <PATH|WORKSPACE_ID>]
cli-sessions-attach-help = 将 unscoped 旧会话显式归属到 Workspace。

    用法：rivo sessions attach <SESSION_ID> <PATH|WORKSPACE_ID>
cli-sessions-delete-help = 删除指定本地会话。

    用法：rivo sessions delete <SESSION_ID> [--yes]
cli-sessions-clear-help = 清空全部本地会话历史。

    用法：rivo sessions clear [--yes]
cli-sessions-backup-created = 已创建备份
    source={$source}
    destination={$destination}
    schema_version={$schema}
    sha256={$sha256}
cli-sessions-reset-complete = 已重置：已创建 schema 18 会话历史；schema 14 备份已保留
cli-sessions-migration-current = 无需迁移：schema 18 已是当前版本
cli-sessions-migration-complete = 迁移完成：schema 17 -> schema 18
    backup={$backup}
    backup_sha256={$sha256}
    bound_sessions={$bound}
    unscoped_sessions={$unscoped}
    backup retained
cli-sessions-migration-complete-simple = 迁移完成：已创建 schema 18 Workspace 会话历史；备份已保留
cli-sessions-deleted = 已删除 {$id}
cli-sessions-cleared = 已清空本地会话历史
cli-sessions-attached = 已归属会话 {$id}
cli-sessions-expired = 已过期：{$reason}
cli-sessions-restorable = 可恢复
cli-sessions-no-persisted-task = 尚无已持久化任务
cli-sessions-retention-policy = 本地留存策略
cli-sessions-column-id = ID
cli-sessions-column-workspace-id = Workspace ID
cli-sessions-column-workspace = Workspace
cli-sessions-column-workspace-path = Workspace 路径
cli-sessions-column-created = 创建时间
cli-sessions-column-summary = 摘要
cli-sessions-column-events = 事件
cli-sessions-column-messages = 消息
cli-sessions-column-status = 状态
cli-sessions-column-type = 类型
cli-sessions-column-sequence = 序号
cli-sessions-column-kind = 类型
cli-sessions-confirm-delete = 删除本地会话 {$id}
cli-sessions-confirm-clear = 清空所有本地会话历史
cli-sessions-workspace-value-missing = --workspace 缺少值
cli-sessions-unknown-list-arg = 未知 sessions list 参数：{$value}
cli-sessions-scope-exclusive = --workspace、--all-workspaces、--unscoped 互斥
cli-sessions-storage-worker-interrupted = 本地会话存储 worker 已中断
cli-sessions-backup-usage = 用法：rivo sessions backup <target>
cli-sessions-reset-usage = 用法：rivo sessions reset --backup <target> --yes
cli-sessions-migrate-usage = 用法：rivo sessions migrate --yes|--backup <target> --yes
cli-sessions-show-usage = 用法：rivo sessions show <session_id>
cli-sessions-delete-usage = 用法：rivo sessions delete <session_id> [--yes]
cli-sessions-clear-usage = 用法：rivo sessions clear [--yes]
cli-sessions-resume-usage = 用法：rivo sessions resume <session_id> (--prompt <TEXT>|--stdin)
cli-sessions-stdin-empty = --stdin 没有收到非空 UTF-8 任务
cli-sessions-prompt-empty = --prompt 必须提供非空任务文本
cli-sessions-attach-usage = 用法：rivo sessions attach <session_id> <PATH|WORKSPACE_ID>
cli-sessions-usage = 用法：rivo sessions path|backup <target>|reset --backup <target> --yes|migrate --yes|migrate --backup <target> --yes|list [--workspace <path|id>|--all-workspaces|--unscoped]|show <id>|resume <id> (--prompt <TEXT>|--stdin) [--workspace <path|id>]|attach <id> <path|id>|delete <id> [--yes]|clear [--yes]
cli-sessions-prompt-value-missing = --prompt 缺少值
cli-sessions-prompt-duplicate = --prompt 只能指定一次
cli-sessions-stdin-duplicate = --stdin 只能指定一次
cli-sessions-resume-usage-workspace = 用法：rivo sessions resume <session_id> (--prompt <TEXT>|--stdin) [--workspace <path|id>]
cli-sessions-prompt-or-stdin = 必须且只能指定 --prompt 或 --stdin
cli-sessions-events = 事件
cli-sessions-messages = 消息
cli-run-media-ready-path = rivo：媒体已就绪：{$path}
cli-run-media-ready-url = rivo：媒体已就绪：{$url}
cli-run-media-started = rivo：媒体操作已开始：{$operation}
cli-run-media-failed = rivo：媒体操作失败：{$message}
cli-run-approval-unavailable = rivo：无 TTY，无法显示确认提示；请求已拒绝
cli-run-approval-denied = rivo：已拒绝 {$tool} 的确认；请使用 TTY 提示或精确的 approval manifest 规则
cli-run-interaction-noninteractive = rivo：非交互模式无法回答交互 {$id}
cli-run-stopped = rivo：运行已停止：{$reason}
cli-run-stream-ended = rivo：会话事件流在终止事件前结束
cli-run-shutdown-failed = rivo：会话关闭失败：{$error}
cli-mcp-not-found = MCP server 不存在
cli-mcp-login-oauth-only = rivo mcp login 仅适用于配置了 --oauth 的 HTTP server
cli-mcp-login-success = MCP server {$name} 登录成功；凭据已存入系统 keychain
cli-mcp-disabled = MCP server 未启用：{$name}
cli-mcp-state-changed = MCP server {$name} 已{$action}
cli-mcp-install-ready = 已安装 MCP server {$name}；status=ready tools={$tools} resources={$resources} prompts={$prompts}
cli-mcp-install-oauth-pending = 已安装 MCP server {$name}（OAuth 未登录，已跳过实时发现）；请运行 `rivo mcp login {$name}`，随后 `rivo mcp inspect {$name}` 验证
cli-mcp-toolset-missing-tool = MCP server {$server} 的 toolset {$toolset} 引用了未发现的工具 {$tool}
cli-config-path-unresolved = 无法确定配置文件路径
cli-mcp-name-invalid = MCP server 名称不能为空且不能包含空白字符
cli-mcp-oauth-http-only = --oauth 仅适用于 http MCP
cli-mcp-loopback-http-only = --allow-loopback 仅适用于 http MCP
cli-mcp-auto-login-http-only = --auto-login 仅适用于 http MCP
cli-mcp-flag-value-missing = {$flag} 缺少值
cli-mcp-capability-unknown = 未知 MCP capability：{$capability}
cli-mcp-toolset-risk-invalid = toolset 风险必须是 read_only、action 或 unknown
cli-mcp-toolset-format-invalid = toolset 格式为 <name>:<risk>:<tool,tool>
cli-mcp-header-env-format-invalid = --header-env 格式为 <Header>=<ENV>
cli-mcp-env-format-invalid = --env 格式为 <KEY>=<ENV>
cli-mcp-bearer-env-missing = --bearer-env 需要环境变量名
cli-mcp-oauth-token-env-missing = --oauth-token-env 需要环境变量名
cli-mcp-oauth-port-invalid = --oauth-redirect-port 必须是端口号
cli-mcp-install-flag-unsupported = 不支持的 MCP install 参数：{$flag}
cli-mcp-capability-required = 必须至少声明一个 --capability
cli-mcp-toolset-required = 声明 tools capability 时必须配置至少一个精确 --toolset
cli-mcp-auth-conflict = --bearer-env 与 --oauth 不能同时使用
cli-mcp-stdio-header-env-unsupported = stdio MCP 不支持 --header-env；请由进程环境提供凭据
cli-mcp-transport-invalid = MCP install transport 必须是 http 或 stdio
cli-mcp-config-invalid = MCP 配置输出无效
cli-mcp-usage = 用法：rivo mcp list|show|inspect <server>|install|enable|disable|remove
cli-output-property = 属性
cli-output-value = 值
cli-output-type = 类型
cli-output-name = 名称
cli-output-description = 描述
cli-output-config-path-unresolved = 无法确定配置文件路径
cli-output-table-column-mismatch = 表格列数与表头不一致
cli-output-format-duplicate = --format 不能重复
cli-output-format-missing = --format 缺少值
cli-mcp-root-help = 管理已配置的 MCP server。

    用法：rivo mcp <COMMAND>

    命令：list、show、inspect、login、install、enable、disable、remove

    选项：-h、--help  显示帮助
cli-mcp-list-help = 列出已配置的 MCP server。

    用法：
      rivo mcp list
cli-mcp-show-help = 显示 MCP server 的脱敏配置。

    用法：
      rivo mcp show <NAME>
cli-mcp-inspect-help = 连接 MCP server 并发现 tools、resources 和 prompts。

    用法：
      rivo mcp inspect <NAME>
cli-mcp-login-help = 为 HTTP MCP server 执行 OAuth 登录。

    用法：
      rivo mcp login <NAME>
cli-mcp-enable-help = 启用 MCP server。

    用法：
      rivo mcp enable <NAME>
cli-mcp-disable-help = 禁用 MCP server。

    用法：
      rivo mcp disable <NAME>
cli-mcp-remove-help = 删除 MCP server。

    用法：
      rivo mcp remove <NAME>
cli-mcp-install-help = 安装并验证 MCP server。

    用法：
      rivo mcp install <http|stdio> <NAME> <TARGET> [OPTIONS]

    命令：
      http   安装 Streamable HTTP server
      stdio  安装本机 stdio server
cli-mcp-install-http-help = 安装 Streamable HTTP MCP server。

    用法：
      rivo mcp install http <NAME> <HTTPS_URL> [OPTIONS]

    选项：
      --capability <LIST>     允许的能力
      --toolset <SPEC>        命名 toolset；可重复
      --header-env <H>=<ENV>  从环境变量读取 HTTP header
      --bearer-env <ENV>      从环境变量读取 Bearer token
      --oauth                 启用 OAuth
      --allow-loopback        允许 loopback URL
      --allow-sampling       允许受控 sampling
cli-mcp-install-stdio-help = 安装本机 stdio MCP server。

    用法：
      rivo mcp install stdio <NAME> <PROGRAM> [OPTIONS]

    选项：
      --arg <ARG>           进程参数；可重复
      --cwd <PATH>          进程工作目录
      --env <KEY>=<ENV>     从环境变量传入进程环境
      --capability <LIST>   允许的能力
      --toolset <SPEC>      命名 toolset；可重复
      --allow-sampling      允许受控 sampling
cli-config-valid = 有效
cli-config-updated = 已更新 {$path}
cli-config-credential-saved = 已保存 credential：{$name}
cli-config-credential-removed = 已移除 credential：{$name}
cli-config-credentials-migrated = 已迁移 {$count} 个 credential
cli-config-saved-revision = 已保存 revision={$revision}
cli-config-discovery-refreshed = 模型发现已刷新：{$provider}
cli-config-removed-revision = 已移除 revision={$revision}
cli-config-no-provider = 没有可用于 model={$model} vendor={$vendor} 的 Provider
cli-config-discovery-failed = 模型发现失败；请检查 Provider 连接与认证引用
cli-config-discovery-invalidated = 模型发现已失效；请执行 `config provider models refresh {$provider}`
cli-config-model-state = model={$id}	{$state}
cli-config-credentials-help = 管理本地 Credential。

    用法：rivo config credentials [COMMAND]

    命令：path、show、set、unset、migrate

    选项：-h、--help  显示帮助
cli-config-path-help = 显示配置文件路径。

    用法：rivo config path

    选项：-h、--help  显示帮助
cli-config-show-help = 显示脱敏后的有效配置。

    用法：rivo config show

    选项：-h、--help  显示帮助
cli-config-validate-help = 验证配置和模型目录。

    用法：rivo config validate

    选项：-h、--help  显示帮助
cli-config-set-help = 设置配置项。

    用法：rivo config set <KEY> [VALUE]

    选项：-h、--help  显示帮助
cli-config-unset-help = 清除配置项。

    用法：rivo config unset <KEY> [VALUE]

    选项：-h、--help  显示帮助
cli-config-route-help = 预览 Vendor/Model 路由，不发起调用。

    用法：rivo config route --vendor <VENDOR> [--model <MODEL>] [--provider-id <PROVIDER>]

    选项：-h、--help  显示帮助
cli-config-vendor-help = 管理本地 Vendor 目录。

    用法：rivo config vendor <list|show|set|remove> [ARGS]

    选项：-h、--help  显示帮助
cli-config-model-help = 管理规范 Model 目录。

    用法：rivo config model <list|show|set|remove> [ARGS]

    选项：-h、--help  显示帮助
cli-config-provider-help = 管理 Provider 连接。

    用法：rivo config provider <list|show|set|remove> [ARGS]

    选项：-h、--help  显示帮助
cli-config-provider-set-help = 管理 Provider 连接。

    用法：rivo config provider set <ID> --base-url <URL> [--credential-name <NAME>] [--enabled <BOOL>] [--priority <N>] [--refresh] [--confirm <TOKEN>]

    选项：-h、--help  显示帮助
cli-config-credentials-path-help = 显示 Credential 文件路径。

    用法：rivo config credentials path

    选项：-h、--help  显示帮助
cli-config-credentials-show-help = 列出 Credential 名称和保存状态，不显示密钥正文。

    用法：rivo config credentials show

    选项：-h、--help  显示帮助
cli-config-credentials-set-help = 交互式保存 Credential，或从环境变量读取。

    用法：rivo config credentials set <NAME> [--from-env <ENV>]

    选项：-h、--help  显示帮助
cli-config-credentials-unset-help = 删除 Credential。

    用法：rivo config credentials unset <NAME>

    选项：-h、--help  显示帮助
cli-config-credentials-migrate-help = 将旧版明文 Credential 迁移为加密存储。

    用法：rivo config credentials migrate

    选项：-h、--help  显示帮助
cli-config-credentials-set-usage = 用法：rivo config credentials set <环境变量名>
cli-config-credentials-from-env-usage = 用法：rivo config credentials set <name> --from-env <environment-variable>
cli-config-credentials-noninteractive = 无法读取非交互式输入中的 API key；自动化调用请使用 --from-env <environment-variable>
cli-config-credentials-mismatch = 两次输入的 API key 不一致
cli-config-credentials-empty = API key 不能为空
cli-config-credentials-unset-usage = 用法：rivo config credentials unset <环境变量名>
cli-config-credentials-in-use = Credential {$name} 仍被 Provider 引用：{$providers}；请先修正这些 Provider
cli-config-credentials-usage = 用法：rivo config credentials path|show|set|unset|migrate
cli-config-model-providers-usage = 用法：rivo config model providers <model-id>
cli-config-provider-models-usage = 用法：rivo config provider models list|remote|refresh <provider-id>
cli-config-provider-not-found = Provider 不存在：{$id}
cli-config-vendor-not-found = Vendor 不存在：{$id}
cli-config-model-not-found = Model 不存在：{$id}
cli-config-show-id-missing = show 需要 ID
cli-config-route-usage = 用法：rivo config route --vendor <vendor> [--model <model>] [--provider-id <provider>]
cli-output-field = 字段
cli-config-set-usage = 用法：rivo config {$command} <key> [value]
cli-config-value-missing = 缺少 {$key} 的值
cli-config-value-invalid = {$key} 的值无效：{$error}
cli-config-unsupported-field = 不支持的配置字段：{$key}
cli-config-usage = 用法：rivo config path|show|validate|set|unset|vendor|model|provider|credentials|route
cli-config-locale-missing = 缺少 locale
cli-config-locale-invalid = locale 无效：{$error}
cli-config-mcp-json-missing = 缺少 MCP server JSON
cli-config-mcp-json-invalid = MCP server JSON 无效
cli-config-mcp-name-missing = 缺少 MCP server name
cli-config-credential-not-found = Credential {$name} 不存在，请先执行 `config credentials set {$name}`
cli-config-model-discovery-failed = 模型发现失败；请检查认证引用并显式 refresh
cli-config-no-compatible-models = 模型发现成功，但没有发现模型目录中的模型
cli-config-env-missing = 环境变量未设置：{$name}
cli-a2a-serve-usage = 用法：rivo a2a serve [<server-profile>] --config <owner-a2a-server.toml>
cli-a2a-transport-invalid = A2A transport 必须是 mtls 或 psk
cli-a2a-server-args-invalid = A2A Server 参数无效
cli-a2a-config-missing = A2A Server 配置路径不能为空
cli-a2a-psk-secret-required = PSK transport 必须使用 --secret-stdin
cli-a2a-mtls-secret-forbidden = mTLS transport 不接受 --secret-stdin
cli-a2a-psk-transport-required = --secret-stdin 必须与 --transport psk 一起使用
cli-a2a-secret-invalid = PSK secret 必须是 64 个 hex 字符
cli-a2a-secret-length-invalid = PSK secret 必须正好是 32 bytes
cli-a2a-secret-zero = PSK secret 不能全为零
cli-a2a-pairings-usage = 用法：rivo a2a pairings list --config <owner-a2a-server.toml>|pairings approve|reject <request-id>|revoke <peer-id> --config <owner-a2a-server.toml>
cli-a2a-approvals-usage = 用法：rivo a2a approvals list --config <owner-a2a-server.toml>|approvals approve|reject <approval-id> --config <owner-a2a-server.toml>
cli-a2a-owner-terminal-required = A2A 审批只能在 Server 所在本机的交互式 owner 用户会话执行
cli-a2a-pairing-cancelled = 已取消 A2A 配对裁决
cli-a2a-approval-cancelled = 已取消 A2A 本机审批裁决
cli-a2a-delegate-usage = 用法：rivo a2a delegate <peer-id> <skill-id> --message-stdin --yes [--stream]
cli-a2a-peer-skill-invalid = A2A peer_id 或 skill_id 无效
cli-a2a-duplicate-flag = 重复的 A2A 参数：{$flag}
cli-a2a-unknown-flag = 未知的 A2A 参数：{$flag}
cli-a2a-delegate-confirm-required = A2A 委派必须使用 --message-stdin 读取正文，并以 --yes 显式确认数据外发
cli-a2a-control-usage = 用法：rivo a2a control <peer-id> <target-instance-id> <method> --params-stdin [--yes]
cli-a2a-control-id-invalid = A2A control 标识或 method 无效
cli-a2a-control-method-forbidden = process-control 不允许该 method
cli-a2a-control-params-required = A2A control 必须使用 --params-stdin
cli-a2a-control-confirm-required = 变更型 A2A control 必须使用 --yes
cli-a2a-recover-usage = 用法：rivo a2a recover <peer-id> <task-id> [--transport psk --secret-stdin]
cli-a2a-peer-task-invalid = A2A peer_id 或 task_id 无效
cli-a2a-cancel-usage = 用法：rivo a2a cancel <peer-id> <task-id> --yes [--transport psk --secret-stdin]
cli-a2a-pair-usage = 用法：rivo a2a pair <local-identity-label> --endpoint <https-url> --spki-sha256 <sha256>
cli-a2a-local-identity-missing = 本机 identity label 不能为空
cli-a2a-identity-invalid = A2A identity label 无效
cli-a2a-pair-args-invalid = A2A pair 参数无效
cli-a2a-endpoint-missing = A2A endpoint 不能为空
cli-a2a-spki-missing = A2A SPKI 不能为空
cli-a2a-sas-terminal-required = A2A 配对需要本机交互式 SAS 确认
cli-a2a-pair-cancelled = 已取消 A2A 配对
cli-a2a-profile-required = 配置含多个 Server profile；请显式指定 `rivo a2a serve <server-profile> --config <path>`
cli-a2a-bootstrap-secret-missing = PSK Server 缺少仅内存的 bootstrap secret
cli-a2a-server-tls-read-failed = 无法从本机 keychain 读取 A2A Server TLS 身份；配置和 SQLite 不接受私钥
cli-a2a-workspace-alias-invalid = A2A workspace alias 无法解析：{$alias}
