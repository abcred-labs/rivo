# Rivo Browser Extension (Chrome / Edge)

这是 Manifest V3 的开发者加载扩展。Chrome 和 Edge 共用代码，通过
Native Messaging 与 `rivo` 的浏览器桥接进程通信。

## 开发者加载

1. 在 `chrome://extensions` 或 `edge://extensions` 开启“开发者模式”。
2. 选择“加载已解压的扩展”，指向本目录。
3. 在 `chrome.storage.local` 中设置可见的 `profile_key`（默认使用扩展安装 ID）。
4. 使用 `install-native-host.ps1` 注册对应的 Native Messaging Host。Manifest 已固定开发者公钥，默认 Extension ID 为
   `adnanfbbgelpelombhdjflmcggepbcee`；如果你修改了 Manifest 的 `key`，再显式传入新的 ID。

独立 smoke 不依赖 Codex 浏览器连接器。先构建 Rivo，再使用仓库脚本启动隔离的
Chrome/Edge Profile、临时 HTTP 页面和扩展，并由 Rivo 自己的 Rust 客户端执行列表、
导航、快照、点击和输入：

```powershell
cd <rivo-repository>
.\scripts\browser_extension_smoke.ps1 -Browser edge -Build
.\scripts\browser_extension_smoke.ps1 -Browser chrome
```

如果系统 Chrome 禁止命令行加载开发者扩展，可将 `-BrowserExecutable` 指向 Chromium 或
Chrome for Testing 的 `chrome.exe`：

```powershell
.\scripts\browser_extension_smoke.ps1 -Browser chrome `
  -BrowserExecutable C:\tools\chrome-for-testing\chrome-win64\chrome.exe
```

脚本只使用浏览器自身的 Native Messaging 能力和本机 Named Pipe，不读取 Codex
连接器状态，也不需要 CDP 端口。

Rivo 运行时可通过配置选择 Host 的本机命名管道和实例：

```toml
[browser]
  extension_pipe = "\\\\.\\pipe\\rivo-browser-extension-edge"
  extension_instance = { browser_family = "edge", extension_id = "", installation_id = "", profile_key = "", boot_id = "" }
```

也可使用 `RIVO_BROWSER_EXTENSION_PIPE` 与 JSON 格式的
`RIVO_BROWSER_EXTENSION_INSTANCE` 临时覆盖。没有扩展配置时继续使用 CDP。

协议版本为 `rivo-browser-extension-v1`。每个扩展安装使用独立
`installation_id` 和 `boot_id`，Rivo 通过二者区分多个浏览器和 Profile。
同一 Chrome 或 Edge family 的多个 Profile 可以共享 family pipe；Rivo 会按握手身份选择目标
实例，断开错误连接不会终止扩展 Host。

---

[English](README.md)
