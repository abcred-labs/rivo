# A2A 跨平台研究集群组建手册

> 适用范围：V1 Windows 11 x64 Client + Debian/Ubuntu Server 的受控局域网研究分工。
> 正式 transport 为 HTTPS/TLS 1.3 + mTLS；PSK 仅用于互操作诊断，不作为生产信任路径。
> 本手册是双主机互操作扩展，不属于 Windows/Linux/macOS 共用的默认本机多进程 Gate，也不阻断 RF-11。

## 1. 拓扑与边界

```text
Windows 11 x64（协调 Agent / Client）
  192.168.8.30:8443  -- TCP/8443 -->  Debian/Ubuntu（research Server）
                                      192.168.8.11:8443
```

两台主机必须位于同一受控 RFC1918/ULA 网段。Server 只绑定明确私网 IP，禁止公网地址、DNS bind、`0.0.0.0`、`::`、NAT 穿透和公网目录。mDNS/DNS-SD 只产生候选，不产生信任；在 mDNS 被网络策略阻断时使用显式 endpoint，并在证据中记录 `discovery=explicit-endpoint`。

本手册不承诺中心监督、多租户、无人值守审批、跨组织委派或公网部署。Linux/macOS 原生运行时适配仍属于 V2；本手册只验证协议跨平台互操作。

## 2. 前置条件

- 两台主机安装同一源码提交构建的 `rivo`；Windows 使用 `rivo.exe`，Debian/Ubuntu 使用 ELF release binary。
- Server 主机准备只读测试 workspace，例如 `D:\rivo\research-workspace` 或 `/srv/rivo/research-workspace`。
- 两端可交互核对六词 SAS；Server owner 可在本机 TTY 执行 pairings/approvals 命令。
- Windows 防火墙允许到 Server 的 TCP/8443；Server 防火墙允许来自 Client 私网地址的 TCP/8443。SSH 只用于受控 smoke 脚本，不是 A2A transport。
- 正式 mTLS 需要 Server TLS identity、profile Client CA 和 Client keychain 身份。测试身份通过 `provision` 创建，生产身份必须由组织自己的 keychain/CA 流程提供。

## 3. Server 配置

以下结构与 `A2aServerFileConfig` 一致。将文件保存为 Server 主机上的 `owner-a2a-server.toml`，并确保 `workspace` 目录已存在。

```toml
schema_version = 1
state_db = "state.db"

[workspace_aliases]
research = "/srv/rivo/research-workspace"

[[profiles]]
identity_ref = "a2a-server-research"

[profiles.profile]
name = "research-lan"
bind = "192.168.8.11:8443"
skill_id = "research"
workspace_alias = "research"
capability_profile = "research"
policy_ceiling = "strict"
network_enabled = false
browser_enabled = false
mcp_servers = []
mcp_toolsets = {}
per_peer_max_active_tasks = 1
max_active_tasks = 3
max_message_bytes = 65536
max_output_bytes = 262144
max_duration_seconds = 1800
approval_timeout_seconds = 300
allowed_peers = []
issuer_ref = "a2a-ca-research"
idempotency_retention_seconds = 86400
audit_retention_days = 30
```

`allowed_peers` 在首次配对前为空；配对批准后将 Server 分配的 `peer_id` 写入该数组。配置路径按文件所在目录解析，workspace 必须可 `canonicalize` 且为目录。修改配置后重新执行 `rivo a2a serve ...`，远端请求不能覆盖 profile 字段。

## 4. 凭据与启动

在 Server 主机执行一次测试身份 provisioning（正式环境替换为组织凭据流程）：

```text
rivo a2a provision --yes --bind 192.168.8.11:8443 \
  --server-identity-ref a2a-server-research --issuer-ref a2a-ca-research
rivo a2a provision check \
  --server-identity-ref a2a-server-research --issuer-ref a2a-ca-research
rivo a2a serve research-lan --config owner-a2a-server.toml
```

私钥、Server 证书、Client CA 和配对证书只进入 OS keychain（Windows Credential Manager 或 Linux 等价安全存储）；TOML、SQLite、日志和诊断包只保存引用、指纹和摘要。退出测试后使用 `rivo a2a provision cleanup --yes ...` 清理测试身份。

启动输出应包含 `profile=research-lan`、`bind=192.168.8.11:8443`、`transport=mtls` 和 `spki_sha256`，不得包含私钥、workspace 绝对路径或凭据正文。Linux 长期运行可由 systemd/受控进程管理器托管，但必须保持 owner-only state DB 权限，并将服务日志按 Rivo 脱敏规则收集。

## 5. Client 配对与研究委派

在 Windows Client 主机执行：

```text
rivo a2a discover
rivo a2a pair windows-research \
  --endpoint https://192.168.8.11:8443/a2a \
  --spki-sha256 <server-spki-sha256>
```

Server owner 在本机核对 request ID、SAS、endpoint、SPKI 和客户端公钥摘要后批准：

```text
rivo a2a pairings list --config owner-a2a-server.toml
rivo a2a pairings approve <request-id> --config owner-a2a-server.toml
```

Client 使用 claim 响应返回的不透明 `peer_id`：

```text
rivo a2a inspect <peer-id>
rivo run --a2a-peer <peer-id> --prompt "比较三个候选市场的进入门槛，并返回带来源的 JSON 摘要"
```

只有 trusted peer 才会注册 `a2a_delegate`。研究 profile 固定 `research`/`strict`、网络/Browser/MCP 关闭；不得发送 private memory、credential、本机路径或完整历史。medium policy 下外发确认必须绑定 peer、skill、message hash 和 Artifact hash。

## 6. 网络中断、取消与撤销

- 断线恢复：`rivo a2a recover <peer-id> <task-id>`；先 `GetTask`/订阅既有 task，禁止未知提交状态下重发。
- 取消：`rivo a2a cancel <peer-id> <task-id> --yes`；双方最终只允许一个 terminal 状态。
- Client 撤销：`rivo a2a revoke <peer-id> --yes`。Server 侧无法在线通知时，必须在 Server 本机执行 `rivo a2a pairings revoke <peer-id> --config owner-a2a-server.toml`。
- Card、SPKI、endpoint、协议版本或技能变化必须进入 `suspended`，重新发现和配对后才能恢复。

## 7. 验收矩阵与证据

每次记录提交号、OS、binary version、双方私网地址、transport、discovery、task 状态序列、cleanup 状态和失败原因。

| 验收项 | 通过条件 |
| --- | --- |
| 配置与启动 | 空目录创建 workspace/config，`serve` 成功，非法 bind/schema 被拒绝 |
| mTLS 配对 | SAS、SPKI、IP SAN、ClientAuth EKU、PeerGrant 均核对并持久化 |
| 研究业务 | 5 分钟内返回 `claims[]`、`sources[]`、`limitations[]`，引用可追溯 |
| 流式与恢复 | `SUBMITTED -> WORKING -> COMPLETED`；断线后 recover 不创建第二 task |
| 取消/审批 | 取消仅一个终态；高风险操作只在 Server owner TTY 审批，远端不能裁决 |
| 信任变化 | Card/SPKI/endpoint 变化产生 `suspended` 和稳定错误码 |
| 跨平台 | Windows↔Debian 双向 mTLS smoke；PSK 证据单独标注，不替代 mTLS |
| 发现 | mDNS 成功才记录 `mdns`；否则明确 `explicit-endpoint` |
| 脱敏与清理 | 日志/SQLite/诊断无 token、私钥、Header、正文和绝对 workspace；测试 keychain/临时目录清理 |

故障优先检查 `a2a_server_bind_rejected`、`a2a_auth_required`、`a2a_peer_not_trusted`、`a2a_peer_suspended`、`a2a_certificate_changed`、`a2a_agent_card_changed`、`a2a_transport_failure`；禁止以明文 HTTP 或忽略 SPKI 作为绕过。
