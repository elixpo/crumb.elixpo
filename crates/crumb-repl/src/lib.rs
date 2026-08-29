//! Minimal interactive loop and input classification for crumb.

use std::env;
use std::io::{self, BufRead, Write};
use std::path::Path;

use crumb_core::{BuiltInCommand, InputEvent};
use crumb_platform::Platform;

/// Reason the REPL returned control to its caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplOutcome {
    Exit,
    LaunchNativeShell,
}

/// Classifies one line without executing native shell input.
#[must_use]
pub fn classify_input(input: &str) -> InputEvent {
    match input.trim_end_matches(['\r', '\n']) {
        ":exit" => InputEvent::BuiltIn(BuiltInCommand::Exit),
        ":platform" => InputEvent::BuiltIn(BuiltInCommand::Platform),
        ":shell" => InputEvent::BuiltIn(BuiltInCommand::Shell),
        ":version" => InputEvent::BuiltIn(BuiltInCommand::Version),
        command => InputEvent::NativeInput(command.to_owned()),
    }
}

/// Renders the phase-one prompt for a working directory.
#[must_use]
pub fn render_prompt(cwd: &Path) -> String {
    format!("crumb:{}> ", cwd.display())
}

/// Reads and classifies one prompt line.
///
/// # Errors
///
/// Returns an error when terminal input/output fails.
pub fn read_input<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    cwd: &Path,
) -> io::Result<Option<InputEvent>> {
    write!(writer, "{}", render_prompt(cwd))?;
    writer.flush()?;

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(classify_input(&line)))
}

/// Runs the WP-001 REPL until `:exit` or end-of-input.
///
/// Native input is classified and reported, but deliberately not executed.
///
/// # Errors
///
/// Returns an error when the working directory cannot be read or terminal
/// input/output fails.
pub fn run<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    platform: Platform,
    version: &str,
) -> io::Result<ReplOutcome> {
    loop {
        let cwd = env::current_dir()?;
        let Some(event) = read_input(&mut reader, &mut writer, &cwd)? else {
            return Ok(ReplOutcome::Exit);
        };

        match event {
            InputEvent::BuiltIn(BuiltInCommand::Exit) => return Ok(ReplOutcome::Exit),
            InputEvent::BuiltIn(BuiltInCommand::Platform) => writeln!(writer, "{platform}")?,
            InputEvent::BuiltIn(BuiltInCommand::Shell) => {
                let shell_name = match platform {
                    Platform::Linux => "Bash",
                    Platform::MacOs => "Zsh",
                    Platform::Windows => "PowerShell",
                };
                writeln!(
                    writer,
                    "entering native {shell_name} through crumb (type `exit` to leave crumb)"
                )?;
                writer.flush()?;
                return Ok(ReplOutcome::LaunchNativeShell);
            }
            InputEvent::BuiltIn(BuiltInCommand::Version) => writeln!(writer, "crumb {version}")?,
            InputEvent::NativeInput(command) if command.trim().is_empty() => {}
            InputEvent::NativeInput(command) => {
                writeln!(
                    writer,
                    "native input (execution arrives in WP-002): {command}"
                )?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::Path;

    use crumb_core::{BuiltInCommand, InputEvent};
    use crumb_platform::Platform;

    use super::{ReplOutcome, classify_input, read_input, render_prompt, run};

    #[test]
    fn classifies_supported_built_ins() {
        assert_eq!(
            classify_input(":exit\n"),
            InputEvent::BuiltIn(BuiltInCommand::Exit)
        );
        assert_eq!(
            classify_input(":version"),
            InputEvent::BuiltIn(BuiltInCommand::Version)
        );
        assert_eq!(
            classify_input(":platform"),
            InputEvent::BuiltIn(BuiltInCommand::Platform)
        );
        assert_eq!(
            classify_input(":shell"),
            InputEvent::BuiltIn(BuiltInCommand::Shell)
        );
    }

    #[test]
    fn classifies_other_input_as_native() {
        assert_eq!(
            classify_input("  git status  "),
            InputEvent::NativeInput("  git status  ".to_owned())
        );
    }

    #[test]
    fn prompt_contains_the_working_directory() {
        assert_eq!(
            render_prompt(Path::new("/tmp/project")),
            "crumb:/tmp/project> "
        );
    }

    #[test]
    fn reads_one_classified_input_event() {
        let mut input = Cursor::new("git status\n");
        let mut output = Vec::new();

        let event = read_input(&mut input, &mut output, Path::new("/workspace"))
            .expect("input should be read");

        assert_eq!(
            event,
            Some(InputEvent::NativeInput("git status".to_owned()))
        );
        assert_eq!(
            String::from_utf8(output).expect("output should be UTF-8"),
            "crumb:/workspace> "
        );
    }

    #[test]
    fn repl_handles_built_ins_and_stops() {
        let input = Cursor::new(":platform\n:version\n:exit\n");
        let mut output = Vec::new();

        let outcome = run(input, &mut output, Platform::Linux, "0.1.0").expect("REPL should run");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert_eq!(outcome, ReplOutcome::Exit);
        assert!(output.contains("linux"));
        assert!(output.contains("crumb 0.1.0"));
    }

    #[test]
    fn shell_command_returns_control_to_the_cli() {
        let input = Cursor::new(":shell\n");
        let mut output = Vec::new();

        let outcome = run(input, &mut output, Platform::Linux, "0.1.0").expect("REPL should run");

        assert_eq!(outcome, ReplOutcome::LaunchNativeShell);
        assert!(
            String::from_utf8(output)
                .expect("output should be UTF-8")
                .contains("entering native Bash")
        );
    }
}
