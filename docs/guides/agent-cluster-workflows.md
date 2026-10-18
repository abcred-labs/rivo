# Agent cluster workflows

Rivo supports three practical ways to split work: one local Agent with tools, internal SubAgents, and trusted A2A peers. MCP is an external capability connection, not a cluster coordinator.

## Choose a workflow

Use one local Turn for tasks that share files and state. Use SubAgents for independent bounded investigations; they inherit the parent Model, workspace, policy and budget, and return bounded summaries and evidence. Use A2A when another process or host owns the data or execution boundary and can be paired explicitly.

## Local and SubAgent flow

Describe the goal, scope, constraints and deliverable. Keep write operations in the parent workspace unless a child task has an explicit safe boundary. Review evidence and conflicts before applying a result. `--no-subagents` disables internal delegation; `--max-subagents` controls concurrency.

## A2A flow

Pair the peer, inspect its Agent Card and trust record, then delegate a bounded skill with the message on stdin:

```text
rivo a2a delegate <peer-id> <skill-id> --message-stdin --yes --stream
```

The command reports a task ID and status; it does not automatically export a complete business report. Cancel, recover or revoke using the task/peer IDs. The remote owner applies its own policy and approvals.

## Compare costs and risks

| Workflow | Best for | Main boundary |
| --- | --- | --- |
| Single local Turn | Shared files and iterative edits | One workspace and one active Turn |
| SubAgents | Independent parallel research | Parent policy, budget and model |
| MCP | External data/tool service | Server transport and allowlist |
| A2A | Trusted cross-host delegation | Pairing, remote owner and network |

Do not put secrets in prompts, messages, command arguments or smoke reports. Verify evidence and terminal status before treating delegated work as complete.

[Chinese version](agent-cluster-workflows.zh-CN.md) | [A2A pairing](../runbooks/a2a-lan-pairing.md)
