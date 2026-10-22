# A2A 局域网发现与配对手册

> 适用范围：`FEAT-021-a2a-client` 和 `FEAT-022-a2a-server`
> 状态：`in_progress`。本机 loopback mTLS 是默认阶段 Gate；本手册的 mTLS 与显式 PSK transport 都只用于受控跨平台互操作 smoke，不能替代本机 Gate。

本手册说明受控局域网中的候选发现、双端配对、信任轮换和撤销流程。它不适用于公网部署、跨组织目录或无人值守机器。

## PSK 互操作模式

`rivo-a2a-psk-v1` 是独立的显式 transport，不是明文 HTTP 或 mTLS 的静默降级。它仍使用 TLS，
并用一次性 32-byte PSK proof、首响应认证的临时 P-256 ECDH 与 HKDF 会话派生，以及 AEAD envelope 保护
A2A JSON-RPC、SSE 与控制正文。

- 仅使用 `--transport psk --secret-stdin` 启用；默认仍是 mTLS。
- secret 只能从 stdin 输入，不得进入参数、环境变量、TOML、SQLite、keychain、日志、诊断包或 shell history。
- PSK peer 没有持久客户端私钥，因此 trusted snapshot 不含 `identity_ref`；每个 CLI 进程都重新 bootstrap，Server 重启后旧会话不可复用。
- PSK Server TLS leaf 只在内存中，由同一 PSK 与私网 bind 确定性重建，使 SPKI pin 跨 Server 重启稳定；更换 PSK、endpoint 或 Card hash 必须重新配对。
- nonce 与 request ID 的重放记录只在 30 秒 bootstrap/session 窗口内有效；窗口结束后旧记录被淘汰，新的 bootstrap 仍须完成完整 proof。
- 仅允许私网 HTTPS bind 和显式 endpoint。mDNS 不可用时必须在证据中标为 `explicit-endpoint`。

```text
# Server：secret 从受控 stdin 输入，不要把它传给命令行。
rivo a2a serve research-lan --config <owner-a2a-server.toml> --transport psk --secret-stdin

# Client：显式固定 TLS SPKI 后从 stdin 输入同一 secret。
rivo a2a pair <local-label> --endpoint <https://private-ip:port/a2a> \
  --spki-sha256 <sha256> --transport psk --secret-stdin
```

PSK 失败会返回 `a2a_psk_invalid_secret`、`a2a_psk_transport`、`a2a_psk_session_required` 或
`a2a_psk_replay`，并且不得创建 task 或 trusted peer。Card/SPKI 变化仍会 suspend peer；不得按名称或 IP 自动恢复信任。

### Windows PowerShell reverse PSK smoke

Windows Server、Debian Client 的反向互操作使用
`scripts/a2a_psk_linux_windows_pair_probe.ps1`。这是受控 PSK 互操作证据，不替代
Windows/Linux/macOS 共用的本机 mTLS 阶段 Gate；发现方式固定记录为 `explicit-endpoint`。

前提：从提升权限的 Windows PowerShell 启动脚本，使其仅在运行期间为 Debian 私网地址创建临时
TCP 入站规则；Debian 上必须已存在当前源码原生构建的
`/tmp/rivo-a2a-src/runtime/target/release/rivo`。PSK 只经 stdin 传入，绝不保存到日志、报告、
参数、环境变量或临时文件。

```text
cd <rivo-repository>
.\scripts\a2a_psk_linux_windows_pair_probe.ps1 -OpenFirewall
```

成功输出必须包含：

```text
reverse_pair_probe=passed peer_id=<peer-id> transport=psk
reverse_lifecycle_probe=passed peer_id=<peer-id> task_id=<task-id> states=WORKING,RECOVERED_TWICE,CANCELED,REVOKED
```

脚本启动 Windows Server、准备 Debian Client 的隔离数据目录，并在退出时停止 Server、删除临时
firewall rule 和 Debian Client 数据。它使用 `RIVO_A2A_SMOKE_HOLD_MS=30000` 仅为 smoke 提供可取消
窗口；不得将该变量用于正常服务配置。

PowerShell 5.1 没有 `ProcessStartInfo.ArgumentList`，脚本必须使用 `ProcessStartInfo.Arguments`。
进入 SSH 命令的 host、远端二进制路径、bind、peer ID 和 task ID 必须先受限校验；错误匹配只使用
ASCII 稳定分类，例如 `a2a_` 与 `peer`，不得依赖中文 CLI 文案或非 ASCII 正则。

## 前置条件

- 两台机器位于同一受控局域网，且网络策略允许 mDNS/DNS-SD 与明确的私网 HTTPS 地址。
- Server 已由本机所有者创建服务 profile，并以明确 RFC1918/ULA 地址启动。
- 双方本机用户均可核对六词 SAS；不得在聊天、日志、工单或非受控通道传递私钥、claim token 或证书文件。

## 1. 启动 Server

```text
rivo a2a serve research-lan --config <owner-a2a-server.toml>
```

预期：

- Server 仅绑定 profile 中声明的私网地址。
- 最小 Agent Card 可被候选发现使用。
- task API、Extended Agent Card 和流式接口要求 mTLS。
- 启动输出只显示 profile、私网 endpoint、公开证书指纹和状态，不显示私钥、workspace 或凭据。

## 2. 发现候选

```text
rivo a2a discover
```

预期输出包含候选 ID、公开名称、endpoint、协议版本、SPKI 指纹、Agent Card hash 和发现时间。该命令不会：

- 创建 trusted peer。
- 下载或执行远端 Artifact。
- 启动任务、发送本地上下文或保存任何凭据。

发现到同名、同 endpoint 或不同指纹的多个候选时，必须分别展示，不能按名称自动合并。

## 3. 配对

配对使用 rivo-a2a-lan-v1 SAS + CSR 引导扩展，而非 A2A Task API。Client 在核验候选 Server
SPKI 后生成不可导出私钥和 CSR；Server 创建 10 分钟、单次使用、限速的 pending request。Client
显示 SAS、endpoint、SPKI、Card JCS hash 和请求 ID；Server 所在本机使用 pairings list、approve
或 reject 命令查看并裁决。

Server 本机用户必须在批准前核对相同 SAS、endpoint、SPKI、Card hash 与客户端公钥摘要。每个
profile 使用独立 keychain 私有 CA；Server TLS 身份独立且其证书 SAN 必须是 bind IP。批准后，
Client 用 CSR 私钥签名 claim proof，并连同一次性 claim token 经已固定 SPKI 的 TLS 通道领取证书链。
证书有效期一年，仅含 digitalSignature、clientAuth EKU、随机 serial 和不透明 peer ID URI SAN；
私钥/证书引用保存在 OS keychain。配对 endpoint 不创建 Task，也不接受 workspace、模型、MCP 或
policy 参数。

```text
# Client：label 只命名本机 keychain 身份，成功后输出的 peer_id 才是后续使用的远端身份。
rivo a2a pair <local-identity-label> --endpoint <https://private-ip:port/a2a> --spki-sha256 <sha256>

# Server 所在机器：在同一交互式本机用户会话中核对 CLI 显示的 request_id、SAS、endpoint、SPKI 与客户端公钥摘要后裁决。
rivo a2a pairings list --config <owner-a2a-server.toml>
rivo a2a pairings approve <request-id> --config <owner-a2a-server.toml>
# 或：rivo a2a pairings reject <request-id> --config <owner-a2a-server.toml>
```

Client 获取最小 Agent Card 和服务端证书后，校验 bind IP SAN 与 SPKI，生成 CSR 并显示 endpoint、
SPKI、Card JCS hash、公开技能、request ID 和六词 SAS。用户必须在 Server 所在机器核对相同 SAS，
再分别批准。`local-identity-label` 不会写入 trust snapshot，也不能指定 Server 分配的 `peer_id`；claim
响应中的不透明 `peer_id` 是后续 `inspect`、`revoke`、`recover` 和 `--a2a-peer` 的唯一输入。

成功后：

- Server 还保存 issuer、客户端证书指纹、有效期和各 ServiceProfile 的 grant；Task API 将拒绝
  issuer、EKU、指纹、有效期或 grant 不匹配的连接。

- Client 固定 endpoint、SPKI、Agent Card identity/hash 和允许技能。
- Server 登记该 Client 的 mTLS 身份及最小技能 grant。
- 私钥/证书保存在 OS keychain，配置仅保存 `identity_ref`。
- peer 状态变为 `trusted`。

SAS 不匹配、claim token 重放、CSR proof 无效、Card/SPKI 变化、10 分钟过期或用户拒绝时，不创建
peer grant，候选保持 `candidate` 或转为 `suspended`。

### 本机双进程检查

没有第二台机器时，可运行 `python scripts/a2a_loopback_smoke.py --rivo <rivo 路径>`：它在回环地址启动 PSK Server（仅由 `RIVO_A2A_LOCAL_TEST=1` 允许），再用独立的 Client 进程配对，检查正确 PSK 能配对并成为 Trusted，错误 PSK 或错误 SPKI pin 被拒绝且不创建 peer。加 `--client-rivo <旧版 rivo>` 可检查与其他构建的兼容性。它使用一次性数据目录和测试身份，不能替代真实跨主机验收。

## 4. 检查与使用

```text
rivo a2a peers
rivo a2a inspect <peer-id>
rivo run --a2a-peer <peer-id> --prompt "请委派一项研究任务"
```

`inspect` 只显示公开 endpoint、固定指纹、允许技能、状态、最后验证时间和脱敏审计摘要。普通 turn 只有在传入 `--a2a-peer` 后才会注册 `a2a_delegate`。

任务在网络中断后只能恢复已落入本机 SQLite 的 task 映射：该命令先 `GetTask`、再读取 SSE，并逐条持久化单调 revision，绝不重新发送业务委派。

```text
rivo a2a recover <peer-id> <task-id>
```

## 5. 轮换与重新批准

Server 证书、Agent Card identity/hash、endpoint、协议版本或技能安全声明变化后，Client 将 peer 标记为 `suspended`。用户必须重新执行发现和配对；不得以“同一名称”或“同一 IP”自动恢复信任。

## 6. 撤销

Client 撤销先在本机原子停用 peer、取消可取消任务并删除身份引用；仅在仍能使用旧 mTLS 身份时
尝试通知 Server。通知失败时，CLI 必须明确提示需要在 Server 所在机器撤销，不能显示为已双端撤销。

```text
rivo a2a revoke <peer-id>
```

撤销会删除本机身份引用、停止新任务、取消可取消的活动任务并写入审计。Server 删除或禁用 PeerGrant
后立即拒绝新 mTLS 连接、取消活动 task 并使等待审批失效，不维护 CRL。Client 无法在线通知 Server
时必须由 Server 本机执行撤销。

## 排障

| 现象 | 处理 |
| --- | --- |
| 未发现候选 | 检查双方是否在同一受控网段、mDNS 是否被网络策略阻断，以及 Server 是否显式启动。 |
| 指纹不一致 | 停止配对；核实 endpoint 和证书轮换，禁止忽略该提示。 |
| `a2a_peer_suspended` | 重新审阅 Card/证书变化后执行新的本机配对。 |
| mTLS 认证失败 | 确认 peer 未撤销、Client 身份仍在 keychain 中且 Server grant 未过期。 |
| 模型看不到委派工具 | 确认当前 turn 显式选择了 trusted peer，且策略未处于 `strict`。 |
| `remote_binary_missing` | 在 Debian 原生构建当前源码的 `rivo`，并确认脚本的 `-RemoteBinary` 指向该可执行文件。 |
| `firewall_rule_failed` | 用提升权限的 Windows PowerShell 运行 `-OpenFirewall`；不要将长期公网规则作为替代。 |
| `linux_client_timeout` | 检查临时入站规则、私网 bind、Debian 到 Windows 的 TCP 连通性和 SSH 路径；不要回退到明文 HTTP。 |
| Debian SQLite error 14 | 脚本必须先创建 `/tmp/rivo-psk-reverse-client/rivo`；删除旧目录后重新运行，不复用 PSK 会话。 |
| `ArgumentList` 属性不存在 | 运行环境是 Windows PowerShell 5.1；使用脚本中的 `ProcessStartInfo.Arguments` 兼容路径。 |
| PowerShell 提示字符串未终止 | 移除非 ASCII 错误正则，改用 `a2a_`/`peer` 等稳定 ASCII 错误分类后重新执行 parser 检查。 |

---

[English](a2a-lan-pairing.md)
