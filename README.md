# crumb

`crumb` is a cross-platform, AI-native terminal layer over Bash on Linux, Zsh on macOS, and PowerShell on Windows. Native shell behavior remains available even when AI is disabled or offline.

The project is in its initial bootstrap phase. See [the documentation index](docs/README.md), [the master plan](docs/product/master-plan.md), and [the Pollinations reference](docs/reference/pollinations.md).

## Workspace

```text
crates/
├── crumb-cli       # `crumb` executable and startup lifecycle
├── crumb-core      # provider-neutral domain types
├── crumb-platform  # typed host-platform detection
└── crumb-repl      # prompt, built-ins, and input classification

docs/
├── development     # active roadmap and engineering notes
├── product         # product and architecture specifications
└── reference       # external integration references
```

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

WP-001 builds the workspace, platform detection, typed input events, and a minimal REPL. Native command execution and Pollinations integration intentionally begin in later work packages. Current progress is tracked in [the development TODO](docs/development/TODO.md).
