# Persistence file compatibility and upgrades

Rivo manages configuration, credentials, instances and Session history as separate files. Upgrades are explicit, validated and zero-write on unsafe input.

## Managed files

| File | Current contract | Secret content |
| --- | --- | --- |
| `config.toml` | schema 6; Vendor/Model/Provider, defaults, MCP/A2A references | forbidden |
| `credentials.toml` | current local encrypted/obfuscated Credential store | contains only Credential values |
| `instances.json` | local instance registry | references and endpoints only |
| SQLite sessions | current Session/Workspace history schema | may contain business content |

`RIVO_CONFIG` selects the config file; Credential storage remains beside it. `RIVO_CONFIG_CONTENT` is a read-only test snapshot and never writes.

## Upgrade process

Supported historical config, Credential and instance formats are parsed, validated and written atomically to the current format. Unknown fields, corruption, future versions or semantic loss abort with zero writes and preserve the original file. A backup should be made before any controlled Session migration.

Schema 17 to 18 Workspace migration is not silent. Run `sessions migrate --yes` or provide `--backup <PATH> --yes`. Unscoped sessions remain unscoped until explicitly attached. Do not delete the whole data directory; retain Credential, memory, MCP, A2A trust and other non-session data.

## Observability

Upgrade reports the source, destination, schema and checksum. A failed migration leaves the source intact. Diagnostics may export redacted health and journal data but must be treated as sensitive.

[Development runbook](../runbooks/development-runtime.md) | [Chinese version](persistence-file-compatibility.zh-CN.md)
