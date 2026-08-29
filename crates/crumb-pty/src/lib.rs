//! PTY primitives isolated from crumb's shell and UI layers.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Initial or updated dimensions for a pseudoterminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl TerminalSize {
    #[must_use]
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(size: TerminalSize) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        }
    }
}

/// Provider-neutral description of a process launched inside a PTY.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
}

impl CommandSpec {
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    #[must_use]
    pub fn working_directory(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    fn to_command_builder(&self) -> CommandBuilder {
        let mut command = CommandBuilder::new(&self.program);
        for arg in &self.args {
            command.arg(arg);
        }
        if let Some(path) = &self.current_dir {
            command.cwd(path);
        }
        command
    }
}

/// Backend boundary for creating PTY-backed child processes.
pub trait PtyBackend {
    /// Starts `command` inside a new pseudoterminal.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY cannot be allocated or the child cannot be
    /// spawned.
    fn spawn(&self, command: &CommandSpec, size: TerminalSize) -> Result<PtyProcess>;
}

/// The operating system's native PTY backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemPty;

impl PtyBackend for SystemPty {
    fn spawn(&self, command: &CommandSpec, size: TerminalSize) -> Result<PtyProcess> {
        let pair = native_pty_system().openpty(size.into())?;
        let child = pair.slave.spawn_command(command.to_command_builder())?;
        drop(pair.slave);
        let writer = pair.master.take_writer()?;

        Ok(PtyProcess {
            master: pair.master,
            writer,
            child,
        })
    }
}

/// A live child process and its controlling pseudoterminal.
pub struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtyProcess {
    /// Clones a reader for the PTY output stream.
    ///
    /// # Errors
    ///
    /// Returns an error if the operating-system PTY reader cannot be cloned.
    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader()
    }

    /// Writes bytes to the PTY input stream and flushes them immediately.
    ///
    /// # Errors
    ///
    /// Returns an error if writing or flushing the PTY fails.
    pub fn write_input(&mut self, input: &[u8]) -> Result<()> {
        self.writer.write_all(input)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Updates the terminal dimensions seen by the child process.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform cannot resize the PTY.
    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        self.master.resize(size.into())
    }

    /// Requests termination of the child process.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform cannot signal the child.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill()
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandSpec, TerminalSize};

    #[test]
    fn terminal_size_defaults_to_no_pixel_dimensions() {
        let size = TerminalSize::new(24, 80);

        assert_eq!(size.rows, 24);
        assert_eq!(size.cols, 80);
        assert_eq!(size.pixel_width, 0);
        assert_eq!(size.pixel_height, 0);
    }

    #[test]
    fn command_spec_preserves_program_arguments_and_directory() {
        let command = CommandSpec::new("bash")
            .arg("-i")
            .current_dir("/tmp/project");

        assert_eq!(command.program(), "bash");
        assert_eq!(command.args(), ["-i"]);
        assert_eq!(command.working_directory(), Some(Path::new("/tmp/project")));
    }
}
