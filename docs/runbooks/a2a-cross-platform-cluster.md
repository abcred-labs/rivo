# Cross-platform A2A cluster runbook

Use this runbook to test a Windows/Linux or Linux/macOS trusted A2A pair. It is a research and acceptance procedure, not a hosted orchestration service.

## Topology and prerequisites

Build both hosts from the same commit and use matching protocol revisions. Windows uses `rivo.exe`; Linux/macOS use the native release binary. Reserve a private bind address and firewall rule, prepare OS-protected server/client identity material, and confirm that each host can run an interactive command.

## Server setup

Provision the server identity and owner config, bind only to the intended private address, and start `rivo a2a serve`. Keep private keys and certificates in OS-protected storage. The TOML, SQLite and logs may contain references and fingerprints but never private key material.

## Pair and delegate

On the client run discovery, verify the endpoint/SPKI and pair with PSK or mTLS. The server owner confirms request ID and SAS before approving. After pairing, inspect the peer and delegate a bounded skill through stdin. Cancellation and recovery use the remote task ID; revocation is explicit on both sides.

## Acceptance evidence

Record OS, commit, protocol revision, Agent Card, pairing evidence, task ID, event sequence, cancellation result, recovery result and revoked trust state. A successful local fixture does not prove every firewall, terminal, certificate store or network topology.

[A2A pairing](a2a-lan-pairing.md) | [Chinese version](a2a-cross-platform-cluster.zh-CN.md)
