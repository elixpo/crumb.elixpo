# Crumb MCP boundary

Crumb exposes tools through a Rust-owned MCP server rather than granting the
Harness direct shell or filesystem capabilities. The initial server supports
newline-delimited stdio for MCP `2025-11-25`, used by the current DeepSeek
Harness client, and the stateless `2026-07-28` discovery shape.

## Policy

- `plan` denies every tool call.
- `auto` permits only tools registered as read-only without interaction.
- `negotiate` and all mutating risk classes require either a user-owned
  allow-once decision or an exact persistent grant in live configuration.
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

## Checkpointed workspace writes

`write_file` is registered as `write_workspace` and remains denied unless its
exact tool name appears in the user-owned workspace permission allowlist. Every
successful write records a bounded preimage and post-edit digest under
`.crumb/checkpoints`. Credential-sensitive paths, private keys, symlinks, and
workspace escapes are refused before mutation. Rewind restores only when the
current file still matches Crumb's post-edit digest; it never invokes Git reset
or overwrites a later user change. `/review` shows bounded file summaries before
diffs, accepts per-file or all-pending decisions, and exports stable JSON.
Review comments stay only in process memory and are consumed by the next agent
turn. `crumb review export <id|all>` provides prompt-free JSON for automation.

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

The stdio entry point registers `read_file`, `list_directory`, and checkpointed
`write_file`. Its stdout is MCP-only and it loads limits, mode, and exact
user-owned permission allowlists from the workspace's live
`.crumb/agent.json`. The checked-in Cordis composition exposes these tools as
`mcp__crumb__...` and omits Harness-native shell and filesystem plugins.
