use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use crumb_core::{BuiltInCommand, HistoryAction, InputEvent};
use crumb_history::{HistoryEntry, HistoryMode, HistoryStore, RecordContext};
use crumb_native::session::{CommandOutcome, ShellSession};
use crumb_native::shell_for;
use crumb_platform::Platform;
use crumb_pty::{SystemPty, TerminalSize};
use crumb_repl::{ReplOutcome, read_classified_line};
use crumb_ui::{GitSegment, PromptContext, Renderer, UiSettings};

fn main() -> Result<()> {
    if run_managed_repl()? == ReplOutcome::LaunchNativeShell {
        run_native_shell()?;
    }

    Ok(())
}

fn run_managed_repl() -> Result<ReplOutcome> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let renderer = Renderer::new(UiSettings::from_environment(stdout.is_terminal()));
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let platform = Platform::current();
    let mut session: Option<ShellSession> = None;
    let mut last_exit_code = None;
    let history = match HistoryStore::open_default() {
        Ok(store) => Some(store),
        Err(error) => {
            writeln!(writer, "warning: command history is unavailable: {error}")?;
            None
        }
    };

    let branding = renderer.branding();
    if !branding.is_empty() {
        writeln!(writer, "{branding}")?;
    }

    loop {
        let cwd = session
            .as_ref()
            .map_or_else(current_process_dir, |shell| Ok(shell.cwd().to_path_buf()))?;
        let git = GitSegment::discover(&cwd);
        let prompt = renderer.prompt(&PromptContext {
            cwd: &cwd,
            platform,
            git: git.as_ref(),
            last_exit_code,
        });
        writer.write_all(prompt.as_bytes())?;
        writer.flush()?;

        let Some(event) = read_classified_line(&mut reader)? else {
            shutdown_session(session)?;
            return Ok(ReplOutcome::Exit);
        };

        match event {
            InputEvent::BuiltIn(BuiltInCommand::Exit) => {
                shutdown_session(session)?;
                return Ok(ReplOutcome::Exit);
            }
            InputEvent::BuiltIn(BuiltInCommand::History(action)) => {
                show_history(history.as_ref(), &action, &mut writer)?;
            }
            InputEvent::BuiltIn(BuiltInCommand::Platform) => {
                writeln!(writer, "{platform}")?;
                record_history(
                    history.as_ref(),
                    ":platform",
                    &cwd,
                    platform,
                    HistoryMode::BuiltIn,
                    Some(0),
                    &mut writer,
                )?;
            }
            InputEvent::BuiltIn(BuiltInCommand::Version) => {
                writeln!(writer, "crumb {}", env!("CARGO_PKG_VERSION"))?;
                record_history(
                    history.as_ref(),
                    ":version",
                    &cwd,
                    platform,
                    HistoryMode::BuiltIn,
                    Some(0),
                    &mut writer,
                )?;
            }
            InputEvent::BuiltIn(BuiltInCommand::Shell) if session.is_none() => {
                return Ok(ReplOutcome::LaunchNativeShell);
            }
            InputEvent::BuiltIn(BuiltInCommand::Shell) => {
                writeln!(
                    writer,
                    "`:shell` is available before the managed shell starts; restart crumb to enter raw mode"
                )?;
            }
            InputEvent::NativeInput(command) if command.trim().is_empty() => {}
            InputEvent::NativeInput(command) => {
                if session.is_none() {
                    let (cols, rows) = size()?;
                    let shell = shell_for(platform);
                    session = Some(ShellSession::start(
                        shell.as_ref(),
                        &SystemPty,
                        TerminalSize::new(rows, cols),
                    )?);
                }
                if let Some(shell) = session.as_mut() {
                    match shell.execute(&command, &mut writer)? {
                        CommandOutcome::Completed(completion) => {
                            last_exit_code = Some(completion.exit_code);
                            record_history(
                                history.as_ref(),
                                &command,
                                &cwd,
                                platform,
                                HistoryMode::Native,
                                Some(completion.exit_code),
                                &mut writer,
                            )?;
                        }
                        CommandOutcome::ShellExited => {
                            record_history(
                                history.as_ref(),
                                &command,
                                &cwd,
                                platform,
                                HistoryMode::Native,
                                None,
                                &mut writer,
                            )?;
                            return Ok(ReplOutcome::Exit);
                        }
                    }
                }
            }
        }
    }
}

fn show_history(
    history: Option<&HistoryStore>,
    action: &HistoryAction,
    writer: &mut dyn Write,
) -> Result<()> {
    let Some(history) = history else {
        writeln!(writer, "history is unavailable")?;
        return Ok(());
    };
    let result = match action {
        HistoryAction::Recent => history.recent(20),
        HistoryAction::Search(query) if query.trim().is_empty() => {
            writeln!(writer, "usage: :history search <text>")?;
            return Ok(());
        }
        HistoryAction::Search(query) => history.search(query, 20),
    };
    match result {
        Ok(entries) if entries.is_empty() => writeln!(writer, "no history entries")?,
        Ok(entries) => {
            for entry in entries {
                writeln!(writer, "{}", format_history_entry(&entry))?;
            }
        }
        Err(error) => writeln!(writer, "warning: history query failed: {error}")?,
    }
    Ok(())
}

fn format_history_entry(entry: &HistoryEntry) -> String {
    let exit = entry
        .exit_code
        .map_or_else(|| "-".to_owned(), |code| code.to_string());
    format!(
        "{}\t{}\t{}\t{}",
        entry.id,
        exit,
        entry.cwd.display(),
        entry.command
    )
}

#[allow(clippy::too_many_arguments)]
fn record_history(
    history: Option<&HistoryStore>,
    command: &str,
    cwd: &std::path::Path,
    platform: Platform,
    mode: HistoryMode,
    exit_code: Option<i32>,
    writer: &mut dyn Write,
) -> Result<()> {
    if let Some(history) = history
        && let Err(error) = history.record(
            command,
            RecordContext {
                cwd,
                platform,
                mode,
                exit_code,
            },
        )
    {
        writeln!(writer, "warning: failed to record history: {error}")?;
    }
    Ok(())
}

fn current_process_dir() -> Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

fn shutdown_session(session: Option<ShellSession>) -> Result<()> {
    if let Some(session) = session {
        session.shutdown()?;
    }
    Ok(())
}

fn run_native_shell() -> Result<()> {
    let (cols, rows) = size()?;
    let shell = shell_for(Platform::current());
    let mut process = shell.spawn(&SystemPty, TerminalSize::new(rows, cols))?;
    let mut reader = process.try_clone_reader()?;
    let mut writer = process.take_writer()?;
    let resizer = process.resizer();
    let running = Arc::new(AtomicBool::new(true));
    let _raw_mode = RawModeGuard::enable()?;

    let input_thread = thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        io::copy(&mut stdin, &mut writer)
    });

    let resize_running = Arc::clone(&running);
    let resize_thread = thread::spawn(move || {
        let mut previous = (cols, rows);
        while resize_running.load(Ordering::Relaxed) {
            if let Ok(current @ (new_cols, new_rows)) = size()
                && current != previous
            {
                let _ = resizer.resize(TerminalSize::new(new_rows, new_cols));
                previous = current;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });

    let mut stdout = io::stdout().lock();
    let output_result = relay_output(&mut reader, &mut stdout);

    running.store(false, Ordering::Relaxed);
    let _ = resize_thread.join();
    if output_result.is_err() {
        let _ = process.kill();
    }
    let wait_result = process.wait();
    drop(input_thread);

    output_result?;
    wait_result?;
    Ok(())
}

fn relay_output(reader: &mut dyn Read, writer: &mut dyn Write) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        writer.write_all(&buffer[..bytes_read])?;
        writer.flush()?;
    }
    Ok(())
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
