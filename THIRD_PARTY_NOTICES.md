# Third-party notices

Rivo 自身使用 MIT License；依赖和协议输入保留各自授权。

- [完整依赖清单](licenses/dependencies.json)记录锁定版本、许可证表达式、选用分支、作者和精确版本源码下载地址。
- [许可证、版权及内嵌原生库声明](licenses/THIRD_PARTY_LICENSES.txt)包含依赖许可证正文，以及 AWS-LC、ring、SQLite 的上游声明。
- 声明覆盖五个发布目标的工作区合集，包含构建和开发依赖；列入清单不表示每个平台二进制均链接该依赖。MPL-2.0 依赖未经修改，清单中的对应源码下载地址提供其完整源码。

仓库内冻结的 A2A 协议输入包含上游 A2A 的 Apache License 2.0 文本，并记录了上游提交和校验值；BIP39 英文词表的来源记录在协议清单中。重新分发源码或构件时请保留这些文件和对应的上游声明。

发布归档在 `share/rivo/licenses/` 中携带上述文件。维护者更新依赖后必须重新生成并审核声明：

```text
cargo install cargo-about --version 0.9.2 --locked
python scripts/generate_licenses.py
python scripts/generate_licenses.py --check
cargo install cargo-audit --version 0.22.2 --locked
cargo audit --file runtime/Cargo.lock --deny warnings
```

`runtime/about.toml` 维护允许的许可证分支。新增依赖若不能满足许可证要求，扫描必须失败；不通过忽略漏洞或许可证错误放行发布。
