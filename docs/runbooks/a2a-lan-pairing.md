# A2A LAN pairing runbook

This runbook covers trusted local/LAN A2A pairing. Discovery finds candidates; it does not establish trust. Use a private network, verify endpoint and SPKI, and keep PSKs on controlled stdin.

## Prerequisites

Install the same release or source commit on both hosts. Prepare a bind address reachable only by the intended peer, a server config, and OS-protected identity material. Do not put PSKs or private keys in command arguments, TOML, logs or reports.

## Server and discovery

Start the local server with the documented `rivo a2a serve` profile and owner config. On the client:

```text
rivo a2a discover
rivo a2a pair <LOCAL_IDENTITY_LABEL> --endpoint <HTTPS_URL> --spki-sha256 <SHA256> --transport psk --secret-stdin
```

Enter the same PSK through stdin when prompted. Verify the displayed endpoint, SAS, SPKI and peer public-key digest on the server owner console before approving the pairing request.

## Inspect and use

```text
rivo a2a peers
rivo a2a inspect <PEER_ID>
rivo a2a delegate <PEER_ID> <SKILL_ID> --message-stdin --yes --transport psk --secret-stdin
rivo a2a cancel <PEER_ID> <TASK_ID> --yes --transport psk --secret-stdin
rivo a2a recover <PEER_ID> <TASK_ID> --transport psk --secret-stdin
```

Delegated tasks run under the remote owner's policy and approvals. A peer cannot provide arbitrary endpoint, Credential or policy overrides.

## Revoke and troubleshoot

Revoke on both sides when identity or endpoint changes. Check firewall, bind address, certificate/SPKI, PSK input, peer trust and server owner approval. A network timeout is not proof of a successful remote task; recover by task ID and verify one terminal state.

[Architecture](../architecture/a2a-interoperability.md) | [Chinese version](a2a-lan-pairing.zh-CN.md)
