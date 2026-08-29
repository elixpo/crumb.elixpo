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

## Next slices

1. [x] Workspace-confined read and directory-list tools.
2. [x] Isolated shell execution with output/time ceilings.
3. Interactive allow-once approval bridge for negotiate mode.
4. `crumb mcp serve` entry point and Harness Cordis composition.
