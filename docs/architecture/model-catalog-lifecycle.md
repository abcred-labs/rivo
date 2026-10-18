# Model catalog lifecycle

The catalog has Vendor, canonical Model and Provider objects. A Model is created only when its exact ID, owner Vendor and supported protocol evidence are known. Provider discovery reports what an endpoint exposes; it does not create catalog entries or infer protocol capability.

## Add a Model

1. Verify the exact canonical ID from the provider's official catalog and discovery response.
2. Create or select its Vendor.
3. Register a non-empty, ordered protocol list and optional reasoning support.
4. Configure a Provider and refresh discovery.
5. Verify `config model providers <model-id>` and `config route` before a real request.

Built-in Models can receive local overrides for protocol order, reasoning support, priority and enabled state. The override does not change the built-in catalog file.

## Deprecate a Model

Disable a Model before removal when a staged migration is needed. Disabled Models remain inspectable but are excluded from default selection and explicit routing fails. Update documentation and release notes with the replacement canonical ID and protocol differences.

## Remove a Model

`config model remove` prints an impact plan and requires confirmation or `--yes`. The command must clear invalid defaults and reject dangling catalog references atomically. It does not delete Provider discovery or Credentials.

## Validation

`config validate` checks schema, IDs, Vendor references, non-empty protocols, ranges and default references. It does not prove remote authentication or discovery. `provider models refresh` is required for availability and exact-ID matching.

[Model management](../guides/configuration/model-management.md) | [Chinese version](model-catalog-lifecycle.zh-CN.md)
