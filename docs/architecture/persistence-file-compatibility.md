# 用户持久化文件兼容与自动升级契约

- 状态：`in_progress`
- 范围：V1 Windows 11 x64 的 `config.toml`、`credentials.toml` 与 `instances.json`。
- 归属：配置格式演进的统一项目规范。模型路由字段见
  [Provider 地址归属与模型协议契约](provider-endpoint-ownership.md)，实例控制协议见
  [A2A 双角色互操作](a2a-interoperability.md)。

## 目标与边界

Rivo 程序版本与用户持久化文件格式版本彼此独立。每个已发布版本都必须能读取其支持范围内的历史用户文件，并在首次需要该文件的写入型启动前自动、无损地升级为当前格式。用户不得因升级 Rivo 而被要求手工重建本规范涵盖的文件。

本契约只管理本机用户文件格式，不改变 SQLite 会话 schema、A2A wire、app-server
payload、模型 canonical ID 或 Provider discovery 的语义。历史模型配置必须通过明确、可测试的迁移规则变为当前模型结构；迁移不得猜测 Model ID、Credential 正文、Provider 地址或 discovery 成功状态。

本契约从 Rivo 品牌和数据目录启用后开始适用。MAgents 到 Rivo 的产品改名是一次明确的 breaking change，不构成受支持的 Rivo 历史文件格式，也不改变旧协议、命令、路径或环境变量的拒绝边界。

“兼容”不表示接受任意输入：

- 已发布且仍受支持的旧格式必须自动升级。
- 损坏、无法保留语义、违反安全约束的文件必须明确失败，不得删除、覆盖或部分修复。
- 文件格式版本高于当前程序支持的未来版本必须明确失败且零写入；不得降级、丢弃未知数据或以默认值启动。
- 同一版本升级重复执行必须幂等；升级完成后的语义和对象标识必须保持不变。

`FEAT-032` 已实现本规范定义的 format 0/3 到当前格式的升级入口和回归测试；候选 `50ad3ea` 已完成其质量门和 Windows preview 证据。未列为支持格式的历史文件仍必须明确拒绝，而不能将其误认为可迁移输入。

## 受管文件与规范版本

| 文件 | 默认位置 | 版本字段 | 当前规范格式 | 持久化责任 |
| --- | --- | --- | --- | --- |
| `config.toml` | `%APPDATA%\rivo\config.toml`，或 `RIVO_CONFIG` 指定的位置 | 顶层 `version` | schema 6 | 用户设置、Vendor/Model/Provider、逐模型有序协议列表（Provider 不含协议）、MCP/A2A 与公开引用；禁止保存秘密正文。`RIVO_CONFIG_CONTENT` 注入整份文档时按只读快照使用，不读写该文件，也不参与自动升级。 |
| `credentials.toml` | 与主配置同目录 | 顶层 `format_version` | format 1 | 命名 Credential 的加密载荷；Provider 仅引用名称。 |
| `instances.json` | `%LOCALAPPDATA%\rivo\instances\instances.json` | 顶层 `format_version` | format 1 | 可控制本机实例的注册信息，不保存会话历史或密钥。 |

`instances.json` 的未版本化顶层数组是历史 format 0。format 1 的规范形状为：

```json
{
  "format_version": 1,
  "instances": []
}
```

每个格式升级只能引入一个递增的格式版本，并提供从每个仍受支持前序版本到新版本的确定性迁移步骤。不得以 Rivo 的产品版本号、SQLite schema 版本、协议版本或发布时间替代上述文件版本字段。

`config.toml` 支持两类历史表示：缺失 `version` 或 `version = 3` 的扁平单 Provider 配置，以及字段均符合历史结构的嵌套 schema 3 路由配置。schema 4 仅在本地 Model 已显式含 `supported_protocols` 时可升级；旧本地 Model 缺少该字段无法安全推断协议，必须失败且零写入。嵌套 schema 3 的唯一删除规则是顶层 `[supervision]` 恰为 `enabled = false`：中心监督实现已不属于当前 schema，删除该禁用段不改变运行语义。启用 supervision、包含额外 supervision 字段或任何其他无法由 schema 6 表示的输入必须失败且零写入。

schema 4、schema 5 与嵌套 schema 3 升级到 schema 6 时移除每个 Provider 的 `protocol`：协议改由 Model 有序 `supported_protocols` 决定，Provider 协议不再是可表达的用户意图，因此按显式删除规则处理而非静默丢弃。仅当旧 discovery `config_fingerprint` 与旧连接身份（ID、协议、base_url、认证引用）一致时，按新指纹（ID、base_url、认证引用）重新绑定，否则保持失效。扁平单 Provider 配置的协议选择映射为该内置 Model 的本地协议顺序覆盖（仅当该协议属于内置列表且不是首项）。升级同样原子、幂等，只读查询不写盘。

## 强制升级流程

所有三个文件的写型访问入口必须使用同一套升级流程；纯查询入口可以兼容读取已支持的旧格式，但不得因查询而改写用户文件：

1. 在取得该文件的排他锁后读取原始字节，识别格式版本，并在内存中执行完整迁移链和当前 schema 校验。
2. 仅在迁移后的语义完整、交叉引用有效且当前 schema 校验通过时，使用同目录临时文件、`sync_all` 和原子替换提交新内容。替换前必须再次确认原始字节未变化。
3. 迁移前后的稳定标识、Credential 名称、Provider 引用、实例 ID 和实例 endpoint 必须保持不变。不得发起网络请求、刷新 discovery、启动实例、改变 policy，或产生新的默认模型选择。
4. 迁移失败、锁冲突、断电恢复或外部并发修改时，保留可读取的原文件，报告稳定且脱敏的错误，并在下次启动安全重试；不得留下部分序列化的目标文件。

`config.toml` 与 `credentials.toml` 存在引用关系。任何重命名或拆分 Credential 的迁移必须在一个可恢复事务中同时验证并提交两个文件；普通格式升级不得改名。`instances.json` 的升级独立执行，但不得通过清空注册项来规避解析或版本错误。

升级过程不得记录配置正文、Credential 正文、明文密钥、认证 header 或实例工作区路径。Credential 迁移只处理原始密文载荷；若历史格式含明文，原文件只能在成功原子替换前保留，之后不得生成包含该明文的备份、日志或诊断包。

## 字段演进与兼容规则

- 新字段必须定义缺省值和从每个历史格式的来源；删除字段必须定义保留、映射或明确不可支持的判定，不能静默丢弃用户意图。
- 对象 ID、Credential 名称、Provider 地址和协议枚举不得通过 trim、大小写折叠、alias 或随机重建“修复”。
- 当前 schema 的安全关键字段继续严格校验。迁移仅将已知历史表示转换为当前表示，不允许旧 Profile、Binding、Endpoint 或未知字段绕过当前 Provider 地址归属和精确模型匹配规则。
- 格式升级完成后，当前程序写出的文件必须只使用当前规范格式；旧程序遇到更高版本必须停止写入并提示需要较新 Rivo，而不是反向改写。
- 可选扩展必须拥有命名空间、版本和保留规则。没有此约定的未知字段不能静默删除；若无法保留，必须在写入前失败。

## 可恢复性与可观察性

实现可以以不含秘密的方式记录文件种类、旧/新格式版本、迁移链、程序版本、时间和结果。失败记录只能包含稳定错误码和路径类别，不包含文件正文或秘密。

实施可以保留受访问控制保护的原始备份和恢复 journal；其生命周期、命名和清理规则必须在实现 Feature 中登记并测试。恢复材料不得提交到仓库、不得被诊断包收集，且不得降低 Credential 文件的保密性。仅为了升级失败恢复而创建的材料不构成用户配置来源。

## 实施与验收门

任何变更 `version`、`format_version`、受管文件结构或默认序列化逻辑的提交，必须同时具备：

1. 每个已支持旧格式的 fixture，以及升级后与当前格式 fixture 的语义等价断言。
2. 自动升级、重复启动幂等、原子写失败、进程中断恢复、并发外部修改、未来版本零写入和损坏输入零写入测试。
3. `config.toml` 与 `credentials.toml` 的引用完整性、加密载荷不泄漏，以及 `instances.json` 全量实例保留测试。
4. CLI、TUI 与 app-server 共用同一迁移入口的集成测试；无网络、无实例启动、无 discovery 刷新的副作用断言。
5. 面向用户的迁移说明、稳定错误码和同一候选的质量门证据。

在这些条件满足前，不得提高受管文件格式版本，也不得把“自动升级”标记为 `completed`。
