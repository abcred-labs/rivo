# 模型路由

- 适用版本：V1 Windows 11 x64 本地预览。
- 接口边界：路由规则见[地址归属契约](../../architecture/provider-endpoint-ownership.zh-CN.md)。

路由选择一个 Vendor、一个 canonical Model 和一个 Provider。运行请求使用被选 Model 的首选协议，以及被选 Provider 的地址和认证引用，不能用运行时 URL 或认证参数覆盖。

## 设置默认选择

```powershell
rivo config set vendor <vendor-id>
rivo config set model <canonical-model-id>
rivo config set provider_id <provider-id>
```

取消默认显式 Provider 后回到 Auto：

```powershell
rivo config unset provider_id
```

也可在单次运行时使用 `--vendor`、`--model`、`--provider-id`；`--auto-provider` 与 `--provider-id` 互斥。

## 验证路由

```powershell
rivo config model providers <canonical-model-id>
rivo config route --vendor <vendor-id> --model <canonical-model-id> --provider-id <provider-id>
```

这些查询离线且只读。实际运行前，先[刷新 Provider 模型目录](provider-model-refresh.zh-CN.md)。

## Auto 与显式 Provider

- 未选 Model 时，按 Model priority 降序、Model ID 升序选择；选定后不会因为无可用 Provider 而更换模型。
- Auto 按 Provider priority 降序、Provider ID 升序选择可用 Provider。
- 指定显式 Provider 后，它不可用就立即拒绝，不回退到其他 Provider。
- 精确 ID 不匹配、未刷新、最近刷新失败、disabled、unsupported 或配置指纹不符都会拒绝。
- 当前没有运行期故障切换；后续 turn 会重新解析路由。

[返回配置索引](README.zh-CN.md) | [Provider 管理](provider-management.zh-CN.md)

---

[English](model-routing.md)
