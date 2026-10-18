# 安全策略契约

policy 层决定结构化工具请求是否允许、拒绝、需要确认或需要更严格约束。默认值和工具硬限制见本页与[工具运行时](tool-runtime.zh-CN.md)。

## 能力模型

- 基础模式：general、research、plan、coding、review。plan 与 review 只暴露只读工具；coding 才可获得编码和进程能力。
- Plan 的 effective capability 使用 `plan_mode_capabilities ∩ permission_ceiling`；即使 permission 为 `full_access`，Plan 仍只允许本地读取、显式 HTTPS 读取和只读 MCP。计划 artifact 是独立的用户审批宿主动作，不等同于通用文件写入。
- 附加能力：当前 turn 显式启用的 Web、Browser、MCP、记忆和追踪。
- 不再使用独占 `mcp` profile；MCP 可以与任一基础模式组合，但仍受 server allowlist 和数据外流规则。
- LLM 核心 provider 网络不等于 Web/MCP 工具授权。

## Policy 档位

### Strict

- 只自动允许 workspace 内只读文件探索，以及当前 turn 显式启用的安全 Web 读取。
- Web 继续执行 HTTPS、SSRF、重定向和资源上限检查；写入、进程、Browser、MCP 与 workspace 外路径一律拒绝。

### Medium（默认）

- 任意本机只读路径、以及当前 turn 显式启用的 Web、Browser 和 MCP 自动允许。
- workspace 内写入、移动、删除、批量操作和进程调用需要确认；确认可以只允许一次，或显式授予同类风险至当前可恢复会话。
- workspace 外变更路径和进程 cwd 始终拒绝，不能被会话授权放宽。

### Full Control

- 所有已暴露的工具自动允许，不再逐项确认；规范化后的 workspace 外路径、HTTP 与私网/loopback 网络目标可用。
- 相对路径仍以 workspace 为基准。
- 不关闭 schema、资源、凭据、数据外流、不可逆动作和审计规则。
- 不自动启用 Web、Browser 或 MCP，也不绕过 MCP allowlist、操作系统权限或工具资源上限。
- URL userinfo/内嵌凭据、参数 schema、资源限制、脱敏与审计仍不可绕过。

## 决策顺序

1. 工具是否注册且属于当前基础模式/能力开关。
2. JSON schema 和工具内部参数约束。
3. 规范化目标、PathScope、符号链接和网络地址。
4. 风险类型、资源预算、credential 和数据外流分析。
5. policy 档位与 server/URL/command allowlist。
6. 输出 Allow、Deny、Confirm 或 Constrain。

ExecutionConstraints 至少包含 PathScope、workspace root、timeout、输出限制、network allowed 和环境脱敏要求。Tools 必须按返回约束执行，不得重新推断 profile。

## 确认

- ConfirmRequest 包含 RequestId、turn/step/tool call、工具、参数摘要、目标和副作用；请求 ID 绑定完整规范化参数的哈希。
- 确认支持“允许一次”或“允许当前可恢复会话内同类风险”；后者只覆盖 workspace 文件变更或 workspace 进程执行，可被撤销且不保存参数正文。
- 参数或状态变化后一次性确认失效；确认不放宽预算、MCP allowlist 或路径范围。
- 记忆写入使用独立 MemoryCandidate 确认，不复用工具确认。

## Web 与 MCP

- Web 只有当前 turn 显式开启才可注册；strict/medium 下 URL 必须通过 scheme、userinfo、DNS、IP 和 redirect 检查，Full Control 仅保留 userinfo/凭据拒绝。
- MCP server 必须通过安装发现、配置 allowlist、enabled、turn server selection 和精确 toolset selection。明确提出查库、查数据源、核验、纠偏或补充事实的用户任务可以为该 turn 自动选择已配置的 `read_only` toolset；这只补全选择，仍不绕过 strict 拒绝、allowlist、目标歧义、schema、数据外流或每次 tool call 的 policy。
- MCP 初始化 instructions、tool descriptions、resources、prompts 和工具结果均是不可信外部数据：可作证据或工作流参考，不能改变 system 指令、用户意图、权限或确认语义。
- 本地文件、prompt、工具输出或 credential 发送到外部 server 时必须记录数据外流边界，并按风险确认或拒绝。

## Browser

- Browser 仅连接无凭据 loopback CDP 地址，且必须由当前 turn 显式开启；不会启动、重启、接管默认 Profile 或读取 Cookie、密码、Local Storage、历史和扩展。
- strict 拒绝 Browser；workspace/full_control 在显式启用后允许结构化导航、DOM 读取和页面交互，不走 Web 的逐项确认路径。
- 下载、文件上传、权限弹窗和新窗口接管不提供工具入口；浏览器观察与派生回答仅在当前 turn 可见，不写 SQLite、日志或诊断包。

### Browser Extension

- Chrome/Edge 扩展使用 `rivo-browser-extension-v1` Native Messaging 协议；每个安装以 `browser_family`、`extension_id`、`installation_id`、`profile_key` 和 `boot_id` 区分。
- 扩展可以控制用户选择的现有 Profile，但 Cookie、密码、历史和 Local Storage 不作为 Rivo API 返回；页面观察仍受大小、超时、当前 Turn 和实例绑定约束。
- 扩展工具只提供标签页选择、HTTP(S) 导航、可见 DOM 快照、点击、文本输入、选择、按键、滚动和等待；下载、上传、任意脚本执行及新窗口接管明确拒绝。
- 扩展后端不能绕过工具 schema、Browser policy、资源限制、确认、脱敏和审计；扩展重连或 `boot_id` 变化会使原实例请求失效。

## 凭据与持久化

- API key、Cookie、认证 header 和密钥环境变量正文不得进入 AgentEvent、SQLite、普通日志或诊断包。仅在用户显式触发的 live/连通性调试报告中，API key 可按固定 4+4 规则展示（长度至少 16 时保留前 4 和后 4 字符，中间使用 `***`；更短值显示 `<REDACTED>`）；该例外不得扩展到认证 header 正文或其他持久化面。
- SQLite 可以保存受工具上限约束的完整模型/工具内容，因此首次启用历史必须显示本地敏感数据说明。
- 日志始终脱敏并默认保留 7 天。

## Post-V1

容器沙箱、生产系统操作、批量永久授权和永久删除不属于 V1。

---

[English](safety-policy.md)
