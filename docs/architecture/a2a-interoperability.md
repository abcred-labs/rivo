# A2A interoperability

Rivo's A2A surface connects explicitly trusted agent instances over a versioned protocol. It is a task and instance-control integration, not a hosted coordinator or a replacement for local policy.

## Boundaries

The client discovers LAN candidates, verifies an endpoint and certificate/public-key identity, pairs with an owner-approved server, and stores a local peer trust record. The server authenticates the peer, applies its own policy and owner approval, then runs the task through a local Session. Discovery never implies trust.

## Stable protocol

The wire version, protocol revision, Agent Card, task IDs and terminal states are versioned. Delegation carries a bounded skill ID, message and workspace-safe options. Events stream through SSE/JSON-RPC, and cancellation/recovery use the remote task ID. Exactly one terminal state is accepted.

## Pairing and security

Pairing verifies endpoint, TLS SPKI, SAS and public-key proof. PSKs are read from controlled stdin. Private keys, server certificates and client CA material stay in OS-protected storage; configuration and logs contain references and fingerprints only. Revoke both client and server trust when a peer or identity changes.

## Current selection

A delegated turn can select Vendor, canonical Model and Auto/explicit Provider only from the server's local valid catalog. It cannot supply an arbitrary endpoint, Credential, policy bypass or remote owner approval. A remote cancellation stops the corresponding local Turn and produces a terminal event.

Use the [A2A LAN pairing runbook](../runbooks/a2a-lan-pairing.md) and [cross-platform runbook](../runbooks/a2a-cross-platform-cluster.md) for commands and acceptance evidence.

[Chinese version](a2a-interoperability.zh-CN.md)
