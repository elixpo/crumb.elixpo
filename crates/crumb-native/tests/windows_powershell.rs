#![cfg(target_os = "windows")]

use std::io::Read;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crumb_native::{NativeShell, ShellKind};
use crumb_pty::{CommandSpec, SystemPty, TerminalSize};

struct DeterministicPowerShell;

impl NativeShell for DeterministicPowerShell {
    fn kind(&self) -> ShellKind {
        ShellKind::PowerShell
    }

    fn command_spec(&self) -> CommandSpec {
        CommandSpec::new("pwsh")
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NoExit")
    }
}

#[test]
fn powershell_state_persists_in_one_resizable_conpty() {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = exercise_persistent_powershell();
        sender
            .send(result)
            .expect("test receiver should remain open");
    });

    let output = receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("interactive PowerShell should exit within fifteen seconds")
        .expect("ConPTY session should succeed");
    worker.join().expect("ConPTY worker should not panic");

    assert!(
        output.contains("env=ready"),
        "unexpected ConPTY output: {output:?}"
    );
    assert!(
        output.to_ascii_lowercase().contains("cwd=c:\\windows"),
        "unexpected ConPTY output: {output:?}"
    );
}

fn exercise_persistent_powershell() -> Result<String> {
    let mut process = DeterministicPowerShell.spawn(&SystemPty, TerminalSize::new(24, 80))?;
    let mut reader = process.try_clone_reader()?;

    process.resize(TerminalSize::new(40, 120))?;
    process.write_input(
        b"Set-Location C:\\Windows\r\n$env:CRUMB_WP004_STATE = 'ready'\r\nWrite-Output \"__CRUMB_STATE__ cwd=$((Get-Location).Path) env=$env:CRUMB_WP004_STATE\"\r\nexit\r\n",
    )?;

    let mut output = String::new();
    reader.read_to_string(&mut output)?;
    process.wait()?;
    Ok(output)
}
