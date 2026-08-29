use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crumb_agent::{
    CancellationToken, RiskClass, ToolDescriptor, ToolHandler, ToolHost, ToolOutput, ToolTransport,
};
use serde_json::{Value, json};

use crate::bounded_text;

const RUN_SHELL: &str = "run_shell";
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Launch and runtime limits for the isolated agent shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentShellConfig {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub path: Option<OsString>,
    pub max_output_bytes: usize,
    pub timeout: Duration,
}

/// Registers an approval-gated shell tool rooted at one canonical workspace.
///
/// The program receives the model-proposed command as its final argument. Its
/// inherited environment is cleared; only the configured `PATH` is restored.
///
/// # Errors
///
/// Returns an error when the workspace, executable, or runtime limits are
/// invalid, or when the tool name is already registered.
pub fn register_shell_tool(
    host: &mut ToolHost,
    workspace: &Path,
    config: AgentShellConfig,
) -> Result<()> {
    if !config.program.is_absolute() {
        bail!("agent shell program must be an absolute path");
    }
    if config.max_output_bytes == 0 {
        bail!("agent shell output limit must be positive");
    }
    if config.timeout.is_zero() {
        bail!("agent shell timeout must be positive");
    }
    let workspace = std::fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
    if !workspace.is_dir() {
        bail!("agent shell workspace must be a directory");
    }
    host.register(descriptor(), Arc::new(ShellTool { workspace, config }))?;
    Ok(())
}

struct ShellTool {
    workspace: PathBuf,
    config: AgentShellConfig,
}

impl ToolHandler for ShellTool {
    fn call(&self, arguments: &Value, cancellation: &CancellationToken) -> Result<ToolOutput> {
        match run_shell(&self.workspace, &self.config, arguments, cancellation) {
            Ok(output) => Ok(output),
            Err(error) if cancellation.is_cancelled() => Err(error),
            Err(error) => Ok(ToolOutput::error(error.to_string())),
        }
    }
}

fn run_shell(
    workspace: &Path,
    config: &AgentShellConfig,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<ToolOutput> {
    ensure_active(cancellation)?;
    let command = arguments
        .get("command")
        .and_then(Value::as_str)
        .context("command must be a string")?;
    if command.trim().is_empty() {
        bail!("command cannot be empty");
    }
    let timeout = requested_timeout(arguments, config.timeout)?;
    let mut process = Command::new(&config.program);
    process
        .args(&config.arguments)
        .arg(command)
        .current_dir(workspace)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = &config.path {
        process.env("PATH", path);
    }
    configure_process_group(&mut process);
    let mut child = process
        .spawn()
        .context("failed to start isolated agent shell")?;
    let stdout = child
        .stdout
        .take()
        .context("agent shell stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("agent shell stderr unavailable")?;
    let stdout_reader = capture(stdout, config.max_output_bytes, CapturePosition::Head);
    let stderr_reader = capture(stderr, config.max_output_bytes, CapturePosition::Tail);
    let started = Instant::now();

    let outcome = loop {
        if cancellation.is_cancelled() {
            terminate(&mut child);
            child
                .wait()
                .context("failed to reap cancelled agent shell")?;
            break ProcessOutcome::Cancelled;
        }
        if started.elapsed() >= timeout {
            terminate(&mut child);
            child
                .wait()
                .context("failed to reap timed-out agent shell")?;
            break ProcessOutcome::TimedOut;
        }
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect agent shell status")?
        {
            break ProcessOutcome::Exited(status);
        }
        thread::sleep(POLL_INTERVAL);
    };

    let stdout = join_capture(stdout_reader)?;
    let stderr = join_capture(stderr_reader)?;
    match outcome {
        ProcessOutcome::Cancelled => bail!("tool call cancelled"),
        ProcessOutcome::TimedOut => Ok(ToolOutput::error(render_output(
            "timed_out",
            &stdout,
            &stderr,
            true,
            config.max_output_bytes,
        ))),
        ProcessOutcome::Exited(status) => {
            let failed = !status.success();
            let status = status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string());
            let text = render_output(&status, &stdout, &stderr, failed, config.max_output_bytes);
            Ok(if failed {
                ToolOutput::error(text)
            } else {
                ToolOutput::text(text)
            })
        }
    }
}

fn requested_timeout(arguments: &Value, ceiling: Duration) -> Result<Duration> {
    let Some(seconds) = arguments.get("timeout_seconds") else {
        return Ok(ceiling);
    };
    let seconds = seconds
        .as_u64()
        .context("timeout_seconds must be a positive integer")?;
    if seconds == 0 {
        bail!("timeout_seconds must be positive");
    }
    Ok(Duration::from_secs(seconds).min(ceiling))
}

enum ProcessOutcome {
    Cancelled,
    TimedOut,
    Exited(ExitStatus),
}

#[derive(Clone, Copy)]
enum CapturePosition {
    Head,
    Tail,
}

struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

fn capture(
    mut reader: impl Read + Send + 'static,
    limit: usize,
    position: CapturePosition,
) -> JoinHandle<io::Result<CapturedStream>> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut truncated = false;
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Ok(CapturedStream {
                    bytes: captured,
                    truncated,
                });
            }
            match position {
                CapturePosition::Head => {
                    let remaining = limit.saturating_sub(captured.len());
                    captured.extend_from_slice(&buffer[..read.min(remaining)]);
                    truncated |= read > remaining;
                }
                CapturePosition::Tail => {
                    captured.extend_from_slice(&buffer[..read]);
                    if captured.len() > limit {
                        truncated = true;
                        captured.drain(..captured.len() - limit);
                    }
                }
            }
        }
    })
}

fn join_capture(reader: JoinHandle<io::Result<CapturedStream>>) -> Result<CapturedStream> {
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("agent shell output reader panicked"))?
        .context("failed to read agent shell output")
}

fn render_output(
    status: &str,
    stdout: &CapturedStream,
    stderr: &CapturedStream,
    diagnostics_first: bool,
    limit: usize,
) -> String {
    let stdout_text = String::from_utf8_lossy(&stdout.bytes);
    let stderr_text = String::from_utf8_lossy(&stderr.bytes);
    let stdout_label = if stdout.truncated {
        "stdout (head, truncated)"
    } else {
        "stdout"
    };
    let stderr_label = if stderr.truncated {
        "stderr (tail, truncated)"
    } else {
        "stderr"
    };
    let streams = if diagnostics_first {
        format!("{stderr_label}:\n{stderr_text}\n{stdout_label}:\n{stdout_text}")
    } else {
        format!("{stdout_label}:\n{stdout_text}\n{stderr_label}:\n{stderr_text}")
    };
    bounded_text(format!("exit: {status}\n{streams}"), limit)
}

fn ensure_active(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        bail!("tool call cancelled");
    }
    Ok(())
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate(child: &mut Child) {
    use rustix::process::{Pid, Signal, kill_process_group};

    let pid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    if let Some(pid) = pid {
        if kill_process_group(pid, Signal::Kill).is_err() {
            let _ = child.kill();
        }
    } else {
        let _ = child.kill();
    }
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) {
    let _ = child.kill();
}

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: RUN_SHELL.to_owned(),
        description: "Run a bounded command in Crumb's isolated agent shell.".to_owned(),
        input_schema: json!({
            "type":"object",
            "properties":{
                "command":{"type":"string","minLength":1},
                "timeout_seconds":{"type":"integer","minimum":1}
            },
            "required":["command"],
            "additionalProperties":false
        }),
        risk: RiskClass::ProcessExecution,
        transport: ToolTransport::Native,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use crumb_agent::{
        AgentMode, ApprovalBroker, ApprovalDecision, ApprovalRequest, CancellationToken,
        DenyAllApprovals, RiskClass, ToolCallErrorKind, ToolHost,
    };
    use serde_json::json;

    use super::{AgentShellConfig, register_shell_tool};

    struct AllowOnce;

    impl ApprovalBroker for AllowOnce {
        fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
            ApprovalDecision::AllowOnce
        }
    }

    fn config(timeout: Duration) -> AgentShellConfig {
        AgentShellConfig {
            program: PathBuf::from("/bin/sh"),
            arguments: vec![OsString::from("-c")],
            path: Some(OsString::from("/usr/bin:/bin")),
            max_output_bytes: 256,
            timeout,
        }
    }

    fn host(timeout: Duration) -> ToolHost {
        let mut host = ToolHost::default();
        register_shell_tool(
            &mut host,
            &std::env::current_dir().expect("current directory is available"),
            config(timeout),
        )
        .expect("shell tool is registered");
        host
    }

    #[test]
    fn shell_execution_requires_approval() {
        let host = host(Duration::from_secs(1));
        let descriptor = host.tools().next().expect("shell descriptor exists");
        assert_eq!(descriptor.risk, RiskClass::ProcessExecution);
        let error = host
            .call(
                "run_shell",
                &json!({"command":"printf should-not-run"}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &CancellationToken::default(),
            )
            .expect_err("process execution is approval gated");
        assert_eq!(error.kind, ToolCallErrorKind::Denied);
    }

    #[cfg(unix)]
    #[test]
    fn approved_shell_is_isolated_and_returns_bounded_output() {
        let output = host(Duration::from_secs(1))
            .call(
                "run_shell",
                &json!({"command":"printf hello; /usr/bin/env"}),
                AgentMode::Auto,
                &AllowOnce,
                &CancellationToken::default(),
            )
            .expect("approved shell call succeeds");
        assert!(!output.is_error);
        assert!(output.text.contains("hello"));
        assert!(output.text.contains("PATH=/usr/bin:/bin"));
        assert!(!output.text.contains("HOME="));
        assert!(output.text.len() <= 256);
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_prioritizes_stderr() {
        let output = host(Duration::from_secs(1))
            .call(
                "run_shell",
                &json!({"command":"printf diagnostic >&2; exit 7"}),
                AgentMode::Auto,
                &AllowOnce,
                &CancellationToken::default(),
            )
            .expect("command failure is returned as tool output");
        assert!(output.is_error);
        assert!(output.text.starts_with("exit: 7\nstderr:\ndiagnostic"));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_the_command() {
        let output = host(Duration::from_millis(30))
            .call(
                "run_shell",
                &json!({"command":"sleep 2"}),
                AgentMode::Auto,
                &AllowOnce,
                &CancellationToken::default(),
            )
            .expect("timeout is an expected tool result");
        assert!(output.is_error);
        assert!(output.text.starts_with("exit: timed_out"));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_the_command() {
        let host = Arc::new(host(Duration::from_secs(2)));
        let cancellation = CancellationToken::default();
        let call_token = cancellation.clone();
        let call_host = Arc::clone(&host);
        let call = thread::spawn(move || {
            call_host.call(
                "run_shell",
                &json!({"command":"sleep 2"}),
                AgentMode::Auto,
                &AllowOnce,
                &call_token,
            )
        });
        thread::sleep(Duration::from_millis(30));
        cancellation.cancel();
        let error = call
            .join()
            .expect("tool thread does not panic")
            .expect_err("cancelled command returns a typed error");
        assert_eq!(error.kind, ToolCallErrorKind::Cancelled);
    }
}
