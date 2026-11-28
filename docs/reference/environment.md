# Environment variables

This page lists every `CRUMB_*` environment variable that Crumb's non-test
code reads or sets. Each entry gives what it does, the values it accepts, the
default, and the file that reads it.

Variables in the "Set by Crumb" section are written by Crumb for child
processes. You do not set them yourself.

## Variables you can set

### Display and terminal

| Variable | What it does | Accepted values | Default | Read in |
| --- | --- | --- | --- | --- |
| `CRUMB_FULLSCREEN` | Opts into the alternate-screen (full-screen) view. | Any value except empty, `0`, `off`, `false` or `no` (case-insensitive) turns it on. | Off, so Crumb runs inline in the terminal's scrollback. | `crates/crumb-cli/src/main.rs` (`fullscreen_requested`) |
| `CRUMB_INLINE` | Forces the inline view. When it is set to anything, including an empty value, it wins over `CRUMB_FULLSCREEN`. | Any value. Only whether it is set matters. | Unset. | `crates/crumb-cli/src/main.rs` (`fullscreen_requested`) |
| `CRUMB_NOTIFICATIONS` | Turns the end-of-task terminal notification off. | `0`, `off`, `false` or `no` (case-insensitive) disables it. Any other value, or unset, leaves it on. | On. | `crates/crumb-cli/src/notify.rs` (`sequence`) |
| `CRUMB_PLAIN` | Plain output: no color and no rich rendering. Crumb also goes plain on its own when it is not attached to a terminal. | Any value except empty, `0`, `false` or `no`. | Off. | `crates/crumb-ui/src/lib.rs` (`UiSettings::from_environment`) |
| `CRUMB_SCREEN_READER` | Screen-reader output: plain text, reduced motion and no startup branding. | Any value except empty, `0`, `false` or `no`. | Off. | `crates/crumb-ui/src/lib.rs` (`UiSettings::from_environment`) |
| `CRUMB_REDUCED_MOTION` | Reduces animation. | Any value except empty, `0`, `false` or `no`. | Off. | `crates/crumb-ui/src/lib.rs` (`UiSettings::from_environment`) |
| `CRUMB_BRANDING` | Chooses the startup banner. | `compact` for a one-line banner. `off`, `none` or `disabled` hides it. Any other value gives the full banner. | Full banner. The banner is always hidden outside a terminal and in screen-reader mode. | `crates/crumb-ui/src/lib.rs` (`UiSettings::from_environment`) |
| `CRUMB_MOOD` | Chooses the panda shown in the banner. | `awake` or `cool`. | `cool` when an agent is configured, otherwise `awake`. | `crates/crumb-ui/src/lib.rs` (`panda_mood`) |

`CRUMB_PLAIN`, `CRUMB_SCREEN_READER` and `CRUMB_REDUCED_MOTION` use a stricter
"off" list than `CRUMB_FULLSCREEN` and `CRUMB_NOTIFICATIONS`: the first three
compare the value as written, so `OFF` or `False` counts as on.

### Account

| Variable | What it does | Accepted values | Default | Read in |
| --- | --- | --- | --- | --- |
| `CRUMB_ACCOUNT_URL` | Base URL of the Crumb account site used by `crumb auth login`. Use it to test against a local web app. | An `https://` URL, or `http://` only for `localhost` or `127.0.0.1`. Anything else is rejected. | `https://crumb.elixpo.com` | `crates/crumb-cli/src/device_auth.rs` (`connect`, `trusted_origin`) |

### `crumb exec` custom endpoint

These three work together. `--model exec/<model>` needs `CRUMB_EXEC_BASE_URL`,
and `CRUMB_EXEC_BASE_URL` needs `--model exec/<model>`. Setting only one of the
two is an error.

| Variable | What it does | Accepted values | Default | Read in |
| --- | --- | --- | --- | --- |
| `CRUMB_EXEC_BASE_URL` | Base URL of an OpenAI-compatible endpoint for `crumb exec`. Blank values count as unset. | A URL. | Unset. | `crates/crumb-cli/src/main.rs` (`EXEC_BASE_URL_ENV`) |
| `CRUMB_EXEC_API_KEY` | API key sent to that endpoint. It is passed to the harness as an environment credential reference, not written to config. | Any string. | Unset, so no credential is sent. | `crates/crumb-cli/src/main.rs` (`EXEC_API_KEY_ENV`) |
| `CRUMB_EXEC_PROTOCOL` | Wire protocol of that endpoint. | `openai_completions` or `openai_responses`. Empty counts as the default. Any other value is an error. | `openai_completions` | `crates/crumb-cli/src/main.rs` (`EXEC_PROTOCOL_ENV`) |

### Harness

| Variable | What it does | Accepted values | Default | Read in |
| --- | --- | --- | --- | --- |
| `CRUMB_CORDIS_CONFIG` | Path to the harness composition file. A relative path is resolved against the workspace. A path passed in code takes priority over this variable. | A file path. | `config/harness/crumb.cordis.yml` | `crates/crumb-cli/src/agent_runtime.rs` (`resolve_composition`) |

## Set by Crumb

Crumb sets these for the processes it starts. They are not settings.

| Variable | Set for | Value | Written in |
| --- | --- | --- | --- |
| `CRUMB_HOOK_EVENT` | Hook commands. | The event name that fired the hook. | `crates/crumb-cli/src/hooks.rs` |
| `CRUMB_WORKSPACE` | Hook commands. | The workspace directory. | `crates/crumb-cli/src/hooks.rs` |
| `CRUMB_MCP_COMMAND` | The external harness. The composition file uses it as the command that starts Crumb's MCP server. | The path of the running `crumb` executable. | `crates/crumb-cli/src/agent_runtime.rs` |
| `CRUMB_HARNESS_PROVIDERS` | The external harness. | JSON describing the selected provider profile. Header values that reference an environment variable are resolved by the composition file. | `crates/crumb-cli/src/agent_runtime.rs` |
| `CRUMB_PROVIDER_API_KEY` | The external harness, when the selected provider's credential is in the OS keyring. | The keyring secret. It is only placed in the child's environment. | `crates/crumb-cli/src/provider_projection.rs` |

## How this list was made

Search used:

```bash
grep -rnE "CRUMB_[A-Z0-9_]+" crates --include=*.rs
```

Every hit outside `#[cfg(test)]` modules and `tests/` directories was read at
its call site. Three kinds of hit are not environment variables:
`CRUMB_BADGE` in `crates/crumb-ui/src/lib.rs` is a Rust constant for the banner
text, `DEFAULT_CRUMB_ORIGIN` in `crates/crumb-cli/src/device_auth.rs` is the
default for `CRUMB_ACCOUNT_URL`, and `__CRUMB_FRAME__` and `__CRUMB_END__` in
`crates/crumb-native/src/protocol.rs` are terminal frame markers.
`CRUMB_MCP_ENVIRONMENT_TEST_CHILD` and `CRUMB_MCP_TEST_SECRET` in
`crates/crumb-mcp/src/client.rs` only appear in that file's test module.
