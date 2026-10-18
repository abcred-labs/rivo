# Provider model refresh

Provider discovery is the only configuration operation that contacts a model service. It reads the Provider's authenticated model directory and records a redacted, managed discovery result.

## Inspect, preview, and refresh

```text
rivo config provider models remote <provider-id>
rivo config provider models refresh <provider-id>
rivo config provider models list <provider-id>
rivo config provider show <provider-id>
```

`remote` prints the complete remote IDs without changing local configuration. `refresh` validates the response and persists only exact IDs that already exist in the local Model catalog and are enabled. It does not create Vendors or Models and does not change defaults. `list` shows the last status, timestamp, error and matched local IDs.

Discovery first requests `/v1/models` with Bearer authentication. On HTTP 401 or 403 it retries once with `x-api-key` and `anthropic-version: 2023-06-01`. Actual inference authentication follows the selected Model protocol.

## Conditions for a usable route

A Provider can route a Model only when all of these are true:

1. Provider is enabled.
2. The latest refresh succeeded and is marked supported.
3. The current URL and Credential reference match the discovery fingerprint.
4. Discovery contains the exact, case-sensitive canonical Model ID.
5. The Model is enabled and has a non-empty protocol list.

There is no wall-clock TTL or background refresh. Changing the URL or Credential immediately invalidates discovery. Changing Model protocol order does not.

## Failure handling

A failed refresh stores a redacted error and removes old usable status. Fix the URL, Credential or remote permission and refresh again; never hand-edit the cache. A 404 or `unsupported` response means the directory endpoint is unavailable for this Provider and cannot be bypassed by running the inference endpoint directly.

[Configuration index](README.md) | [Provider management](provider-management.md) | [Troubleshooting](troubleshooting.md)

---

[简体中文](provider-model-refresh.zh-CN.md)
