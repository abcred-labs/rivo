# Provider endpoint ownership and Model protocol contract

The routing domain contains four independent objects: Vendor, canonical Model, Provider and Credential. A Provider owns the connection endpoint and authentication reference. A Model owns its identity and ordered protocol capability. No binding table, request-model alias, or Provider protocol field is used.

## Object lifecycle

1. Create or select a Vendor.
2. Register a canonical Model with an evidence-based, non-empty protocol list.
3. Create a Provider with a base URL and optional Credential reference.
4. Run authenticated discovery explicitly.
5. Resolve exact Vendor/Model/Provider selection for each Turn.

A single Provider can serve multiple Vendors and Models. Two Providers with the same URL remain separate so that credentials, priorities and discovery fingerprints remain independent.

## Schema 6 routing fields

Each local Model stores `vendor`, `display_name`, `supported_protocols`, `supported_reasoning_efforts`, `enabled` and `priority`. The first supported protocol is the request protocol. Supported values are `openai_chat_completions`, `openai_responses` and `claude_messages`.

Each Provider stores `base_url`, optional `auth.credential`, `enabled`, `priority` and a discovery cache. The cache records supported state, exact discovered IDs, last check time, configuration fingerprint and a redacted last error.

Credential values are stored separately. The public config and route projection never contains secret text.

## Endpoint and authentication rules

Use a base URL such as `https://host/v1`; do not include an operation path. The request paths are:

| Model protocol | Request path | Authentication |
| --- | --- | --- |
| `openai_chat_completions` | `<base>/chat/completions` | `Authorization: Bearer` |
| `openai_responses` | `<base>/responses` | `Authorization: Bearer` |
| `claude_messages` | `<base>/messages` when base ends `/v1`, otherwise `<base>/v1/messages` | `x-api-key` and `anthropic-version: 2023-06-01` |

Discovery requests `/v1/models` with Bearer first and retries once with Claude headers only after 401 or 403. Provider URL and Credential changes invalidate discovery. Protocol-list changes do not invalidate it.

## Resolution and availability

A route is available only if the Provider is enabled, discovery succeeded, its fingerprint matches the current URL and Credential reference, the Model is enabled with at least one protocol, and discovery contains the exact case-sensitive Model ID. Remote discovery never creates local catalog entries or infers protocols.

Auto Provider sorts usable Providers by priority descending and ID ascending. If Model is omitted, enabled Models sort by Model priority descending and ID ascending. Explicit unusable selections fail immediately. There is no runtime failover or alias matching.

## Changes and deletion

Provider URL/Credential changes produce an impact plan containing affected Models, completeness and a revision. The same command with the current confirmation token commits the change. Vendor, Model and Provider deletion show a token-free impact plan and require interactive confirmation or `--yes`. Deleting a Provider does not delete its Credential.

Configuration migration supports the documented historical `config.toml`, `credentials.toml` and `instances.json` formats. Corrupt, future or semantically unsafe input is rejected with zero writes. Session migration is controlled separately and retains non-session data.

[Configuration guides](../guides/configuration/README.md) | [Chinese version](provider-endpoint-ownership.zh-CN.md)
