//! Native-shell selection and lifecycle boundaries.

use std::error::Error;
use std::fmt;

use anyhow::Result;
use crumb_platform::Platform;
use crumb_pty::{CommandSpec, PtyBackend, PtyProcess, TerminalSize};

/// Native shell families supported by crumb's architecture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellKind {
    Bash,
    Zsh,
    PowerShell,
}

/// Common lifecycle contract for an interactive native shell.
pub trait NativeShell {
    #[must_use]
    fn kind(&self) -> ShellKind;

    #[must_use]
    fn command_spec(&self) -> CommandSpec;

    /// Starts the native shell with the selected PTY backend.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot allocate a PTY or start the
    /// shell process.
    fn spawn(&self, backend: &dyn PtyBackend, size: TerminalSize) -> Result<PtyProcess> {
        backend.spawn(&self.command_spec(), size)
    }
}

/// Linux Bash implementation for WP-002.
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxBash;

impl NativeShell for LinuxBash {
    fn kind(&self) -> ShellKind {
        ShellKind::Bash
    }

    fn command_spec(&self) -> CommandSpec {
        CommandSpec::new("bash").arg("-i")
    }
}

/// Error returned when the active platform's shell backend is not built yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedPlatform(pub Platform);

impl fmt::Display for UnsupportedPlatform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "native shell support is not implemented for {}",
            self.0
        )
    }
}

impl Error for UnsupportedPlatform {}

/// Selects the WP-002 native shell for a platform.
///
/// # Errors
///
/// Returns [`UnsupportedPlatform`] until the macOS and Windows parity work
/// packages are implemented.
pub fn shell_for(platform: Platform) -> Result<Box<dyn NativeShell>, UnsupportedPlatform> {
    match platform {
        Platform::Linux => Ok(Box::new(LinuxBash)),
        Platform::MacOs | Platform::Windows => Err(UnsupportedPlatform(platform)),
    }
}

#[cfg(test)]
mod tests {
    use crumb_platform::Platform;

    use super::{ShellKind, shell_for};

    #[test]
    fn linux_selects_interactive_bash() {
        let shell = shell_for(Platform::Linux).expect("Linux should be supported");
        let command = shell.command_spec();

        assert_eq!(shell.kind(), ShellKind::Bash);
        assert_eq!(command.program(), "bash");
        assert_eq!(command.args(), ["-i"]);
    }

    #[test]
    fn parity_platforms_are_explicitly_deferred() {
        assert!(shell_for(Platform::MacOs).is_err());
        assert!(shell_for(Platform::Windows).is_err());
    }
}
