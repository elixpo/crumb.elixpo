//! Minimal interactive loop and input classification for crumb.

use std::env;
use std::io::{self, BufRead, Write};
use std::path::Path;

use crumb_core::{AuthAction, BuiltInCommand, HistoryAction, InputEvent};
use crumb_platform::Platform;

/// Reason the REPL returned control to its caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplOutcome {
    Exit,
    LaunchNativeShell,
}

/// One command exposed through Crumb's `/` namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlashCommand {
    pub usage: &'static str,
    pub description: &'static str,
}

/// Commands shown by `/help` and interactive completion.
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        usage: "/help",
        description: "show Crumb commands",
    },
    SlashCommand {
        usage: "/auth login",
        description: "connect Pollinations",
    },
    SlashCommand {
        usage: "/auth status",
        description: "show connector authentication",
    },
    SlashCommand {
        usage: "/auth logout",
        description: "remove the stored connector",
    },
    SlashCommand {
        usage: "/connectors",
        description: "show connected services",
    },
    SlashCommand {
        usage: "/skills",
        description: "show configured skills",
    },
    SlashCommand {
        usage: "/context",
        description: "show inline reference syntax",
    },
    SlashCommand {
        usage: "/history",
        description: "show recent native history",
    },
    SlashCommand {
        usage: "/history search ",
        description: "search native history",
    },
    SlashCommand {
        usage: "/mode",
        description: "select auto, negotiate, or plan mode",
    },
    SlashCommand {
        usage: "/mode use ",
        description: "set auto, negotiate, or plan mode",
    },
    SlashCommand {
        usage: "/model",
        description: "inspect or select a model",
    },
    SlashCommand {
        usage: "/model use ",
        description: "select an exact provider/model",
    },
    SlashCommand {
        usage: "/effort",
        description: "inspect or set reasoning effort",
    },
    SlashCommand {
        usage: "/effort use ",
        description: "set an exact effort or provider default",
    },
    SlashCommand {
        usage: "/session",
        description: "manage agent sessions",
    },
    SlashCommand {
        usage: "/session list",
        description: "list agent sessions",
    },
    SlashCommand {
        usage: "/session search ",
        description: "search session metadata",
    },
    SlashCommand {
        usage: "/session resume ",
        description: "resume an agent session",
    },
    SlashCommand {
        usage: "/session inspect ",
        description: "inspect redacted session metadata",
    },
    SlashCommand {
        usage: "/session rename ",
        description: "label an agent session",
    },
    SlashCommand {
        usage: "/session archive ",
        description: "archive an agent session",
    },
    SlashCommand {
        usage: "/session restore ",
        description: "restore an archived session",
    },
    SlashCommand {
        usage: "/session export ",
        description: "print a redacted session export",
    },
    SlashCommand {
        usage: "/session delete ",
        description: "move a session to recoverable trash",
    },
    SlashCommand {
        usage: "/review",
        description: "review Crumb-owned edits",
    },
    SlashCommand {
        usage: "/review list",
        description: "list edit checkpoints",
    },
    SlashCommand {
        usage: "/review diff ",
        description: "show a bounded checkpoint diff",
    },
    SlashCommand {
        usage: "/review approve ",
        description: "approve a checkpoint",
    },
    SlashCommand {
        usage: "/review reject ",
        description: "safely rewind a checkpoint",
    },
    SlashCommand {
        usage: "/review comment ",
        description: "attach feedback to the next agent turn",
    },
    SlashCommand {
        usage: "/review export ",
        description: "print checkpoint metadata as JSON",
    },
    SlashCommand {
        usage: "/jobs",
        description: "list local agent jobs",
    },
    SlashCommand {
        usage: "/jobs inspect ",
        description: "inspect redacted job metadata",
    },
    SlashCommand {
        usage: "/jobs create ",
        description: "start an approved background request",
    },
    SlashCommand {
        usage: "/jobs schedule once ",
        description: "opt in to one scheduled run",
    },
    SlashCommand {
        usage: "/jobs schedule recurring ",
        description: "opt in to a recurring local run",
    },
    SlashCommand {
        usage: "/jobs cancel ",
        description: "cancel a queued or running job",
    },
    SlashCommand {
        usage: "/jobs reattach ",
        description: "resume a completed job session",
    },
    SlashCommand {
        usage: "/jobs tick",
        description: "launch due opted-in schedules",
    },
    SlashCommand {
        usage: "/background",
        description: "continue the active agent turn as a local job",
    },
    SlashCommand {
        usage: "/attach ",
        description: "attach a typed @ reference",
    },
    SlashCommand {
        usage: "/detach ",
        description: "remove attached context",
    },
    SlashCommand {
        usage: "/plugins",
        description: "inspect installed plugins",
    },
    SlashCommand {
        usage: "/tools",
        description: "inspect available tools",
    },
    SlashCommand {
        usage: "/permissions",
        description: "inspect agent permissions",
    },
    SlashCommand {
        usage: "/memory",
        description: "manage approved memory",
    },
    SlashCommand {
        usage: "/config",
        description: "inspect live configuration",
    },
    SlashCommand {
        usage: "/config provider ",
        description: "configure Harness providers and models",
    },
    SlashCommand {
        usage: "/cancel",
        description: "cancel the active agent turn",
    },
    SlashCommand {
        usage: "/doctor",
        description: "diagnose optional AI services",
    },
    SlashCommand {
        usage: "/cost",
        description: "show session usage",
    },
    SlashCommand {
        usage: "/platform",
        description: "show the native platform",
    },
    SlashCommand {
        usage: "/version",
        description: "show the Crumb version",
    },
    SlashCommand {
        usage: "/shell",
        description: "enter the raw native shell",
    },
    SlashCommand {
        usage: "/exit",
        description: "exit Crumb",
    },
];

/// Classifies one line without executing native shell input.
#[must_use]
pub fn classify_input(input: &str) -> InputEvent {
    let command = input.trim_end_matches(['\r', '\n']);
    match command {
        "/auth login" => InputEvent::BuiltIn(BuiltInCommand::Auth(AuthAction::Login)),
        "/auth status" => InputEvent::BuiltIn(BuiltInCommand::Auth(AuthAction::Status)),
        "/auth logout" => InputEvent::BuiltIn(BuiltInCommand::Auth(AuthAction::Logout)),
        "/connectors" => InputEvent::BuiltIn(BuiltInCommand::Connectors),
        "/context" => InputEvent::BuiltIn(BuiltInCommand::Context),
        "/exit" => InputEvent::BuiltIn(BuiltInCommand::Exit),
        "/help" | "?" => InputEvent::BuiltIn(BuiltInCommand::Help),
        "/history" => InputEvent::BuiltIn(BuiltInCommand::History(HistoryAction::Recent)),
        "/history search" => InputEvent::BuiltIn(BuiltInCommand::History(HistoryAction::Search(
            String::new(),
        ))),
        "/platform" => InputEvent::BuiltIn(BuiltInCommand::Platform),
        "/shell" => InputEvent::BuiltIn(BuiltInCommand::Shell),
        "/skills" => InputEvent::BuiltIn(BuiltInCommand::Skills),
        "/version" => InputEvent::BuiltIn(BuiltInCommand::Version),
        _ if command.starts_with("/history search ") => {
            InputEvent::BuiltIn(BuiltInCommand::History(HistoryAction::Search(
                command["/history search ".len()..].to_owned(),
            )))
        }
        _ if reserved_slash_command(command) => {
            InputEvent::BuiltIn(BuiltInCommand::Reserved(command.to_owned()))
        }
        _ => InputEvent::NativeInput(command.to_owned()),
    }
}

fn reserved_slash_command(command: &str) -> bool {
    let Some(name) = command.split_whitespace().next() else {
        return false;
    };
    SLASH_COMMANDS.iter().any(|candidate| {
        candidate
            .usage
            .split_whitespace()
            .next()
            .is_some_and(|root| root == name)
    })
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

/// Reads and classifies one line without rendering a prompt.
///
/// # Errors
///
/// Returns an error when terminal input fails.
pub fn read_classified_line<R: BufRead>(reader: &mut R) -> io::Result<Option<InputEvent>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(classify_input(&line)))
}

/// Runs the WP-001 REPL until `/exit` or end-of-input.
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
            InputEvent::BuiltIn(BuiltInCommand::Auth(_)) => {
                writeln!(
                    writer,
                    "authentication is available in the crumb executable"
                )?;
            }
            InputEvent::BuiltIn(BuiltInCommand::Connectors) => {
                writeln!(writer, "connectors are available in the crumb executable")?;
            }
            InputEvent::BuiltIn(BuiltInCommand::Context) => {
                writeln!(
                    writer,
                    "context references are available in the crumb executable"
                )?;
            }
            InputEvent::BuiltIn(BuiltInCommand::Exit) => return Ok(ReplOutcome::Exit),
            InputEvent::BuiltIn(BuiltInCommand::Help) => {
                writeln!(writer, "help is available in the crumb executable")?;
            }
            InputEvent::BuiltIn(BuiltInCommand::History(_)) => {
                writeln!(writer, "history is available in the crumb executable")?;
            }
            InputEvent::BuiltIn(BuiltInCommand::Platform) => writeln!(writer, "{platform}")?,
            InputEvent::BuiltIn(BuiltInCommand::Reserved(command)) => {
                writeln!(writer, "reserved Crumb command: {command}")?;
            }
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
            InputEvent::BuiltIn(BuiltInCommand::Skills) => {
                writeln!(writer, "skills are available in the crumb executable")?;
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

    use crumb_core::{AuthAction, BuiltInCommand, HistoryAction, InputEvent};
    use crumb_platform::Platform;

    use super::{ReplOutcome, classify_input, read_input, render_prompt, run};

    #[test]
    fn classifies_supported_built_ins() {
        assert_eq!(
            classify_input("/exit\n"),
            InputEvent::BuiltIn(BuiltInCommand::Exit)
        );
        assert_eq!(
            classify_input("/version"),
            InputEvent::BuiltIn(BuiltInCommand::Version)
        );
        assert_eq!(
            classify_input("/platform"),
            InputEvent::BuiltIn(BuiltInCommand::Platform)
        );
        assert_eq!(
            classify_input("/shell"),
            InputEvent::BuiltIn(BuiltInCommand::Shell)
        );
        assert_eq!(
            classify_input("/auth login"),
            InputEvent::BuiltIn(BuiltInCommand::Auth(AuthAction::Login))
        );
        assert_eq!(
            classify_input("/history search cargo test"),
            InputEvent::BuiltIn(BuiltInCommand::History(HistoryAction::Search(
                "cargo test".to_owned()
            )))
        );
        assert_eq!(
            classify_input("/skills"),
            InputEvent::BuiltIn(BuiltInCommand::Skills)
        );
        assert_eq!(
            classify_input("?"),
            InputEvent::BuiltIn(BuiltInCommand::Help)
        );
        assert_eq!(
            classify_input("/mode auto"),
            InputEvent::BuiltIn(BuiltInCommand::Reserved("/mode auto".to_owned()))
        );
    }

    #[test]
    fn classifies_other_input_as_native() {
        assert_eq!(
            classify_input("  git status  "),
            InputEvent::NativeInput("  git status  ".to_owned())
        );
        assert_eq!(
            classify_input("/usr/bin/env"),
            InputEvent::NativeInput("/usr/bin/env".to_owned())
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
        let input = Cursor::new("/platform\n/version\n/exit\n");
        let mut output = Vec::new();

        let outcome = run(input, &mut output, Platform::Linux, "0.1.0").expect("REPL should run");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert_eq!(outcome, ReplOutcome::Exit);
        assert!(output.contains("linux"));
        assert!(output.contains("crumb 0.1.0"));
    }

    #[test]
    fn shell_command_returns_control_to_the_cli() {
        let input = Cursor::new("/shell\n");
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
