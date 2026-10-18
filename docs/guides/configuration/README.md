# Configuration guides

Rivo separates four objects: Vendor (publisher namespace), Model (exact canonical
model ID and preferred protocol list), Provider (base URL, priority and discovery),
and Credential (local authentication reference). Configure them in that order, then
refresh discovery before running a task.

## Recommended first-time flow

```text
rivo config path
rivo config credentials set provider-key
rivo config vendor list
rivo config model list --format table
rivo config provider set gateway --base-url https://api.example.com/v1 \
  --credential-name provider-key
rivo config provider models refresh gateway
rivo config route --vendor <vendor-id> --model <exact-model-id> \
  --provider-id gateway --format table
rivo tui
```

The credential command masks interactive input. For automation, use
`rivo config credentials set NAME --from-env ENV`; never put the secret in a command
argument, TOML file, log or report.

## Guides

- [Vendor management](vendor-management.md)
- [Credential management](credential-management.md)
- [Provider management](provider-management.md)
- [Model management](model-management.md)
- [Model routing](model-routing.md)
- [Provider model refresh](provider-model-refresh.md)
- [Troubleshooting](troubleshooting.md)

All queries (`list`, `show`, `validate`, and `route`) are offline and read-only.
Only `provider models remote` and `provider models refresh` contact a Provider;
`refresh` is the only command that updates the managed discovery cache.

Every query list/show command accepts `--format text`, `--format table`, or
`--format json`. The configured default can be changed with:

```text
rivo config set query_format json
rivo config unset query_format
```

## Language versions

- [简体中文配置指南](README.zh-CN.md)
