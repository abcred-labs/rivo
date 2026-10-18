# 配置指南

Rivo 将配置拆成四类对象：Vendor（发布方命名空间）、Model（完整 canonical ID 和
首选协议列表）、Provider（Base URL、priority 和 discovery）以及 Credential（本地
认证引用）。首次配置应按这个顺序完成，然后刷新 discovery 才能运行任务。

## 首次配置推荐流程

```text
rivo config path
rivo config credentials set provider-key
rivo config vendor list
rivo config model list --format table
rivo config provider set gateway --base-url https://api.example.com/v1 `
  --credential-name provider-key
rivo config provider models refresh gateway
rivo config route --vendor <vendor-id> --model <exact-model-id> `
  --provider-id gateway --format table
rivo tui
```

Credential 命令会遮罩交互输入。自动化使用
`rivo config credentials set NAME --from-env ENV`；不要把密钥放在命令参数、TOML、
日志或报告中。

## 详细指南

- [Vendor 管理](vendor-management.zh-CN.md)
- [Credential 管理](credential-management.zh-CN.md)
- [Provider 管理](provider-management.zh-CN.md)
- [Model 管理](model-management.zh-CN.md)
- [模型路由](model-routing.zh-CN.md)
- [Provider 模型刷新](provider-model-refresh.zh-CN.md)
- [配置故障排查](troubleshooting.zh-CN.md)

所有 `list`、`show`、`validate`、`route` 查询都离线且只读。只有
`provider models remote` 和 `provider models refresh` 会访问 Provider；其中 refresh
是唯一更新受管 discovery 缓存的命令。

所有查询类 list/show 命令支持 `--format text`、`--format table` 和 `--format json`。
可以设置默认格式：

```text
rivo config set query_format json
rivo config unset query_format
```

## English

- [English configuration guides](README.md)

---

[English](README.md)
