# Development TODO

This is the local source of truth for implementation order. GitHub issues provide discussion and ownership; completed work remains checked here for quick repository context.

## Completed milestones

- [x] [WP-001 — workspace bootstrap](https://github.com/elixpo/crumb.elixpo/issues/1)
- [x] [WP-002 — persistent Linux Bash PTY](https://github.com/elixpo/crumb.elixpo/issues/4)

## Active stack

- [ ] [WP-003 — macOS Zsh parity](https://github.com/elixpo/crumb.elixpo/issues/6) ([PR #7](https://github.com/elixpo/crumb.elixpo/pull/7))
- [ ] [WP-004 — Windows PowerShell parity](https://github.com/elixpo/crumb.elixpo/issues/8) ([PR #9](https://github.com/elixpo/crumb.elixpo/pull/9))
- [ ] [WP-005 — shell lifecycle protocol](https://github.com/elixpo/crumb.elixpo/issues/10) ([PR #12](https://github.com/elixpo/crumb.elixpo/pull/12))
- [ ] [WP-006 — terminal UI layer](https://github.com/elixpo/crumb.elixpo/issues/13) ([PR #14](https://github.com/elixpo/crumb.elixpo/pull/14))
- [ ] [WP-007 — persistent history](https://github.com/elixpo/crumb.elixpo/issues/15) ([PR #17](https://github.com/elixpo/crumb.elixpo/pull/17))
- [ ] [Linux terminal parity](https://github.com/elixpo/crumb.elixpo/issues/16) ([PR #18](https://github.com/elixpo/crumb.elixpo/pull/18))
- [ ] [WP-008 — provider-neutral LLM interface](https://github.com/elixpo/crumb.elixpo/issues/19) ([PR #20](https://github.com/elixpo/crumb.elixpo/pull/20))
- [ ] [WP-009 — Pollinations adapter](https://github.com/elixpo/crumb.elixpo/issues/21) ([PR #23](https://github.com/elixpo/crumb.elixpo/pull/23))
- [ ] [WP-010 — secure Pollinations BYOK](https://github.com/elixpo/crumb.elixpo/issues/24) ([PR #27](https://github.com/elixpo/crumb.elixpo/pull/27))

## Current milestone: [WP-011 — browser account connector](https://github.com/elixpo/crumb.elixpo/issues/26)

- [x] Add the minimal Next.js Cloudflare Worker surface.
- [x] Add Elixpo Accounts sign-in with `openid profile email`.
- [x] Adapt the Pollinations PKCE connector.
- [x] Register a separate public Accounts CLI device client.
- [x] Exchange verified Accounts device authorization for the linked connector.
- [x] Encrypt provider tokens at rest in D1.
- [x] Replace pasted-key login with the browser connection flow.
- [x] Keep the OS keyring as the terminal credential boundary.
- [x] Pin the initial multimodal Pollinations connector allowlist.
- [x] Configure local D1, KV, and `.env.local` credentials.
- [ ] Register and validate the signed Accounts deletion webhook.
- [ ] Validate the browser-to-terminal flow.
- [ ] Pass Rust and web checks.
- [ ] Merge WP-011.

<details>
<summary>WP-001 completion details</summary>

- [x] Create the Cargo workspace.
- [x] Add `crumb-cli`, `crumb-core`, `crumb-repl`, and `crumb-platform`.
- [x] Detect Linux, macOS, and Windows with a typed `Platform` enum.
- [x] Render a minimal prompt containing the working directory.
- [x] Implement `/exit`, `/version`, and `/platform`.
- [x] Classify all other input as `NativeInput` without executing it.
- [x] Add unit tests and cross-platform GitHub Actions.
- [x] Pass formatting, clippy, and workspace tests.
- [x] Publish WP-001 to `main`.

</details>

## Active CLI package: `/` commands and `@` references

- [x] Keep plain English as the only natural-language prompt form.
- [x] Reserve known `/` commands without intercepting native absolute paths.
- [x] Implement `/help`, `/skills`, `/connectors`, and `/context`.
- [x] Keep authentication, history, platform, version, shell, and exit commands functional.
- [x] Add Tab suggestions for slash commands and typed inline references.
- [x] Load enabled skill and configured MCP/plugin identifiers into suggestions.
- [x] Bound file and folder suggestions to the active workspace.
- [ ] Resolve selected `@` references into bounded, redacted agent context.
- [ ] Implement `/attach`, `/detach`, and persistent session context.
- [ ] Implement live `/mode`, `/model`, `/effort`, and `/config` changes.
- [ ] Add the installable terminal UI and binary packaging pass.

## Active terminal UX pass

- [x] Make the interactive startup identity use the Crumb ASCII mark and product punchline.
- [x] Keep non-interactive output free of startup branding and animations.
- [x] Render Harness turns with model, mode, effort, cancellation guidance, and session metadata.
- [x] Add a self-clearing inline activity indicator for synchronous Harness turns.
- [x] Replace flat help output with a grouped `/` command palette.
- [x] Add read-only `/mode`, `/model`, `/effort`, `/config`, and `/plugins` surfaces.
- [x] Stream real Harness notifications into the renderer instead of projecting them after completion.
- [x] Expose bounded Pollinations web search through MCP with an explicit network-tool grant.
- [x] Add bounded queue/replace steering state and redacted Harness activity projections.
- [x] Add queued steering and richer tool-specific event projection to the Harness stream.
- [ ] Add interactive approval, tool-call, patch, error, and completion components.
- [ ] Add live model, effort, and mode selection backed by atomic configuration writes.
- [ ] Add terminal-width snapshots, reduced-motion behavior, and accessibility checks.
- [ ] Package signed standalone binaries for Linux, macOS, and Windows.

### Competitive terminal roadmap

- [x] [Stream Harness activity and support queued steering](https://github.com/elixpo/crumb.elixpo/issues/36)
- [x] [Add reviewable diffs, checkpoints, and safe rewind](https://github.com/elixpo/crumb.elixpo/issues/37)
- [x] [Ship modern input editing, shell completions, and accessibility modes](https://github.com/elixpo/crumb.elixpo/issues/38)
- [x] [Add resumable and searchable agent sessions](https://github.com/elixpo/crumb.elixpo/issues/35)
- [x] [Add opt-in background jobs and scheduled agent work](https://github.com/elixpo/crumb.elixpo/issues/34)
- [x] [Add pluggable Codex and Claude coding-agent backends](https://github.com/elixpo/crumb.elixpo/issues/39)
- [ ] [Add terminal-configurable Harness providers and explicit model selection](https://github.com/elixpo/crumb.elixpo/issues/40)
  - [x] Add the provider-neutral, non-secret endpoint and capability schema.
  - [x] Add validated atomic terminal mutations for providers, models, mode, and effort.
  - [x] Add OpenRouter/Pollinations presets and typed credential/header references.
  - [x] Add advanced provider, compatibility, pricing, retry, and model mutations.
  - [x] Project selected providers into the replaceable Harness adapter.
  - [ ] Add lazy model discovery and local compatibility diagnostics.

## Planned work packages

- [x] [WP-012 — deterministic agent/session foundation](https://github.com/elixpo/crumb.elixpo/issues/33) ([design](agent-foundation.md)).
- [x] [DeepSeek Harness + RTK/token optimizers](https://github.com/elixpo/crumb.elixpo/issues/22): [Harness adapter](deepseek-harness.md) and measured [optimizer pipeline](token-optimization.md).
- [ ] [Skills and natural-language routing](https://github.com/elixpo/crumb.elixpo/issues/25): valid commands stay native; unresolved phrases follow configured deterministic policy.
- [ ] Implement the [`/` command and `@` reference catalog](../product/input-namespaces-and-connectors.md) incrementally; neither symbol activates an AI prompt.
- [ ] Add connectors in the catalog's priority order, starting with GitHub, Google Drive, Cloudinary, email, and calendar.
- [ ] [Crumb MCP policy boundary](mcp-boundary.md): Rust-owned risk, approvals, cancellation, and workspace confinement.
- [ ] Refine the standalone CLI UI after the harness and optimizers.
  - [x] Add compact panda branding, startup readiness, and model/mode status.
  - [x] Add inline history autosuggestions alongside `/` and `@` completion.
  - [x] Add a one-line activity surface with live tool-state labels.
  - [x] Add alternate-screen terminal takeover with an inline-mode escape hatch.
  - [ ] Add the final panda application icon and platform launcher metadata.
  - [ ] Add responsive composer borders and a right-aligned context/model meter.
  - [ ] Add installer, signed release binaries, and desktop launch entries.
- [ ] Resume the `crumb.elixpo` web UI after the CLI UI.
- [ ] WP-011 — streamed AI question mode.

Later work packages remain defined in the [master plan](../product/master-plan.md). Only the current and immediately upcoming package should be expanded into implementation-level tasks.

## Validation commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```
