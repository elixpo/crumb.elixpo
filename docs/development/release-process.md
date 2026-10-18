# Release process

Crumb's native CLI remains the primary product. Release staging is separate
from publication so an unsigned build can never become a public release by
accident.

## Supported staging builds

The `Release Staging` workflow builds the locked `crumb-cli` package on Linux,
macOS, and Windows. Every staging bundle contains the binary, project license,
README, an SPDX software bill of materials, a SHA-256 checksum, and GitHub build
provenance. Staging artifacts expire after 14 days and are not published.

The macOS artifact is intentionally labelled `universal-pending` until both
Apple architectures are built and combined. It must not be advertised as a
universal binary before that release gate is complete.

## Publication gate

A maintainer may publish only after all of the following are true:

- terminal parity tests cover foreground interruption, resize, alternate
  screens, prompts, pagers, background output, and persistent shell state;
- Windows Authenticode and Apple Developer ID/notarization credentials are
  configured in protected environments;
- clean install, upgrade, rollback, uninstall, PATH, and launcher tests pass on
  every supported platform;
- the security review and migration/rollback policy are approved;
- signed updater metadata and its revocation procedure are tested; and
- a maintainer explicitly approves the protected publish job.

Until those gates are met, the website must not present staging bundles as
supported downloads.

## Desktop direction

The CLI and native Rust harness own tools, permissions, sessions, memory, and
execution policy. A future Tauri desktop app may be a thin frontend over a
UI-neutral runtime/event API. It must not fork or replace the harness. This
keeps one security boundary while allowing desktop-only conveniences such as
image clipboard access, drag and drop, previews, and session browsing later.
