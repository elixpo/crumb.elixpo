# Agent foundation

Branch: `feat/wp-012-agent-foundation`

This work package establishes Rust-owned boundaries before any external harness
is allowed to execute. It does not add model traffic to startup or to ordinary
native commands.

## Deterministic input routing

Routing performs no model call and follows this precedence:

1. Crumb built-ins remain typed and local.
2. `?` and `@` explicitly select the agent path.
3. Shell syntax, paths, environment assignments, and commands resolved from
   `PATH` remain native.
4. A likely typo of a discovered executable remains native and carries a
   deterministic suggestion for the error UI.
5. A single unknown token remains native so the shell owns `command not found`.
6. An unresolved multi-word phrase follows the configured fallback: agent,
   negotiate, or native.

This deliberately favors the shell when input is ambiguous. A phrase beginning
with an executable name can always use `?` or `@` to opt into intelligence.

## Operating modes

- `auto`: execute policy-approved harmless steps without extra pauses.
- `negotiate`: show proposed actions and request decisions.
- `plan`: produce a plan and do not execute tools.

These modes affect agent execution, not whether valid shell input is native.

## Live configuration

The schema is represented by `crumb-agent::AgentConfig`; an example lives at
[`docs/examples/agent-config.json`](../examples/agent-config.json). The runtime
reloads the file for each new turn, so edits are immediately visible. The
schema cannot contain credential fields. Prompt-driven changes must become a
typed patch and pass the same validation and approval path as manual edits.

Models, skills, MCP processes, harness commands, optimizer commands, limits,
and modality routes are data. They are not compiled into the router.

## Sessions and cancellation

Crumb owns the session identifier, workspace, mode, limits, cancellation token,
and append-only journal. Journals store event metadata and content digests, not
raw prompts, raw tool arguments, credentials, or unfiltered command output.

Ctrl+C sets the shared cancellation token. Native tools must stop their child
processes. The current DeepSeek Harness JSON-RPC SDK has no turn-cancellation
method, so its adapter must terminate and restart the harness subprocess.

## Harness boundary

DeepSeek Harness is a developer-preview optional process reached through
newline-delimited JSON-RPC on stdio. Crumb must pin and check its version, own
all approvals, and fall back to the native loop when it is absent or fails.
Harness configuration is loaded from the live config and never startup-critical.

## MCP and tools

The Rust tool registry owns names, schemas, risk classes, and transport. MCP is
a transport for registered capabilities, not a way to bypass approval. Native
tools win when they provide the same capability with fewer round trips; an MCP
server is chosen only for capabilities it uniquely owns or when configuration
explicitly selects it.

## Token optimization

The pipeline is secret redaction, command-aware filtering, deduplication,
optional external optimization, and budget clipping. RTK is an optional process
adapter. Missing RTK must return the unmodified redacted output.

TOON 4.1 is a working-draft structured encoding. `auto` must compare encoded
sizes and use TOON only for lossless structures where it is smaller; JSON stays
the fallback for irregular data and function-call protocols that require JSON.

## Next packages

1. Wire routing and suggestions into the CLI without changing native execution.
2. Add error capture and the configurable help panel.
3. Add the native agent state machine and isolated shell tool.
4. Add the DeepSeek Harness subprocess adapter and hard cancellation.
5. Add MCP stdio serving/client transports and approval enforcement.
6. Add native filters, RTK, TOON measurement, and context budgeting.
7. Add skill discovery and prompt-driven configuration patches.
