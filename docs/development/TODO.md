# Development TODO

This is the local source of truth for implementation order. GitHub issues provide discussion and ownership; completed work remains checked here for quick repository context.

## Completed milestones

- [x] [WP-001 — workspace bootstrap](https://github.com/elixpo/crumb.elixpo/issues/1)
- [x] [WP-002 — persistent Linux Bash PTY](https://github.com/elixpo/crumb.elixpo/issues/4)

## Current milestone: [WP-003 — macOS Zsh parity](https://github.com/elixpo/crumb.elixpo/issues/6)

- [x] Add a typed macOS Zsh implementation.
- [x] Select interactive Zsh for `Platform::MacOs`.
- [x] Reuse the portable PTY lifecycle and terminal relay.
- [x] Preserve cwd and environment state in one Zsh process.
- [x] Add deterministic macOS integration coverage.
- [ ] Keep Linux and Windows workspace builds green.
- [ ] Pass formatting, clippy, and workspace tests.
- [ ] Merge WP-003.

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
