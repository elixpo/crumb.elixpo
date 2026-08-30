# Token optimization

Crumb optimizes only output sent to an agent. Native shell output remains exact.
The enforced order is secret redaction, deterministic command-aware filtering,
optional external optimization, then a hard byte ceiling. An unavailable,
failing, or slower-than-budget optimizer falls back to the locally filtered
payload and never blocks terminal startup.

## Linux setup

Rust requires the existing stable toolchain (`rustup`, `cargo`, and `rustc`).
RTK is optional: place an `rtk` executable on `PATH` and confirm it with
`rtk --version`. Crumb needs no RTK library or daemon. `/doctor` reports whether
the configured executable is available without starting it.

The checked example enables:

```json
{"id":"rtk","command":"rtk","arguments":[],"enabled":true}
```

RTK receives only UTF-8 output after Crumb's secret redaction. It runs through
bounded `rtk pipe` filters in a cleared environment, has a hard timeout, and is
used only when its result is smaller. Tool metadata reports input, output,
saved bytes, redacted lines, and the optimizer selected.

## TOON policy

TOON is not automatically assumed to be cheaper. Crumb selects a supplied TOON
candidate only when decoding proves it is equivalent to the JSON value and its
encoded bytes are strictly smaller. Irregular structures, failed round trips,
and protocol payloads remain JSON. This keeps TOON optional and replaceable.
