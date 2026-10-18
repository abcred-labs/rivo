# Provider management

A Provider is a named connection with `base_url`, an optional Credential reference, enabled state, priority and discovery state. It does not carry a protocol or a default Model. Protocol choice belongs to the selected Model.

## Inspect Providers

```text
rivo config provider list
rivo config provider list --format table
rivo config provider show <provider-id> --format json
```

Queries are offline and redact Credential values. Providers with the same URL remain separate objects.

## Create or update a Provider

```text
rivo config provider set gateway \
  --base-url https://api.example.com/v1 \
  --credential-name provider-key \
  --priority 100 \
  --enabled true
```

`--base-url` is a path prefix. Do not include `/chat/completions`, `/responses`, or `/messages`. A `/v1` suffix is recommended. The request paths are `<base>/chat/completions`, `<base>/responses`, or `<base>/messages` for the corresponding Model protocol; Claude Messages adds `/v1/messages` when the base does not already end in `/v1`.

`--credential-name` must refer to an existing Credential. `--enabled` accepts a boolean and `--priority` accepts the Provider ordering value. `--protocol` is not supported; change protocols with `config model set`.

A new Provider or a URL/Credential update is saved locally without network access by default, then its old discovery is invalidated. Rivo prints an impact plan for connection changes. Re-run the same command with the displayed `--confirm <confirmation-token>` after reviewing `affected_models`, `impact_complete`, and the revision. `--refresh` performs discovery after saving; a failed refresh does not roll back the saved connection.

## Refresh and remove

```text
rivo config provider models refresh <provider-id>
rivo config provider remove <provider-id>
rivo config provider remove <provider-id> --yes
```

Removal prints an impact plan and asks for confirmation unless `--yes` is supplied. It clears defaults that point to the Provider but does not delete Models, Vendors, or Credentials.

[Configuration index](README.md) | [Credential management](credential-management.md) | [Provider model refresh](provider-model-refresh.md)

---

[简体中文](provider-management.zh-CN.md)
