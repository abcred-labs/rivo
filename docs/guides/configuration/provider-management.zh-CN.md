# Provider 管理

- 适用版本：V1 Windows 11 x64 本地预览。
- 接口边界：接口和确认语义以 [CLI 契约](../../architecture/cli.zh-CN.md)为准。

Provider 是一个独立连接，拥有 `base_url`、认证引用、enabled、priority 和 discovery。它不携带协议，也不携带默认 Model；相同 URL 的两个 Provider 仍然是不同对象。

请求使用哪个协议只由 Model 决定：路由取该模型有效 `supported_protocols` 列表的第一项。因此同一个 Provider（例如一个同时提供 Chat Completions、Responses 与 Messages 的网关）可以同时服务 `gpt-6-sol`（Responses）与 `claude-sonnet-5-5`（Messages），无需按协议重复登记。调整模型协议见[模型管理](model-management.zh-CN.md)。

## 查看 Provider

```powershell
rivo config provider list
rivo config provider show <provider-id>
```

`list` 与 `show` 输出 Provider 的地址、后备 Credential 引用（`credential:<name>` 或 `none`）和脱敏 discovery 状态；不输出认证正文。

## 创建 Provider

```powershell
rivo config provider set <provider-id> `
  --base-url <base-url> `
  --credential-name <credential-name>
```

`--credential-name` 是可选的本机后备引用，但指定的名称必须已经存在于独立 Credential 存储中。环境变量名不保存到 Provider；需要外部注入时，在实际执行中使用 `rivo run --api-key-env <environment-variable-name>`。

`base_url` 是协议请求路径前缀，不要填写完整的 `/chat/completions`、`/responses` 或 `/v1/messages` URL。推荐以 `/v1` 结尾，三种协议分别请求 `<base>/chat/completions`、`<base>/responses` 与 `<base>/messages`；若不以 `/v1` 结尾，Messages 请求 `<base>/v1/messages`，OpenAI 两种协议仍直接拼接。若同一发布方的不同协议位于不同路径前缀，请拆分为不同 Provider，并在模型上禁用该入口不提供的协议。

`--protocol` 已移除；传入时命令会失败并提示改用 `config model set <model-id> --enable-protocol/--disable-protocol/--protocols`。

`provider set` 默认只保存本地连接配置，不联网。保存后 discovery 会失效，命令会提示执行 `rivo config provider models refresh <provider-id>`。如需显式保存后立即刷新，可追加 `--refresh`；该选项不会在失败时回滚已保存的 Provider。

更新地址或认证引用时，命令会生成影响计划。核对 `affected_models` 与 `impact_complete` 后，以相同参数加 `--confirm <confirmation-token>` 提交；revision 变化后需要重新取得计划。连接字段变更会使 discovery 失效；调整模型协议不会。

## 删除 Provider

```powershell
rivo config provider remove <provider-id>
```

删除会显示影响计划并要求确认。它只清除指向该 Provider 的默认选择，使后续选择回到 Auto；不会删除 Model、Vendor 或 Credential。

## 后续操作

创建或更新 Provider 后，必须[刷新模型目录](provider-model-refresh.zh-CN.md)。仅 `config validate` 通过不表示远程认证和模型发现成功。

[返回配置索引](README.zh-CN.md) | [Credential 管理](credential-management.zh-CN.md) | [模型路由](model-routing.zh-CN.md)

---

[English](provider-management.md)
