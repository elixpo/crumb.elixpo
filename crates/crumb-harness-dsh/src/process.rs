//! Lazy, cancellable ownership of one Harness SDK subprocess.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crumb_agent::CancellationToken;
use serde::Deserialize;

use crate::protocol::{
    IncomingFrame, InitializeParams, Notification, Response, SessionPromptParams,
    encode_initialize, encode_session_prompt, encode_shutdown,
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const STDERR_TAIL_BYTES: usize = 32 * 1024;
const MAX_PROTOCOL_LINE_BYTES: usize = 1024 * 1024;
const SERVER_NAME: &str = "deepseek-harness-sdk-runtime";

/// Explicit child environment. It intentionally has no `Debug` implementation
/// because values may contain credentials supplied by the runtime boundary.
#[derive(Default)]
pub struct HarnessEnvironment {
    values: BTreeMap<OsString, OsString>,
}

impl HarnessEnvironment {
    #[must_use]
    pub fn runtime_basics() -> Self {
        let mut environment = Self::default();
        for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR"] {
            if let Some(value) = std::env::var_os(name) {
                environment.insert(name, value);
            }
        }
        environment
    }

    pub fn insert(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) {
        self.values.insert(name.into(), value.into());
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
    server_info: ServerInfo,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptReceipt {
    pub message_id: String,
}

enum ReaderEvent {
    Frame(IncomingFrame),
    Failed(String),
    Closed,
}

/// One lazily launched, exclusively owned Harness SDK process.
pub struct ProcessHarness {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<ReaderEvent>,
    stderr_tail: Arc<Mutex<BoundedTail>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    next_id: u64,
}

impl ProcessHarness {
    /// Launches the configured process with piped protocol streams and an
    /// explicit credential-bearing environment.
    ///
    /// # Errors
    ///
    /// Returns an error if the process or its reader threads cannot start.
    pub fn spawn(
        program: &Path,
        arguments: &[String],
        cwd: &Path,
        environment: &HarnessEnvironment,
    ) -> Result<Self> {
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(cwd)
            .env_clear()
            .envs(&environment.values)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch Harness process `{}`", program.display()))?;
        let result = Self::attach(&mut child);
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result.map(|parts| Self {
            child,
            stdin: Some(parts.stdin),
            incoming: parts.incoming,
            stderr_tail: parts.stderr_tail,
            stdout_thread: Some(parts.stdout_thread),
            stderr_thread: Some(parts.stderr_thread),
            next_id: 1,
        })
    }

    fn attach(child: &mut Child) -> Result<ProcessParts> {
        let stdin = child.stdin.take().context("Harness stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("Harness stdout was not piped")?;
        let stderr = child
            .stderr
            .take()
            .context("Harness stderr was not piped")?;
        let (sender, incoming) = mpsc::channel();
        let stdout_thread = thread::Builder::new()
            .name("crumb-dsh-stdout".to_owned())
            .spawn(move || read_stdout(stdout, &sender))
            .context("failed to start Harness stdout reader")?;
        let stderr_tail = Arc::new(Mutex::new(BoundedTail::new(STDERR_TAIL_BYTES)));
        let stderr_writer = Arc::clone(&stderr_tail);
        let stderr_thread = thread::Builder::new()
            .name("crumb-dsh-stderr".to_owned())
            .spawn(move || read_stderr(stderr, &stderr_writer))
            .context("failed to start Harness stderr reader")?;
        Ok(ProcessParts {
            stdin,
            incoming,
            stderr_tail,
            stdout_thread,
            stderr_thread,
        })
    }

    /// Initializes the exact provider/model/effort tuple and validates the
    /// runtime identity.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, timeout, transport failure, an SDK
    /// error response, or an incompatible server identity.
    pub fn initialize(
        &mut self,
        params: InitializeParams<'_>,
        cancellation: &CancellationToken,
        timeout: Duration,
    ) -> Result<ServerInfo> {
        let id = self.take_id()?;
        let response = self.exchange(
            id,
            &encode_initialize(id, params)?,
            cancellation,
            timeout,
            "initialize",
        )?;
        let result = successful_result(response)?;
        let initialized: InitializeResult =
            serde_json::from_value(result).context("invalid Harness initialize response")?;
        if initialized.server_info.name != SERVER_NAME {
            bail!(
                "incompatible Harness server `{}`",
                initialized.server_info.name
            );
        }
        Ok(initialized.server_info)
    }

    /// Queues one text prompt and returns its durable admission receipt.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, timeout, transport failure, or an
    /// invalid SDK response.
    pub fn prompt(
        &mut self,
        params: SessionPromptParams<'_>,
        cancellation: &CancellationToken,
        timeout: Duration,
    ) -> Result<(PromptReceipt, Vec<Notification>)> {
        let id = self.take_id()?;
        let (response, notifications) = self.exchange_with_notifications(
            id,
            &encode_session_prompt(id, params)?,
            cancellation,
            timeout,
            "session/prompt",
        )?;
        let receipt = serde_json::from_value(successful_result(response)?)
            .context("invalid Harness prompt receipt")?;
        Ok((receipt, notifications))
    }

    /// Requests graceful shutdown, then forcefully reaps a process that does
    /// not exit within the supplied bound.
    ///
    /// # Errors
    ///
    /// Returns the shutdown protocol or process-wait error after ensuring the
    /// child is no longer running.
    pub fn shutdown(mut self, timeout: Duration) -> Result<()> {
        let cancellation = CancellationToken::default();
        let id = self.take_id()?;
        let exchange = self
            .exchange(
                id,
                &encode_shutdown(id)?,
                &cancellation,
                timeout,
                "shutdown",
            )
            .and_then(successful_result);
        self.stdin.take();
        let wait = self.wait_or_kill(timeout);
        self.join_readers();
        exchange.map(|_| ()).and(wait)
    }

    #[must_use]
    pub fn diagnostics(&self) -> String {
        self.stderr_tail
            .lock()
            .map_or_else(|_| String::new(), |tail| tail.render())
    }

    fn take_id(&mut self) -> Result<u64> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("Harness request id exhausted")?;
        Ok(id)
    }

    fn exchange(
        &mut self,
        id: u64,
        encoded: &[u8],
        cancellation: &CancellationToken,
        timeout: Duration,
        operation: &str,
    ) -> Result<Response> {
        self.exchange_with_notifications(id, encoded, cancellation, timeout, operation)
            .map(|(response, _)| response)
    }

    fn exchange_with_notifications(
        &mut self,
        id: u64,
        encoded: &[u8],
        cancellation: &CancellationToken,
        timeout: Duration,
        operation: &str,
    ) -> Result<(Response, Vec<Notification>)> {
        let stdin = self.stdin.as_mut().context("Harness stdin is closed")?;
        stdin
            .write_all(encoded)
            .context("failed to write Harness request")?;
        stdin.flush().context("failed to flush Harness request")?;
        let deadline = Instant::now() + timeout;
        let mut notifications = Vec::new();
        loop {
            if cancellation.is_cancelled() {
                self.hard_stop();
                bail!("Harness {operation} cancelled");
            }
            let now = Instant::now();
            if now >= deadline {
                self.hard_stop();
                bail!("Harness {operation} timed out");
            }
            let wait = POLL_INTERVAL.min(deadline.saturating_duration_since(now));
            match self.incoming.recv_timeout(wait) {
                Ok(ReaderEvent::Frame(IncomingFrame::Notification(notification))) => {
                    notifications.push(notification);
                }
                Ok(ReaderEvent::Frame(IncomingFrame::Response(response))) => {
                    if response.id.as_u64() != Some(id) {
                        self.hard_stop();
                        bail!("Harness returned an unexpected response id");
                    }
                    return Ok((response, notifications));
                }
                Ok(ReaderEvent::Failed(error)) => {
                    self.hard_stop();
                    return Err(anyhow!(error).context("invalid Harness protocol output"));
                }
                Ok(ReaderEvent::Closed) | Err(RecvTimeoutError::Disconnected) => {
                    let diagnostics = self.diagnostics();
                    self.hard_stop();
                    bail!("Harness process closed its protocol stream{diagnostics}");
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn wait_or_kill(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .child
                .try_wait()
                .context("failed to inspect Harness process")?
                .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.hard_stop();
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn hard_stop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }

    fn join_readers(&mut self) {
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ProcessHarness {
    fn drop(&mut self) {
        self.hard_stop();
        self.join_readers();
    }
}

struct ProcessParts {
    stdin: ChildStdin,
    incoming: Receiver<ReaderEvent>,
    stderr_tail: Arc<Mutex<BoundedTail>>,
    stdout_thread: JoinHandle<()>,
    stderr_thread: JoinHandle<()>,
}

fn successful_result(response: Response) -> Result<serde_json::Value> {
    if let Some(error) = response.error {
        bail!("Harness SDK error {}: {}", error.code, error.message);
    }
    response
        .result
        .context("Harness response omitted its result")
}

fn read_stdout(stdout: impl Read, sender: &mpsc::Sender<ReaderEvent>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_protocol_line(&mut reader) {
            Ok(Some(line)) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let parsed = std::str::from_utf8(&line)
                    .context("Harness protocol line is not UTF-8")
                    .and_then(IncomingFrame::parse);
                let event = parsed.map_or_else(
                    |error| ReaderEvent::Failed(error.to_string()),
                    ReaderEvent::Frame,
                );
                if sender.send(event).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(ReaderEvent::Closed);
                return;
            }
            Err(error) => {
                let _ = sender.send(ReaderEvent::Failed(error.to_string()));
                return;
            }
        }
    }
}

fn read_protocol_line(reader: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(length) > MAX_PROTOCOL_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Harness protocol line exceeded the byte limit",
            ));
        }
        line.extend_from_slice(&available[..length]);
        let complete = available[length - 1] == b'\n';
        reader.consume(length);
        if complete {
            return Ok(Some(line));
        }
    }
}

fn read_stderr(mut stderr: impl Read, destination: &Mutex<BoundedTail>) {
    let mut buffer = [0_u8; 4096];
    while let Ok(count) = stderr.read(&mut buffer) {
        if count == 0 {
            return;
        }
        if let Ok(mut tail) = destination.lock() {
            tail.push(&buffer[..count]);
        }
    }
}

struct BoundedTail {
    bytes: VecDeque<u8>,
    capacity: usize,
}

impl BoundedTail {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.capacity {
            self.bytes.clear();
            self.bytes
                .extend(bytes[bytes.len() - self.capacity..].iter().copied());
            return;
        }
        while self.bytes.len() + bytes.len() > self.capacity {
            self.bytes.pop_front();
        }
        self.bytes.extend(bytes.iter().copied());
    }

    fn render(&self) -> String {
        if self.bytes.is_empty() {
            String::new()
        } else {
            let bytes = self.bytes.iter().copied().collect::<Vec<_>>();
            format!(
                "\nHarness stderr tail:\n{}",
                String::from_utf8_lossy(&bytes)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{BoundedTail, MAX_PROTOCOL_LINE_BYTES, read_protocol_line};

    #[test]
    fn stderr_tail_keeps_only_the_latest_bytes() {
        let mut tail = BoundedTail::new(5);
        tail.push(b"abc");
        tail.push(b"defg");
        assert!(tail.render().ends_with("cdefg"));
    }

    #[test]
    fn protocol_lines_are_bounded() {
        let input = vec![b'x'; MAX_PROTOCOL_LINE_BYTES + 1];
        assert!(read_protocol_line(&mut Cursor::new(input)).is_err());
    }
}
