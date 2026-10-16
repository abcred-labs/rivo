# Agent 媒体契约

媒体能力属于 Agent 普通工具循环，不属于独立 CLI 命令。模型可以选择 `generate_image` 或 `edit_image`，但 Rivo 在工具执行前重新校验能力、资产归属、限制和 policy。

## 公共对象

- `MediaOperation`：`generate` 或 `edit`。
- `MediaCapability`：当前冻结路由可用的操作和输入/输出字节上限。默认不支持任何媒体操作。
- `MediaRequest`：操作、提示词、受限尺寸/格式和已有 `MediaRef`。它不包含路径、Base64、认证信息或任意 URL。
- `MediaRef`：`session_id`、`asset_id`、MIME 和可选尺寸。历史只保存此引用。
- `MediaDelivery`：本地文件路径或短时签名 URL，仅作为当前结果交付，不进入持久化历史。
- `MediaResult`：`MediaRef` 与 `MediaDelivery`。只有 `MediaReady` 才表示媒体已经落地。

`edit_image` 首期只允许一个当前 Session 资产。工具 schema 和执行器同时拒绝路径、Base64、任意 URL、跨 Session 资产、未知 MIME、超限尺寸和过大的提示词。没有资产时，Agent 必须要求用户先导入或生成图片。

## 路由与 policy

能力判断使用当前 turn 已冻结的 Vendor、canonical Model、Provider、路由 protocol（Model 首选协议）和已验证能力表。能力不匹配返回结构化 `unsupported_capability` 语义，不重写模型 ID、不换 Provider、不 failover。媒体操作属于可能收费和外发数据的外部调用：strict 拒绝，medium/full_control 要求确认；取消不会调用 Provider。

## 资产生命周期

Windows preview 的本地实现写入：

```text
RIVO_DATA_DIR/tmp/media/<session-id>/<asset-id>.<ext>
RIVO_DATA_DIR/tmp/media/<session-id>/index.json
```

索引与 SQLite schema 17 分离，记录资产引用、受控相对文件名、创建时间和 TTL。恢复只读取索引中的 `MediaRef`；媒体字节不进 SQLite。显式删除/清空 Session 时删除对应目录；普通进程关闭保留资产以支持恢复，并由启动、访问和 TTL 清理回收。任何索引损坏或资产缺失都 fail closed。

S3/OSS 适配尚未开启；未来只能使用私有对象和短时签名 URL，原始对象地址、Base64、认证信息和签名参数不得进入历史、日志或 Agent 上下文。

## Provider 边界



## 事件顺序

一次成功的媒体工具调用至少按以下顺序产生：

```text
ToolRequested -> [ConfirmationRequired] -> MediaStarted -> MediaReady -> ToolObservation -> FinalAnswer
```

失败使用 `MediaFailed`，并带结构化失败语义；Agent 不得仅凭文字回答声称图片已生成。
