# Credential 管理

- 适用版本：V1 Windows 11 x64 本地预览。
- 接口边界：模型路由见[地址归属契约](../../architecture/provider-endpoint-ownership.md)。

Credential 是独立的本地认证存储。它不会显示 API Key 正文，也不应将密钥写入命令行、脚本、`config.toml`、日志或仓库。

## 创建与检查

```powershell
rivo config credentials set <credential-name>
rivo config credentials show
```

创建时按提示进行两次隐藏输入。名称只可使用 ASCII 字母、数字、`_` 和 `-`。`show` 只显示名称和保存状态。

## 选择认证方式

Provider 认证引用只能二选一：

| 方式 | 使用场景 | Provider 参数 |
| --- | --- | --- |
| Credential | 将凭据存为本机独立引用。 | `--credential-name <credential-name>` |
| 环境变量 | 由外部进程环境安全提供实际值。 | `rivo run --api-key-env <environment-variable-name>` |

运行时非空环境变量优先；变量缺失、空白或不可读取时回退 Provider 引用的 Credential。未配置 Credential 引用时以无认证方式调用；已配置但不可读取时明确失败。Provider 只保存 Credential 名称，不保存环境变量名称或认证正文。

## 删除与迁移

```powershell
rivo config credentials migrate
rivo config credentials unset <credential-name>
```

Credential 仍被 Provider 引用时不能删除；请先修正或移除引用。删除 Provider 不会自动删除 Credential。`credentials migrate` 仅处理独立凭据的加密存储，不迁移模型配置或旧会话。Credential 缺失、损坏或不可解密时，Provider set、refresh 和运行时会给出对应的脱敏错误，不会把 API key 写入 `config.toml`。

[返回配置索引](README.md) | [Provider 管理](provider-management.md)
