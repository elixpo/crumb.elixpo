#![cfg(target_os = "linux")]

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crumb_native::session::ShellSession;
use crumb_native::{NativeShell, ShellKind};
use crumb_pty::{CommandSpec, SystemPty, TerminalSize};

struct DeterministicBash;

impl NativeShell for DeterministicBash {
    fn kind(&self) -> ShellKind {
        ShellKind::Bash
    }

    fn command_spec(&self) -> CommandSpec {
        CommandSpec::new("bash")
            .arg("--noprofile")
            .arg("--norc")
            .arg("-i")
    }
}

#[test]
fn lifecycle_frames_preserve_state_and_report_metadata() {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = exercise_lifecycle();
        sender
            .send(result)
            .expect("test receiver should remain open");
    });

    let output = receiver
        .recv_timeout(Duration::from_secs(8))
        .expect("managed Bash should finish within eight seconds")
        .expect("managed Bash session should succeed");
    worker.join().expect("managed Bash worker should not panic");

    assert!(output.contains("state=ready"));
    assert!(!output.contains("\u{1e}crumb:"));
}

fn exercise_lifecycle() -> Result<String> {
    let mut session =
        ShellSession::start(&DeterministicBash, &SystemPty, TerminalSize::new(24, 80))?;
    let mut output = Vec::new();

    let cd = session.execute("cd /", &mut output)?;
    assert_eq!(cd.sequence, 1);
    assert_eq!(cd.exit_code, 0);
    assert_eq!(cd.cwd.to_string_lossy(), "/");

    session.execute("export CRUMB_LIFECYCLE_STATE=ready", &mut output)?;
    session.execute(
        "printf 'state=%s\\n' \"$CRUMB_LIFECYCLE_STATE\"",
        &mut output,
    )?;
    let failed = session.execute("false", &mut output)?;
    assert_eq!(failed.exit_code, 1);
    session.shutdown()?;

    Ok(String::from_utf8(output)?)
}
