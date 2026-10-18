# Vendor 管理

Vendor 表示 canonical Model 所属的发布方命名空间。它是本地目录对象，不是网络
连接，也不是 Credential。创建自定义 Model 前，必须先有对应 Vendor。

## 查看目录

```text
rivo config vendor list
rivo config vendor list --format table
rivo config vendor list --format json
rivo config vendor show <vendor-id>
```

`list` 和 `show` 是离线只读查询。内置条目会显示来源；本地条目和覆盖保存在用户
配置中。

## 创建或更新 Vendor

```text
rivo config vendor set <vendor-id> --display-name "My Gateway"
```

Vendor ID 是稳定的目录标识，建议使用简短 ASCII ID；display name 应适合在 TUI
选择器中显示。该命令只改本地配置，不访问 Provider。

## 自定义 Model 流程

先创建 Vendor，再登记完整 Model ID 及有证据支持的协议：

```text
rivo config vendor set acme --display-name "Acme AI"
rivo config model set acme-chat `
  --vendor acme `
  --display-name "Acme Chat" `
  --protocols openai_chat_completions `
  --priority 100
```

协议列表不能为空、不能重复，并按偏好排序。只允许
`openai_chat_completions`、`openai_responses` 和 `claude_messages`。Provider
discovery 不会推断协议，也不会自动创建 Model。接着配置 Provider 并显式刷新，详见
[Model 管理](model-management.zh-CN.md)和[Provider 模型刷新](provider-model-refresh.zh-CN.md)。

## 删除 Vendor

```text
rivo config vendor remove <vendor-id>
rivo config vendor remove <vendor-id> --yes
```

省略 `--yes` 时会先显示影响计划并要求交互确认，计划可能包含受影响的 Model、Provider
和默认选择。删除 Vendor 不会删除 Credential 密钥。非交互调用必须显式传入 `--yes`。

## 命名和路由规则

Vendor、Model、Provider ID 都按完整字符串精确比较，区分大小写，不支持别名或前缀
匹配。一个 Vendor 可以拥有多个 Model；一个 Provider 在 discovery 和协议配置允许时
可以服务多个 Vendor 的 Model。

[配置索引](README.zh-CN.md) | [Model 管理](model-management.zh-CN.md) |
[Provider 管理](provider-management.zh-CN.md)

---

[English](vendor-management.md)
