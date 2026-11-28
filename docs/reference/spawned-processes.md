# Spawned processes

Crumb starts child processes for the shell tools, external harnesses, MCP
servers, git worktrees, and small pieces of desktop integration. This page is
the inventory of those call sites: what each one runs, whether the child
inherits Crumb's environment, whether it runs inside the agent sandbox, and
whether the user approves it first.

Environment entries say **cleared** when the call site calls `env_clear()`
before spawning. Some cleared call sites then set a small number of variables
again; those are named in the entry. Sandbox entries refer to the Landlock
confinement applied through `crumb __sandbox-exec` (see
`crates/crumb-tools/src/sandbox.rs`): the launcher restricts itself so it can
only write inside the workspace, temporary directories, `/dev`, and toolchain
caches, then replaces itself with the real program. The sandbox is best
effort: on kernels without Landlock, and on other platforms, the command runs
unconfined.

| Crate | File | What it runs | Environment cleared | In the sandbox | User approves first |
| --- | --- | --- | --- | --- | --- |
| crumb-cli | `src/device_auth.rs:221` | `xdg-open <url>` (Linux) opens the account device-login URL in the default browser | No | No | No separate prompt; part of the user-invoked `crumb auth login` flow |
| crumb-cli | `src/device_auth.rs:225` | `open <url>` (macOS), same function | No | No | No separate prompt; part of the user-invoked `crumb auth login` flow |
| crumb-cli | `src/device_auth.rs:229` | `cmd /C start <url>` (Windows), same function | No | No | No separate prompt; part of the user-invoked `crumb auth login` flow |
| crumb-cli | `src/hooks.rs:154` | A hook command from the user's settings, through `sh -c` (Windows: `cmd /C`), with the event payload on stdin | No; adds `CRUMB_HOOK_EVENT` and `CRUMB_WORKSPACE` | No | No; hooks are commands the user configured, and they run automatically when the event fires |
| crumb-cli | `src/main.rs:176` | Crumb itself, re-executed as `__sandbox-exec`: applies the Landlock ruleset to itself, then replaces itself with the target program | Not at this site; the caller in crumb-tools clears it before spawning the launcher | Yes; this is the sandbox entry point | Internal mechanism; the enclosing tool call was authorized before the launcher was spawned |
| crumb-cli | `src/main.rs:1118` | Crumb itself (`jobs run <id>`) as the worker for a stored job that has come due | No | No | No prompt at this site; jobs are definitions the user stored, launched when due |
| crumb-cli | `src/main.rs:13686` | `open <path>` (macOS) shows a generated image artifact full-size | No | No | Yes; direct response to the user's `o` keypress in the artifact view |
| crumb-cli | `src/main.rs:13693` | `cmd /C start <path>` (Windows), same function | No | No | Yes; direct response to the user's `o` keypress in the artifact view |
| crumb-cli | `src/main.rs:13700` | `xdg-open <path>` (Linux), same function | No | No | Yes; direct response to the user's `o` keypress in the artifact view |
| crumb-cli | `src/notify.rs:59` | `notify-send` raises a desktop notification when a long turn finishes | No | No | No; automatic, and disabled by `CRUMB_NOTIFICATIONS=off` |
| crumb-cli | `src/post_edit_check.rs:112` | The project's post-edit check: a detected tool (`cargo check`, `tsc --noEmit`, `go vet`, `ruff check`) or the configured `checks.command` through `sh -c` / `cmd /C` | No | No | No; runs automatically at the end of an editing turn when checks are enabled in settings |
| crumb-graph | `src/graphify.rs:207` | The configured Graphify executable, running an extraction build over the workspace | Yes; only `PYTHONHASHSEED=0` plus `PATH`, `SYSTEMROOT`, `WINDIR` are set afterwards | No | Yes; runs from the user's `/graph build` or `/graph refresh` command |
| crumb-graph | `src/graphify.rs:272` | The Graphify executable (or `graphify-mcp`) with `--version` or `--help`, probing availability and version | Yes; `PATH`, `SYSTEMROOT`, `WINDIR` are re-added | No | No; automatic probes behind status checks |
| crumb-harness-cli | `src/lib.rs:82` | The explicitly selected external coding CLI (the Codex or Claude backend executable), with the prompt on stdin | No; the backend inherits Crumb's environment and uses its own credentials | No; runs in its own process group | No per-launch prompt in this path; the backend is explicitly selected by the user |
| crumb-harness-cli | `src/lib.rs:343` | `taskkill /PID <pid> /T /F` (Windows) terminates the backend process tree | No | No | No; cleanup on cancellation, timeout, or error |
| crumb-harness-dsh | `src/process.rs:114` | The configured DSH Harness SDK process | Yes; only the explicit harness environment values are set afterwards | No; runs in its own process group | No per-launch prompt at this site; the harness is user-configured and launched lazily on first use |
| crumb-harness | `src/worktree.rs:230` | `git apply --binary`, with the patch on stdin, merging a finished helper worktree | No | No | No separate check at this site; internal to parallel-agent worktree handling |
| crumb-harness | `src/worktree.rs:259` | `git` with varying arguments; the shared helper behind worktree add, remove, diff, and related operations | No | No | No separate check at this site; internal to parallel-agent worktree handling |
| crumb-mcp | `src/client.rs:139` | A configured external MCP server (`server.command` with its configured arguments) over stdio | Yes; `PATH`, `SYSTEMROOT`, `WINDIR` are re-added | No | No prompt for starting; servers come from the user's configuration and start when the session registers them. The tools they expose carry the ProcessExecution risk class, and calls to them pass the approval broker |
| crumb-optimize | `src/lib.rs:161` | The configured RTK optimizer executable with `pipe --filter <kind>`, filtering tool output | Yes; the runtime `PATH` is copied back in | No | No; runs automatically on tool output when the optimizer is enabled in configuration |
| crumb-tools | `src/disposable.rs:110` | The configured interpreter for the `run_disposable` tool, running a model-supplied script in a fresh temporary directory | Yes; the configured `PATH` is set when one is configured | No; confinement is the disposable directory and the cleared environment, not the OS sandbox | Yes; the approval broker must authorize every unconfigured call |
| crumb-tools | `src/github.rs:251` | `gh auth token`, reading a GitHub token when `GH_TOKEN`/`GITHUB_TOKEN` are not set | No; adds `GH_PROMPT_DISABLED` and `GIT_TERMINAL_PROMPT` | No | No; internal credential lookup for the GitHub issue tool |
| crumb-tools | `src/shell.rs:213` | The configured command rewriter (for example `rtk`) with `rewrite <command>`, preparing a shell tool call | Yes, unless the session inherits the environment; the configured `PATH` is restored | No | No; runs during prepare, before the approval decision on the shell call it rewrites |
| crumb-tools | `src/shell.rs:398` | The sandbox launcher (Crumb's own executable) with `__sandbox-exec --write <dir>… -- <program> <args>…`; the `run_shell` and background-job spawn when a sandbox is configured | Cleared by the caller unless the session inherits the environment; the configured `PATH` is restored | Yes; Landlock write confinement, best effort | The enclosing tool call is authorized by the approval broker before this spawn |
| crumb-tools | `src/shell.rs:406` | The configured shell program directly, when no sandbox launcher is configured: `run_shell` without a sandbox, every `run_shell_outside_sandbox` call, and background jobs without a sandbox | Cleared by the caller unless the session inherits the environment; the configured `PATH` is restored | No | The enclosing tool call is authorized by the approval broker before this spawn; `run_shell_outside_sandbox` additionally requires the user's approval for every call |
| crumb-ui | `src/lib.rs:2452` | `git -C <cwd>` with read-only queries (`symbolic-ref`, `rev-parse`, `status --porcelain`) for the status display | No | No | No; automatic, read-only UI refresh |

## Scope and method

The inventory comes from searching the crates for `Command::new`, which is
the acceptance test the issue states. At the time of writing that search
returns 29 call sites. Three of them are test code and are excluded:

- `crates/crumb-graph/src/graphify.rs:395`, a fixture script writer inside
  `#[cfg(test)] mod tests`;
- `crates/crumb-harness/src/parallel.rs:1074`, a `git` helper inside
  `#[cfg(test)] mod tests`;
- `crates/crumb-harness-cli/src/lib.rs:469`, a fixture process inside
  `#[cfg(test)] mod tests`.

Platform variants selected with `#[cfg(target_os)]` (the browser opener in
`device_auth.rs` and the image opener in `main.rs`) are counted as one row
per source line, matching the search.

One spawn path does not appear in the search results because it does not
call `Command::new` itself: background jobs. `BackgroundJobs::start` in
`crates/crumb-tools/src/background.rs` builds its `Command` through
`crate::shell::shell_command` — the same construction as the
`src/shell.rs:398` and `src/shell.rs:406` rows above — then calls
`env_clear()` on it when the session does not inherit the environment, and
spawns it. Background jobs are therefore covered by those two rows, and
`shell_command` has no other callers: the shell tool's `run_shell` path and
background jobs are the only two.
