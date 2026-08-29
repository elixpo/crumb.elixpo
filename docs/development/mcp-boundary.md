# Crumb MCP boundary

Crumb exposes tools through a Rust-owned MCP server rather than granting the
Harness direct shell or filesystem capabilities. The initial server supports
newline-delimited stdio for MCP `2025-11-25`, used by the current DeepSeek
Harness client, and the stateless `2026-07-28` discovery shape.

## Policy

- `plan` denies every tool call.
- `auto` permits only tools registered as read-only without interaction.
- `negotiate` and all mutating risk classes require a user-owned allow-once
  decision.
- No approval channel means deny, never allow.
- Approval metadata contains a digest of arguments, not raw arguments.
- Cancellation is shared with the agent session and tool handler.
- Unknown tools are protocol errors; expected execution failures are MCP tool
  results with `isError: true` so the model may self-correct.

Tool annotations are derived from trusted Rust metadata. Model-provided or
third-party annotations never change Crumb's risk class or approval decision.

## Workspace reads

`crumb-tools` supplies `read_file` and `list_directory` as native, read-only
tools. The host canonicalizes the workspace and every requested target before
access, so parent traversal and symlink escapes are rejected. Callers provide
the output-byte and directory-entry ceilings; tool arguments may only lower
those limits. Results are sorted where applicable, UTF-8 only, bounded, and
share the session cancellation token.

## Isolated shell

`run_shell` starts a fresh non-interactive process for each approved call. It
never reuses the interactive terminal PTY, inherits no environment variables,
and runs from the canonical workspace. The caller selects the shell program,
arguments, safe `PATH`, output ceiling, and timeout. Tool input may shorten the
timeout but cannot raise it. Standard output keeps its head while standard
error keeps its tail, preserving the most useful diagnostics within the shared
result budget. Cancellation and timeout terminate and reap the child; Unix
builds terminate the complete process group.

## Interactive approvals

The bounded approval channel transfers one pending request from execution to a
trusted terminal UI. Metadata carries the stable request identifier, tool,
risk, and argument digest; raw arguments are transient and accessible only from
the UI-side pending value. A decision is consumed once. Dropping the pending
value, losing either channel endpoint, or cancelling the session denies the
request. A full UI queue is polled with cancellation instead of blocking the
agent indefinitely.

## Next slices

1. [x] Workspace-confined read and directory-list tools.
2. [x] Isolated shell execution with output/time ceilings.
3. [x] Interactive allow-once approval bridge for negotiate mode.
4. [x] `crumb mcp serve` entry point and Harness Cordis composition.

The stdio entry point intentionally registers only `read_file` and
`list_directory`. Its stdout is MCP-only, it loads limits and mode from the
workspace's live `.crumb/agent.json`, and it performs no network access. The
checked-in Cordis composition exposes those tools as `mcp__crumb__...` and
omits Harness-native shell and filesystem plugins. Approval-gated mutation will
be added only through a parent-CLI-owned interactive transport; the unattended
stdio child cannot approve mutations.
