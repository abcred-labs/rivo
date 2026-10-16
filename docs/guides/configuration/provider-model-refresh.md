# 刷新 Provider 模型目录

- 适用版本：V1 Windows 11 x64 本地预览。
- 功能状态：`in_progress`；刷新和可用性要求以[地址归属契约](../../architecture/provider-endpoint-ownership.md)为准。

刷新是唯一会请求 Provider 模型目录的配置命令。它执行认证 discovery，更新该 Provider 的 `discovery`；不会创建 Vendor/Model，也不会修改默认模型选择。

## 执行刷新

```powershell
rivo config provider models remote <provider-id>
rivo config provider models refresh <provider-id>
rivo config provider models list <provider-id>
rivo config provider show <provider-id>
```

`models remote` 仅请求并逐行显示远程返回的完整 model ID，不检查本地 Model、Vendor 或协议能力，也不修改配置或 discovery 缓存。需要把远程结果用于本地路由时，仍须显式执行 `models refresh`；该命令会按本地目录筛选后保存受管 discovery。

完成后，确认 discovery 中包含所需 canonical Model 的完整 ID。

## 可用于运行的条件

Provider 与 Model 同时满足以下条件才可以路由：

1. Provider 为 enabled。
2. 最近一次 refresh 成功且 `supported`。
3. 当前配置指纹与刷新时一致。
4. discovery 含有与 Model 精确一致的完整 ID。
5. Model 已启用且至少有一个 `supported_protocols`；请求协议取其第一项，与 Provider 无关。

因此 discovery 返回的任意模型 ID 只是该入口的观察结果，不会自动登记 Model 或推断协议；不在本地目录中的 ID 不会出现在可用 Provider、Auto 候选或实际路由中。discovery 先以 Bearer 认证请求 `/v1/models`，仅在 401/403 时以 `x-api-key` + `anthropic-version` 重试一次。

`base_url` 或认证引用变化会立即使 discovery 失效；调整模型协议不会。priority 变化不代表重新认证。刷新时间仅用于显示，目前没有自动 TTL 或后台刷新。

## 刷新失败

失败会保存脱敏错误并撤销旧可用性，不能保留旧成功缓存继续运行。修复 Provider 的认证或地址后重新刷新；不要手动写入成功 discovery 缓存。

目录 `404` 或 `unsupported` 表明发现接口不可用或不受支持，不等同于推理请求端点故障，但该 Provider 仍不可绕过 discovery 进入路由。`401/403` 应检查唯一认证引用和其权限。

[返回配置索引](README.md) | [Provider 管理](provider-management.md) | [配置故障排查](troubleshooting.md)
