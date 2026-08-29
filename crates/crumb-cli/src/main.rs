use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use crumb_native::shell_for;
use crumb_platform::Platform;
use crumb_pty::{SystemPty, TerminalSize};
use crumb_repl::ReplOutcome;

fn main() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();

    if crumb_cli::run(stdin.lock(), stdout.lock())? == ReplOutcome::LaunchNativeShell {
        run_native_shell()?;
    }

    Ok(())
}

fn run_native_shell() -> Result<()> {
    let (cols, rows) = size()?;
    let shell = shell_for(Platform::current())?;
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
    io::copy(&mut reader, &mut stdout)?;
    stdout.flush()?;

    running.store(false, Ordering::Relaxed);
    let _ = resize_thread.join();
    process.wait()?;
    drop(input_thread);
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
