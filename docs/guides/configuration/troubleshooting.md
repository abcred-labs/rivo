# 配置故障排查

- 适用版本：V1 Windows 11 x64 本地预览。
- 功能状态：`in_progress`；schema 5 以[地址归属契约](../../architecture/provider-endpoint-ownership.md)为准，用户文件升级以[持久化文件兼容与自动升级契约](../../architecture/persistence-file-compatibility.md)为准。

## 症状与处理

| 症状 | 原因或检查点 | 处理 |
| --- | --- | --- |
| 配置升级失败 | 文件损坏、迁移后仍有未知/冲突字段，或文件来自未来版本。 | 保留原文件并查看脱敏错误；不要手工覆盖。已支持历史格式应在迁移器交付后自动升级，未来版本必须使用较新的 Rivo。 |
| Model 未出现在 Provider 列表 | 未刷新，或完整 ID 不匹配。 | 刷新 Provider；核对大小写和前后缀，不使用 alias。 |
| refresh 返回 `401/403` | Provider Credential 缺失、错误或权限不足。 | 检查 Provider 的 Credential 引用及其权限，再刷新；refresh 不读取 turn 的环境变量。 |
| provider set 拒绝 Credential | 指定的 Credential 名称不存在。 | 先执行 `rivo config credentials show`，再使用已有名称，或先创建 Credential。 |
| Credential 无法删除 | 仍有 Provider 引用该 Credential。 | 先更新或移除对应 Provider，再执行 `config credentials unset`。 |
| turn 认证失败或未使用预期 key | 运行时环境变量缺失、空白或不可读取。 | 使用 `rivo run --api-key-env <name>` 指定变量名；缺失时会回退 Provider Credential。若 env 已有值但服务返回 `401/403`，不会切换 Credential。 |
| refresh 返回 `404/unsupported` | 目录发现接口地址不支持。 | 核对 `base_url`；此结果不能绕过 discovery 直接运行。 |
| 运行返回 404 或协议错误 | 入口不提供该 Model 首选协议。 | 用 `config model set <id> --disable-protocol` 或 `--protocols` 调整模型协议顺序。 |
| 路由被拒绝 | Provider disabled、刷新失败、指纹不符或不含精确 Model ID。 | 用 `provider show` 查看脱敏状态；修复连接后重新刷新。 |
| `validate` 通过但仍不能运行 | `validate` 只验证本地结构。 | 执行 refresh，并用 `config route` 与最小真实请求验证。 |
| 更新/删除无法提交 | 影响计划未确认，或 revision 已变更。 | 重新生成计划，核对影响后使用新的确认信息。 |
| 旧配置或旧会话不能恢复 | 历史配置文件的迁移器尚未交付、文件无法安全迁移，或 SQLite 会话 schema 不受本规范覆盖。 | 对已支持历史 `config.toml`、`credentials.toml`、`instances.json` 使用自动升级；只有旧会话 schema 或无法安全迁移的文件按受控流程处理，并保留 Credential、记忆、MCP 和 A2A 数据。 |

## 最小检查序列

```powershell
rivo config validate
rivo config provider show <provider-id>
rivo config provider models refresh <provider-id>
rivo config model providers <canonical-model-id>
rivo config route --vendor <vendor-id> --model <canonical-model-id> --provider-id <provider-id>
```

避免将密钥粘贴到诊断输出、命令行或问题报告中。

[返回配置索引](README.md) | [刷新 Provider 模型目录](provider-model-refresh.md) | [二进制包运行手册](../../runbooks/binary-runtime.md)
