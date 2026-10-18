# Model routing

A route contains one Vendor, one canonical Model, and one Provider. The Model supplies the request protocol; the Provider supplies `base_url`, authentication and Provider priority. Runtime commands cannot override the URL or Credential with ad-hoc values.

## Set defaults

```text
rivo config set vendor <vendor-id>
rivo config set model <canonical-model-id>
rivo config set provider_id <provider-id>
rivo config unset provider_id
```

`unset provider_id` returns the default to Auto. A single run can override the defaults with `--vendor`, `--model`, `--provider-id`, or `--auto-provider`; `--provider-id` and `--auto-provider` are mutually exclusive.

Selection precedence is CLI flags, then `RIVO_VENDOR`/`RIVO_MODEL`/`RIVO_PROVIDER_ID`, then persisted defaults. `--auto-provider` explicitly overrides a configured Provider.

## Preview and verify

```text
rivo config validate
rivo config model providers <canonical-model-id>
rivo config route --vendor <vendor-id> --model <canonical-model-id> --provider-id <provider-id>
rivo config route --vendor <vendor-id> --format json
```

These commands are offline and do not prove that a remote service is reachable. Run a Provider refresh before a real task.

## Auto and explicit Provider

Auto considers enabled Providers with a successful, current discovery fingerprint containing the exact Model ID. It sorts by Provider priority descending and Provider ID ascending. If no Model is specified, enabled Models are sorted by Model priority descending and Model ID ascending; Rivo does not switch Models because a selected Model is unavailable.

An explicit Provider must be usable for the selected Model or the route fails immediately. Rivo has no runtime failover after a request starts. Discovery becomes invalid when the Provider URL or Credential reference changes; changing Model protocol preferences does not invalidate it.

A route is usable only when the Model is enabled, has at least one protocol, the Provider is enabled, discovery succeeded, the configuration fingerprint matches, and discovery contains the exact canonical ID.

[Configuration index](README.md) | [Provider management](provider-management.md) | [Provider model refresh](provider-model-refresh.md)

---

[简体中文](model-routing.zh-CN.md)
