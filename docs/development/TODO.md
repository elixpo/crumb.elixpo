# Development TODO

This is the local source of truth for implementation order. GitHub issues provide discussion and ownership; completed work remains checked here for quick repository context.

## Completed milestones

- [x] [WP-001 — workspace bootstrap](https://github.com/elixpo/crumb.elixpo/issues/1)

## Current milestone: [WP-002 — persistent Linux Bash PTY](https://github.com/elixpo/crumb.elixpo/issues/4)

- [x] Add isolated `crumb-pty` and `crumb-native` crates.
- [x] Define PTY and native-shell lifecycle abstractions.
- [x] Launch interactive Bash inside a PTY on Linux.
- [x] Forward terminal input and output.
- [x] Preserve working-directory and environment state between commands.
- [ ] Forward terminal resize events.
- [ ] Forward Ctrl+C without terminating crumb.
- [ ] Add focused unit and Linux integration tests.
- [ ] Pass formatting, clippy, and workspace tests.
- [ ] Merge WP-002.

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

- [ ] WP-003 — macOS Zsh parity.
- [ ] WP-004 — Windows PowerShell parity.
- [ ] WP-005 — shell lifecycle and completion protocol.
- [ ] WP-006 — terminal UI layer.
- [ ] WP-007 — persistent history.
- [ ] WP-008 — provider-neutral LLM interface.
- [ ] WP-009 — Pollinations adapter.
- [ ] WP-010 — secure authentication.
- [ ] WP-011 — streamed AI question mode.

Later work packages remain defined in the [master plan](../product/master-plan.md). Only the current and immediately upcoming package should be expanded into implementation-level tasks.

## Validation commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```
