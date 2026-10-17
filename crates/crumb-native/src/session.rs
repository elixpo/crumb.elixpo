//! Managed, persistent native-shell command sessions.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use crumb_pty::{PtyBackend, PtyInput, PtyProcess, PtyResizer, TerminalSize};

use crate::protocol::{CommandCompletion, CompletionProtocol};
use crate::{NativeShell, ShellKind};

const CURSOR_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_ORIGIN: &[u8] = b"\x1b[1;1R";

/// Result of submitting one native command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    Completed(CommandCompletion),
    ShellExited,
}

/// One command submitted to a managed shell and awaiting its lifecycle frame.
#[must_use = "a submitted shell command must be driven to completion"]
pub struct SubmittedCommand<'a> {
    session: &'a mut ShellSession,
    sequence: u64,
}

impl SubmittedCommand<'_> {
    /// Streams command output until its completion frame or shell exit.
    ///
    /// # Errors
    ///
    /// Returns an error if PTY output fails or the lifecycle frame is invalid.
    pub fn wait(self, output: &mut dyn Write) -> Result<CommandOutcome> {
        let Some(completion) = self.session.read_until(self.sequence, output)? else {
            return Ok(CommandOutcome::ShellExited);
        };
        self.session.cwd.clone_from(&completion.cwd);
        Ok(CommandOutcome::Completed(completion))
    }
}

/// A persistent native shell controlled through hidden completion frames.
pub struct ShellSession {
    kind: ShellKind,
    process: PtyProcess,
    reader: Box<dyn Read + Send>,
    protocol: CompletionProtocol,
    terminal_control: TerminalControl,
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
        #[cfg(target_os = "windows")]
        eprintln!("native shell spawned");
        let reader = process.try_clone_reader()?;
        let protocol = CompletionProtocol::new();
        let bootstrap = protocol.bootstrap(kind);
        let readiness = protocol.submission(kind, no_op_command(kind), 0);
        process.write_input(format!("{bootstrap}{readiness}").as_bytes())?;
        #[cfg(target_os = "windows")]
        eprintln!("native shell initialization written");

        let mut session = Self {
            kind,
            process,
            reader,
            protocol,
            terminal_control: TerminalControl::default(),
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
        self.submit(command)?.wait(output)
    }

    /// Submits one command before its terminal input relay starts.
    ///
    /// # Errors
    ///
    /// Returns an error if the sequence counter overflows or command input
    /// cannot be written to the PTY.
    pub fn submit(&mut self, command: &str) -> Result<SubmittedCommand<'_>> {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow!("shell command sequence overflowed"))?;
        let submission = self.protocol.submission(self.kind, command, sequence);
        self.process.write_input(submission.as_bytes())?;
        Ok(SubmittedCommand {
            session: self,
            sequence,
        })
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
            let (visible, cursor_queries) = self.terminal_control.filter(&decoded.visible);
            for _ in 0..cursor_queries {
                self.process.write_input(CURSOR_ORIGIN)?;
            }
            if !visible.is_empty() {
                output.write_all(&visible)?;
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

#[derive(Default)]
struct TerminalControl {
    pending: Vec<u8>,
}

impl TerminalControl {
    fn filter(&mut self, input: &[u8]) -> (Vec<u8>, usize) {
        self.pending.extend_from_slice(input);
        let mut visible = Vec::with_capacity(self.pending.len());
        let mut cursor_queries = 0;

        loop {
            let Some(start) = find_subslice(&self.pending, CURSOR_QUERY) else {
                let retained = matching_suffix_len(&self.pending, CURSOR_QUERY);
                let visible_end = self.pending.len() - retained;
                visible.extend(self.pending.drain(..visible_end));
                break;
            };

            visible.extend(self.pending.drain(..start));
            self.pending.drain(..CURSOR_QUERY.len());
            cursor_queries += 1;
        }

        (visible, cursor_queries)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn matching_suffix_len(input: &[u8], prefix: &[u8]) -> usize {
    let maximum = input.len().min(prefix.len().saturating_sub(1));
    (1..=maximum)
        .rev()
        .find(|length| input.ends_with(&prefix[..*length]))
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::TerminalControl;

    #[test]
    fn cursor_queries_are_hidden_and_counted_across_chunks() {
        let mut control = TerminalControl::default();

        let (first, first_queries) = control.filter(b"before\x1b[");
        let (second, second_queries) = control.filter(b"6nafter\x1b[6n");

        assert_eq!(first, b"before");
        assert_eq!(first_queries, 0);
        assert_eq!(second, b"after");
        assert_eq!(second_queries, 2);
    }
}
