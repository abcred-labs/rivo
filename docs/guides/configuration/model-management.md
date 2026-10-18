# Model management

A Model is a canonical catalog identity. It owns the Vendor, display name, enabled state, priority, reasoning support, and ordered `supported_protocols`. It does not own a URL, Credential, or Provider mapping.

Model IDs are case-sensitive and must exactly match the full ID returned by Provider discovery. Rivo does not add aliases, strip prefixes, or rewrite the request model ID.

## Inspect the catalog

```text
rivo config model list
rivo config model list --vendor <vendor-id> --state enabled --format table
rivo config model show <canonical-model-id> --format json
rivo config model providers <canonical-model-id>
```

`--state` accepts `enabled`, `disabled`, or `all` (the default). `list` and `show` are offline. `model providers` reports Providers whose successful discovery contains the exact ID.

## Create a custom Model

```text
rivo config vendor set acme --display-name "Acme AI"
rivo config model set acme-chat \
  --vendor acme \
  --display-name "Acme Chat" \
  --protocols openai_chat_completions,openai_responses \
  --reasoning default,low,medium \
  --priority 100
```

A custom entry requires an existing Vendor, a non-empty protocol list without duplicates, and a priority from `0` through `1000`. Protocol order is the preference order. Use evidence for the exact Model ID and the endpoint's actual protocol support; discovery alone cannot infer either.

When updating an existing built-in or local Model, omitted fields keep their current values. `vendor` and `display-name` are required only when creating an entry.

## Change protocols and enabled state

```text
rivo config model set <model-id> --enable-protocol claude_messages
rivo config model set <model-id> --disable-protocol openai_responses,claude_messages
rivo config model set <model-id> --protocols claude_messages,openai_chat_completions
rivo config model disable <model-id> [<model-id>...]
rivo config model enable <model-id> [<model-id>...]
```

The last enabled protocol cannot be removed. Built-in changes are stored as local overrides. Changing Model protocols does not invalidate Provider discovery, but the selected first protocol can change the request endpoint.

Disabled Models do not participate in default selection. Explicitly selecting a disabled Model fails. Batch enable/disable is one atomic local update; unknown IDs are reported as `missing` while valid IDs continue.

## Remove a Model

```text
rivo config model remove <canonical-model-id>
rivo config model remove <canonical-model-id> --yes
```

Removal shows an impact plan and requires interactive confirmation unless `--yes` is supplied. Defaults and catalog references must remain valid. Provider discovery and Credential objects are not deleted as a side effect.

[Configuration index](README.md) | [Vendor management](vendor-management.md) | [Model routing](model-routing.md)

---

[简体中文](model-management.zh-CN.md)
