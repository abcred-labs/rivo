# Agent capability baseline

Rivo's baseline is a local, policy-controlled agent loop with explicit capability profiles, bounded tools and observable evidence. The product does not promise autonomous unrestricted execution.

## Capability profiles

`general`, `research`, `plan`, `coding` and `review` select prompt and tool defaults. `strict`, `medium` and `full_control` are independent policy profiles. The default policy is medium; higher policy never disables schema validation, resource limits or auditing.

## Tools and boundaries

Read tools inspect files within the workspace boundary. Coding tools require the coding profile and confirmation for changes. Web is enabled per Turn with `--network`; Browser uses a dedicated CDP profile; MCP uses configured server/toolset allowlists; SubAgents inherit the parent model, workspace and policy; A2A requires paired trust. Each capability emits bounded, redacted activity.

## Data and evidence

Research outputs link claims to EvidenceRef records. Validation checks shape and references but cannot prove source truth or model correctness. Session history and diagnostics may contain business content and should be treated as sensitive.

## Acceptance

Verify profiles and tools through CLI/TUI tests, terminal smoke, MCP discovery, A2A pairing and release smoke. Do not treat a unit test or historical report as proof of every platform or external integration.

[Safety policy](safety-policy.md) | [Tool runtime](tool-runtime.md) | [Chinese version](agent-capability-baseline.zh-CN.md)
