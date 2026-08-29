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

## Current milestone: [WP-010 — secure Pollinations BYOK](https://github.com/elixpo/crumb.elixpo/issues/24)

- [x] Add an isolated `crumb-auth` crate and secure-store abstraction.
- [x] Store Pollinations BYOK in the OS credential manager.
- [x] Add redacted, zeroizing secret ownership.
- [x] Add `crumb auth login|status|logout` commands.
- [x] Add matching `:auth login|status|logout` built-ins.
- [x] Support a process-scoped `POLLINATIONS_API_KEY` override.
- [x] Keep secrets out of command arguments, echo, history, diagnostics, and errors.
- [x] Fail safely when no secure credential store is available.
- [x] Document Linux Secret Service behavior and safe headless fallback.
- [ ] Validate Linux Secret Service behavior.
- [ ] Pass formatting, clippy, and workspace tests.
- [ ] Merge WP-010.

<details>
<summary>WP-001 completion details</summary>

- [x] Create the Cargo workspace.
- [x] Add `crumb-cli`, `crumb-core`, `crumb-repl`, and `crumb-platform`.
- [x] Detect Linux, macOS, and Windows with a typed `Platform` enum.
- [x] Render a minimal prompt containing the working directory.
- [x] Implement `:exit`, `:version`, and `:platform`.
- [x] Classify all other input as `NativeInput` without executing it.
- [x] Add unit tests and cross-platform GitHub Actions.
- [x] Pass formatting, clippy, and workspace tests.
- [x] Publish WP-001 to `main`.

</details>

## Planned work packages

- [ ] [DeepSeek Harness + RTK/token optimizers](https://github.com/elixpo/crumb.elixpo/issues/22).
- [ ] [Skills and natural-language routing](https://github.com/elixpo/crumb.elixpo/issues/25).
- [ ] WP-011 — streamed AI question mode.

Later work packages remain defined in the [master plan](../product/master-plan.md). Only the current and immediately upcoming package should be expanded into implementation-level tasks.

## Validation commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```
