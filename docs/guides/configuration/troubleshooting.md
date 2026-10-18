# Configuration troubleshooting

| Symptom | What it means | Action |
| --- | --- | --- |
| `validate` passes but a run is rejected | `validate` checks local structure only. | Refresh the Provider, then run `config route` and a minimal request. |
| Model is missing from Provider results | Discovery is stale or the ID is not exact. | Refresh and compare case, prefix and suffix; do not use an alias. |
| Refresh returns 401/403 | Credential is missing, invalid or unauthorized. | Check the Provider Credential reference and permissions, then refresh. |
| Refresh returns 404/unsupported | The model directory endpoint is not supported at this base URL. | Correct `base_url` or use a Provider that supports discovery; do not bypass refresh. |
| `provider set` rejects a Credential | The named Credential does not exist. | Run `config credentials show` or create it first. |
| Credential cannot be removed | A Provider still references it. | Update or remove every referencing Provider. |
| Request uses the wrong protocol | The Model's first protocol is wrong for the gateway. | Change `--protocols` or disable the unsupported protocol, then retry. |
| Route has no Provider | The Provider is disabled, unrefreshed, stale, or lacks the exact Model ID. | Inspect `provider show`, fix the connection and refresh. |
| Update asks for confirmation | A URL or Credential change affects a route. | Review the printed plan and repeat the same command with its fresh `--confirm` token. |
| TUI cannot start | stdin/stdout is not an interactive terminal. | Run it in a local terminal; use `rivo run --format jsonl` for automation. |

## Failure handling

Treat every rejected route, authentication response, discovery error, and TUI startup failure as actionable diagnostics. Preserve the redacted error summary, fix the named configuration or terminal condition, then repeat validation and refresh; never paste credentials into an issue report.

## Minimal diagnostic sequence

```text
rivo config validate
rivo config provider show <provider-id>
rivo config provider models refresh <provider-id>
rivo config model providers <canonical-model-id>
rivo config route --vendor <vendor-id> --model <canonical-model-id> --provider-id <provider-id>
```

Keep API keys out of diagnostics, shell history and issue reports.

[Configuration index](README.md) | [Provider model refresh](provider-model-refresh.md) | [Binary runtime](../../runbooks/binary-runtime.md)

---

[简体中文](troubleshooting.zh-CN.md)
