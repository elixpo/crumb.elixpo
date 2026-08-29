//! PTY primitives isolated from crumb's shell and UI layers.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
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

    fn to_command_builder(&self) -> Result<CommandBuilder> {
        let mut command = CommandBuilder::new(&self.program);
        for arg in &self.args {
            command.arg(arg);
        }
        let current_dir = match &self.current_dir {
            Some(path) => path.clone(),
            None => env::current_dir()?,
        };
        command.cwd(current_dir);
        Ok(command)
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
        let child = pair.slave.spawn_command(command.to_command_builder()?)?;
        drop(pair.slave);
        let writer = pair.master.take_writer()?;

        Ok(PtyProcess {
            master: Arc::new(Mutex::new(pair.master)),
            writer: Some(writer),
            child,
        })
    }
}

/// A live child process and its controlling pseudoterminal.
pub struct PtyProcess {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtyProcess {
    /// Clones a reader for the PTY output stream.
    ///
    /// # Errors
    ///
    /// Returns an error if the operating-system PTY reader cannot be cloned.
    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master
            .lock()
            .map_err(|_| anyhow!("PTY master lock is poisoned"))?
            .try_clone_reader()
    }

    /// Writes bytes to the PTY input stream and flushes them immediately.
    ///
    /// # Errors
    ///
    /// Returns an error if writing or flushing the PTY fails.
    pub fn write_input(&mut self, input: &[u8]) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow!("PTY input writer has already been taken"))?;
        writer.write_all(input)?;
        writer.flush()?;
        Ok(())
    }

    /// Transfers ownership of the PTY input stream to a relay thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the writer was already taken.
    pub fn take_writer(&mut self) -> Result<Box<dyn Write + Send>> {
        self.writer
            .take()
            .ok_or_else(|| anyhow!("PTY input writer has already been taken"))
    }

    /// Updates the terminal dimensions seen by the child process.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform cannot resize the PTY.
    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        self.resizer().resize(size)
    }

    #[must_use]
    pub fn resizer(&self) -> PtyResizer {
        PtyResizer {
            master: Arc::clone(&self.master),
        }
    }

    /// Requests termination of the child process.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform cannot signal the child.
    pub fn kill(&mut self) -> Result<()> {
        Ok(self.child.kill()?)
    }

    /// Waits for the PTY child to exit.
    ///
    /// # Errors
    ///
    /// Returns an error if the child status cannot be collected.
    pub fn wait(&mut self) -> Result<()> {
        self.child.wait()?;
        Ok(())
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
}

/// Cloneable handle used to resize a live PTY from a watcher thread.
#[derive(Clone)]
pub struct PtyResizer {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
}

impl PtyResizer {
    /// Updates the terminal dimensions seen by the child process.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY lock is poisoned or resizing fails.
    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        self.master
            .lock()
            .map_err(|_| anyhow!("PTY master lock is poisoned"))?
            .resize(size.into())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
