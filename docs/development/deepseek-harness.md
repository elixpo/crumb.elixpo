# DeepSeek Harness adapter

Branch: `feat/wp-013-deepseek-harness`

Crumb treats the Harness as an optional subprocess outside terminal startup and
native command execution. The adapter implements the public newline-delimited
JSON-RPC SDK protocol; stdout is protocol-only and stderr is bounded diagnostic
output.

## Wire contract

The supported client requests are `initialize`, `session/prompt`, and
`shutdown`. Initialization selects the workspace, provider, exact model,
optional reasoning effort, and optional output-token cap. Prompts carry a Crumb
session identifier and content blocks.

Reasoning effort is selected before process initialization. Changing provider,
model, or effort requires a clean process restart so one Harness process cannot
silently retain another route's defaults. Unsupported effort values fail before
model traffic.

## Failure and cancellation

Requests and shutdown are time-bounded. Ctrl+C first sets Crumb's shared
cancellation token, then terminates and reaps the Harness process because the
SDK protocol has no prompt-cancel method. Startup, protocol, timeout, and crash
errors return control to Crumb's native agent runtime; they never terminate the
interactive shell.

## Delivery slices

1. Typed protocol framing and compatibility fixtures.
2. Lazy subprocess lifecycle with bounded stderr and shutdown escalation.
3. Session notification projection and committed terminal output.
4. CLI fallback integration and cancellation.
5. Pollinations-compatible Harness composition and effort capabilities.
