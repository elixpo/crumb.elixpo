# crumb

`crumb` is a cross-platform, AI-native terminal layer over Bash on Linux, Zsh on macOS, and PowerShell on Windows. Native shell behavior remains available even when AI is disabled or offline.

The project is in its initial bootstrap phase. See [the master plan](ai_native_shell_master_plan.md) for the architecture and [the Pollinations reference](pollinations.md) for the planned provider integration.

## Development

Install Rust with [rustup](https://rustup.rs/), including `rustfmt` and `clippy`:

```bash
rustup component add rustfmt clippy
```

Then validate the workspace:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

## Status

WP-001 builds the workspace, platform detection, typed input events, and a minimal REPL. Native command execution and Pollinations integration intentionally begin in later work packages.
