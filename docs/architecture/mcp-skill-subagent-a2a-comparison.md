# MCP, Skill, SubAgent and A2A comparison

These four names describe different extension boundaries:

| Surface | What it is | Trust boundary |
| --- | --- | --- |
| MCP | External tools/resources/prompts protocol | Configured server, transport and per-Turn allowlist |
| Skill | Described, authorizable capability | Local registry and policy |
| SubAgent | Internal child task in the same process | Parent model, workspace, policy and budget |
| A2A | Remote agent task protocol | Explicit pairing, endpoint identity and remote owner |

An Agent can use an MCP tool after local discovery and policy checks. A root Agent can delegate a bounded task to a SubAgent; this is not recursive and does not duplicate the root Session. An A2A client delegates to a paired peer; discovery alone is not trust. An A2A server executes under its own local policy and approval.

Choose local tools for workspace work, MCP for configured external systems, SubAgents for independent bounded subtasks, and A2A for trusted cross-process or cross-host work. All four surfaces retain cancellation, confirmation, evidence and redaction rules.

[Tool runtime](tool-runtime.md) | [A2A interoperability](a2a-interoperability.md) | [Chinese version](mcp-skill-subagent-a2a-comparison.zh-CN.md)
