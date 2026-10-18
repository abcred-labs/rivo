# Model 管理

- 适用版本：V1 Windows 11 x64 本地预览。
- 接口边界：canonical Model 与自动路由的规则以[地址归属契约](../../architecture/provider-endpoint-ownership.zh-CN.md)为准。

Model 是模型目录身份，不保存 Provider 地址、认证或人工映射。canonical Model ID 区分大小写，且前后缀必须与 Provider discovery 返回的完整 ID 精确一致。

Model 同时决定请求协议：`supported_protocols` 是按偏好排序的列表，路由始终使用第一项，与 Provider 无关。

## 查看目录

```powershell
rivo config model list
rivo config model show <canonical-model-id>
```

内置 Model 的 `vendor` 与 `display_name` 只读；本地可以覆盖其协议列表与 priority，`list` 中来源显示为 `BuiltinOverride`。

## 调整模型协议

内置目录提供默认协议顺序：OpenAI 多协议模型 Responses 优先，DeepSeek、GLM 等多协议模型 Chat Completions 优先，Anthropic 模型仅 Messages。需要时可对内置或自定义 Model 增加、禁用或重排协议：

```powershell
# 追加协议（已存在则忽略）
rivo config model set <canonical-model-id> --enable-protocol claude_messages
# 禁用协议；不能禁用最后一个
rivo config model set <canonical-model-id> --disable-protocol openai_responses,claude_messages
# 整体替换并指定偏好顺序
rivo config model set <canonical-model-id> --protocols claude_messages,openai_chat_completions
```

三种参数都基于当前有效列表计算，并把完整列表写入本地 `[models.<id>]` 覆盖；省略 `--vendor`、`--display-name` 时沿用内置或已有值。覆盖对所有 Provider 生效，例如某套餐入口只提供 Chat Completions 时，可在该模型上禁用其他协议。调整协议不会使 Provider discovery 失效。

## 创建或维护自定义 Model

仅在目录中不存在目标 ID 时创建；协议列表必须按该具体 Model ID 的证据填写：

```powershell
rivo config model set <canonical-model-id> --vendor <vendor-id> --display-name <model-display-name> --protocols openai_chat_completions,openai_responses --priority 100
```

`--vendor` 必须引用有效 Vendor。`--protocols` 是非空、无重复的逗号分隔列表，顺序即偏好，只能使用 `openai_chat_completions`、`openai_responses`、`claude_messages`；不能按 Vendor 或模型名前缀推断。priority 范围为 `0..=1000`；未显式选择 Model 时，系统按 priority 降序、Model ID 升序选择，不会因该 Model 暂无可用 Provider 自动改选其他 Model。

## 确认可用 Provider

```powershell
rivo config model providers <canonical-model-id>
```

结果来自已刷新 Provider 的精确 ID 匹配。没有 alias、请求 ID 重写或人工 Provider mapping。

## 删除前检查

```powershell
rivo config model remove <canonical-model-id>
```

先阅读影响计划再确认。删除不能留下默认选择或目录引用不完整的配置。

[返回配置索引](README.zh-CN.md) | [刷新 Provider 模型目录](provider-model-refresh.zh-CN.md) | [模型路由](model-routing.zh-CN.md)

---

[English](model-management.md)
