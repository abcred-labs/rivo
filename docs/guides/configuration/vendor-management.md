# Vendor management

Vendor identifies the publisher namespace used by a canonical Model. It is a local
catalog object, not a network connection and not a credential. A custom Model must
refer to an existing Vendor before it can be routed.

## Inspect the catalog

```text
rivo config vendor list
rivo config vendor list --format table
rivo config vendor list --format json
rivo config vendor show <vendor-id>
```

`list` and `show` are offline read-only queries. Built-in entries are shown with
their source; local entries and overrides are kept in the user configuration.

## Create or update a Vendor

```text
rivo config vendor set <vendor-id> --display-name "My Gateway"
```

Vendor IDs are stable catalog identifiers. Use a short ASCII ID and keep the display
name suitable for a user-facing selector. The command is local and does not contact
the provider.

## Custom Model workflow

Create the Vendor first, then register the exact model ID and its protocol evidence:

```text
rivo config vendor set acme --display-name "Acme AI"
rivo config model set acme-chat \
  --vendor acme \
  --display-name "Acme Chat" \
  --protocols openai_chat_completions \
  --priority 100
```

The protocol list is non-empty, has no duplicates, and is ordered by preference.
Only `openai_chat_completions`, `openai_responses`, and `claude_messages` are
accepted. A Provider discovery result does not infer a protocol or create a Model.
Finish the workflow by configuring a Provider and running an explicit refresh; see
[model management](model-management.md) and [Provider model refresh](provider-model-refresh.md).

## Remove a Vendor

```text
rivo config vendor remove <vendor-id>
rivo config vendor remove <vendor-id> --yes
```

Without `--yes`, Rivo prints an impact plan and asks for interactive confirmation.
The plan can include Models, Providers and default selections affected by the
deletion. Removal does not delete Credential secrets. A non-interactive invocation
must use `--yes` explicitly.

## Naming and routing rules

Vendor, Model and Provider IDs are compared exactly. They are case-sensitive and
are not matched by aliases or prefixes. A Vendor can own multiple Models, and one
Provider can serve Models from multiple Vendors when its discovery and protocol
configuration support them.

[Configuration index](README.md) | [Model management](model-management.md) |
[Provider management](provider-management.md)

---

[简体中文](vendor-management.zh-CN.md)
