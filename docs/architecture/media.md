# Agent media contract

Media is an Agent output asset, not a separate CLI command. A task can request `generate_image` or `edit_image`; the normal Session, policy and confirmation pipeline still applies.

## Public objects

A media asset has an asset ID, bounded metadata, MIME type, dimensions, local path and lifecycle status. Text mode reports a short local path or temporary URL on stderr. JSONL emits `media_started`, `media_ready` or `media_failed`; only `media_ready` means the asset is available.

## Storage and cleanup

Temporary media is stored under `RIVO_DATA_DIR/tmp/media/<session-id>/<asset-id>.<ext>` with an index file. The lifecycle is started, written, verified, ready or failed, followed by retention cleanup. Paths are bounded to the data directory and are never accepted as arbitrary tool destinations.

## Provider and policy boundary

The selected Model/Provider handles the media request under the same route and Credential rules as text. Policy checks workspace, network, size and confirmation requirements. Credentials and provider responses are redacted from public events.

[CLI contract](cli.md) | [Chinese version](media.zh-CN.md)
