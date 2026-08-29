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

## Current milestone: [WP-007 — persistent history](https://github.com/elixpo/crumb.elixpo/issues/15)

- [x] Add an isolated `crumb-history` crate.
- [x] Persist history in local SQLite.
- [x] Store command, cwd, platform/mode, exit status, and timestamp.
- [x] Exclude empty and likely sensitive commands.
- [x] Provide recent and literal text-search queries.
- [x] Integrate recording into managed native execution.
- [x] Add `:history` and `:history search` built-ins.
- [ ] Provide interactive Ctrl+R search.
- [x] Add migration and persistence tests.
- [ ] Pass formatting, clippy, and workspace tests.
- [ ] Merge WP-007.

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
