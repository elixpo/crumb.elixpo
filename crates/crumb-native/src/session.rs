//! Managed, persistent native-shell command sessions.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use crumb_pty::{PtyBackend, PtyInput, PtyProcess, PtyResizer, TerminalSize};

use crate::protocol::{CommandCompletion, CompletionProtocol};
use crate::{NativeShell, ShellKind};

/// Result of submitting one native command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    Completed(CommandCompletion),
    ShellExited,
}

/// A persistent native shell controlled through hidden completion frames.
pub struct ShellSession {
    kind: ShellKind,
    process: PtyProcess,
    reader: Box<dyn Read + Send>,
    protocol: CompletionProtocol,
    next_sequence: u64,
    cwd: PathBuf,
}

impl ShellSession {
    /// Starts and initializes a persistent native shell.
    ///
    /// Startup output from user shell configuration is consumed before this
    /// function returns so it cannot corrupt crumb's prompt.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY or shell cannot start, initialization cannot
    /// be written, or the readiness completion frame cannot be decoded.
    pub fn start(
        shell: &dyn NativeShell,
        backend: &dyn PtyBackend,
        size: TerminalSize,
    ) -> Result<Self> {
        let kind = shell.kind();
        let mut process = shell.spawn(backend, size)?;
        let reader = process.try_clone_reader()?;
        let protocol = CompletionProtocol::new();
        let bootstrap = bootstrap_command(kind);
        let readiness = protocol.submission(kind, no_op_command(kind), 0);
        process.write_input(format!("{bootstrap}{readiness}").as_bytes())?;

        let mut session = Self {
            kind,
            process,
            reader,
            protocol,
            next_sequence: 1,
            cwd: PathBuf::new(),
        };
        let mut startup_sink = std::io::sink();
        let completion = session.read_until(0, &mut startup_sink)?.ok_or_else(|| {
            anyhow!("native shell exited before lifecycle initialization completed")
        })?;
        session.cwd.clone_from(&completion.cwd);
        Ok(session)
    }

    /// Executes one command in the persistent shell and streams visible output.
    ///
    /// # Errors
    ///
    /// Returns an error if command input/output fails or its lifecycle frame is
    /// invalid. A normal shell exit is returned as [`CommandOutcome::ShellExited`].
    pub fn execute(&mut self, command: &str, output: &mut dyn Write) -> Result<CommandOutcome> {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow!("shell command sequence overflowed"))?;
        let submission = self.protocol.submission(self.kind, command, sequence);
        self.process.write_input(submission.as_bytes())?;
        let Some(completion) = self.read_until(sequence, output)? else {
            return Ok(CommandOutcome::ShellExited);
        };
        self.cwd.clone_from(&completion.cwd);
        Ok(CommandOutcome::Completed(completion))
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Resizes the underlying PTY.
    ///
    /// # Errors
    ///
    /// Returns an error if the operating system rejects the resize.
    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        self.process.resize(size)
    }

    /// Returns a synchronized handle for forwarding foreground terminal input.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY input stream is no longer available.
    pub fn try_clone_input(&self) -> Result<PtyInput> {
        self.process.try_clone_input()
    }

    #[must_use]
    pub fn resizer(&self) -> PtyResizer {
        self.process.resizer()
    }

    /// Requests a normal shell exit and waits for process cleanup.
    ///
    /// # Errors
    ///
    /// Returns an error if the exit request cannot be written or the child
    /// status cannot be collected.
    pub fn shutdown(mut self) -> Result<()> {
        self.process
            .write_input(exit_command(self.kind).as_bytes())?;
        self.process.wait()
    }

    fn read_until(
        &mut self,
        expected_sequence: u64,
        output: &mut dyn Write,
    ) -> Result<Option<CommandCompletion>> {
        let mut buffer = [0_u8; 8192];
        loop {
            let bytes_read = self.reader.read(&mut buffer)?;
            if bytes_read == 0 {
                self.process.wait()?;
                return Ok(None);
            }

            let decoded = self.protocol.decode(&buffer[..bytes_read])?;
            if !decoded.visible.is_empty() {
                output.write_all(&decoded.visible)?;
                output.flush()?;
            }
            if let Some(completion) = decoded
                .completions
                .into_iter()
                .find(|completion| completion.sequence == expected_sequence)
            {
                return Ok(Some(completion));
            }
        }
    }
}

fn bootstrap_command(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::Bash => "stty -echo; PS1=''; PROMPT_COMMAND=''\n",
        ShellKind::Zsh => "stty -echo; PS1=''; RPS1=''; unsetopt zle prompt_cr prompt_sp\n",
        ShellKind::PowerShell => {
            "Remove-Module PSReadLine -ErrorAction SilentlyContinue; function global:prompt { '' }\r\n"
        }
    }
}

fn no_op_command(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::Bash | ShellKind::Zsh => ":",
        ShellKind::PowerShell => "$null = $true",
    }
}

fn exit_command(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::Bash | ShellKind::Zsh => "exit\n",
        ShellKind::PowerShell => "exit\r\n",
    }
}
