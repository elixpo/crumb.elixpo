//! Cancellable, append-only agent session primitives.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::AgentMode;

/// Validated directory-safe session identifier.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Creates a validated session identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is empty, too long, or not directory-safe.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            bail!("session id must contain 1-64 ASCII letters, numbers, dashes, or underscores");
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Cloneable cancellation signal shared by Ctrl+C, the agent loop, and tools.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Persisted event vocabulary. Raw prompts, tool arguments, credentials, and
/// unfiltered command output are intentionally absent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionStarted {
        at_ms: u128,
        session_id: SessionId,
        workspace: PathBuf,
        mode: AgentMode,
    },
    TurnStarted {
        at_ms: u128,
        request_bytes: usize,
        request_digest: String,
    },
    ToolRequested {
        at_ms: u128,
        name: String,
        risk: crate::tools::RiskClass,
        arguments_digest: String,
    },
    ToolFinished {
        at_ms: u128,
        name: String,
        success: bool,
        output_bytes: usize,
    },
    TurnFinished {
        at_ms: u128,
        status: TurnStatus,
        steps: u32,
        tool_calls: u32,
    },
    ModeChanged {
        at_ms: u128,
        mode: AgentMode,
    },
    ModelSelected {
        at_ms: u128,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Complete,
    Cancelled,
    Failed,
    LimitReached,
}

/// Redacted metadata reconstructed from one append-only session journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace: PathBuf,
    pub mode: AgentMode,
    pub started_at_ms: u128,
    pub last_event_at_ms: u128,
    pub turns: u32,
    pub last_status: Option<TurnStatus>,
}

/// Lists valid session journals newest-first without loading prompt content.
///
/// # Errors
///
/// Returns an error when the session root cannot be read. Invalid individual
/// entries are skipped so one damaged journal does not hide healthy sessions.
pub fn list_sessions(root: &Path) -> Result<Vec<SessionSummary>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut summaries = fs::read_dir(root)
        .with_context(|| format!("failed to read session root {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| session_summary(root, &entry.file_name().to_string_lossy()).ok())
        .collect::<Vec<_>>();
    summaries.sort_by(|left, right| right.last_event_at_ms.cmp(&left.last_event_at_ms));
    Ok(summaries)
}

/// Reads one redacted session summary by validated identifier.
///
/// # Errors
///
/// Returns an error when the identifier or journal is invalid, missing, or
/// exceeds the bounded event count.
pub fn session_summary(root: &Path, id: &str) -> Result<SessionSummary> {
    const MAX_EVENTS: usize = 100_000;
    let id = SessionId::new(id)?;
    let path = root.join(id.as_str()).join("events.jsonl");
    let file = File::open(&path)
        .with_context(|| format!("failed to open session journal {}", path.display()))?;
    let mut summary = None;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        if index >= MAX_EVENTS {
            bail!("session journal exceeds the event limit");
        }
        let event: SessionEvent = serde_json::from_str(
            &line.with_context(|| format!("failed to read session journal {}", path.display()))?,
        )
        .with_context(|| format!("invalid session event in {}", path.display()))?;
        apply_summary_event(&mut summary, &event)?;
    }
    summary.context("session journal has no start event")
}

fn apply_summary_event(summary: &mut Option<SessionSummary>, event: &SessionEvent) -> Result<()> {
    let at_ms = event_timestamp(event);
    if let SessionEvent::SessionStarted {
        session_id,
        workspace,
        mode,
        ..
    } = event
    {
        if summary.is_some() {
            bail!("session journal contains multiple start events");
        }
        *summary = Some(SessionSummary {
            id: session_id.clone(),
            workspace: workspace.clone(),
            mode: *mode,
            started_at_ms: at_ms,
            last_event_at_ms: at_ms,
            turns: 0,
            last_status: None,
        });
        return Ok(());
    }
    let summary = summary
        .as_mut()
        .context("session journal event precedes its start event")?;
    summary.last_event_at_ms = at_ms;
    match event {
        SessionEvent::ModeChanged { mode, .. } => summary.mode = *mode,
        SessionEvent::TurnFinished { status, .. } => {
            summary.turns = summary.turns.saturating_add(1);
            summary.last_status = Some(*status);
        }
        _ => {}
    }
    Ok(())
}

const fn event_timestamp(event: &SessionEvent) -> u128 {
    match event {
        SessionEvent::SessionStarted { at_ms, .. }
        | SessionEvent::TurnStarted { at_ms, .. }
        | SessionEvent::ToolRequested { at_ms, .. }
        | SessionEvent::ToolFinished { at_ms, .. }
        | SessionEvent::TurnFinished { at_ms, .. }
        | SessionEvent::ModeChanged { at_ms, .. }
        | SessionEvent::ModelSelected { at_ms, .. } => *at_ms,
    }
}

/// Append-only JSONL writer for one session.
#[derive(Debug)]
pub struct SessionJournal {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl SessionJournal {
    /// Opens `<root>/<session-id>/events.jsonl` for append-only writes.
    ///
    /// # Errors
    ///
    /// Returns an error when the session directory or journal cannot be opened.
    pub fn open(root: &Path, session_id: &SessionId) -> Result<Self> {
        let directory = root.join(session_id.as_str());
        fs::create_dir_all(&directory).with_context(|| {
            format!("failed to create session directory {}", directory.display())
        })?;
        let path = directory.join("events.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open session journal {}", path.display()))?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends and flushes one complete JSON event.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization or durable writing fails.
    pub fn append(&mut self, event: &SessionEvent) -> Result<()> {
        serde_json::to_writer(&mut self.writer, event)
            .context("failed to serialize session event")?;
        self.writer
            .write_all(b"\n")
            .context("failed to terminate session event")?;
        self.writer
            .flush()
            .context("failed to flush session event")?;
        Ok(())
    }
}

/// Active session state owned by the native Crumb runtime.
#[derive(Debug)]
pub struct AgentSession {
    id: SessionId,
    mode: AgentMode,
    workspace: PathBuf,
    cancellation: CancellationToken,
    journal: SessionJournal,
}

impl AgentSession {
    /// Starts and records a session.
    ///
    /// # Errors
    ///
    /// Returns an error when its start event cannot be persisted.
    pub fn start(
        id: SessionId,
        mode: AgentMode,
        workspace: PathBuf,
        mut journal: SessionJournal,
    ) -> Result<Self> {
        journal.append(&SessionEvent::SessionStarted {
            at_ms: timestamp_ms(),
            session_id: id.clone(),
            workspace: workspace.clone(),
            mode,
        })?;
        Ok(Self {
            id,
            mode,
            workspace,
            cancellation: CancellationToken::default(),
            journal,
        })
    }

    /// Reopens a previously validated session without writing a second start
    /// event.
    #[must_use]
    pub fn resume(
        id: SessionId,
        mode: AgentMode,
        workspace: PathBuf,
        journal: SessionJournal,
    ) -> Self {
        Self {
            id,
            mode,
            workspace,
            cancellation: CancellationToken::default(),
            journal,
        }
    }

    #[must_use]
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    #[must_use]
    pub fn mode(&self) -> AgentMode {
        self.mode
    }

    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Changes the operating mode and appends the transition.
    ///
    /// # Errors
    ///
    /// Returns an error when the transition cannot be persisted.
    pub fn set_mode(&mut self, mode: AgentMode) -> Result<()> {
        self.mode = mode;
        self.journal.append(&SessionEvent::ModeChanged {
            at_ms: timestamp_ms(),
            mode,
        })
    }

    /// Records the effective exact-model selection and reasoning effort.
    ///
    /// # Errors
    ///
    /// Returns an error when the selection cannot be persisted.
    pub fn record_model_selection(
        &mut self,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<()> {
        self.journal.append(&SessionEvent::ModelSelected {
            at_ms: timestamp_ms(),
            provider,
            model,
            reasoning_effort,
        })
    }

    /// Records only request size and digest, never the raw prompt.
    ///
    /// # Errors
    ///
    /// Returns an error when the event cannot be persisted.
    pub fn record_turn_start(&mut self, request: &str) -> Result<()> {
        self.cancellation = CancellationToken::default();
        self.journal.append(&SessionEvent::TurnStarted {
            at_ms: timestamp_ms(),
            request_bytes: request.len(),
            request_digest: digest(request.as_bytes()),
        })
    }

    /// Records a terminal turn state.
    ///
    /// # Errors
    ///
    /// Returns an error when the event cannot be persisted.
    pub fn record_turn_end(
        &mut self,
        status: TurnStatus,
        steps: u32,
        tool_calls: u32,
    ) -> Result<()> {
        self.journal.append(&SessionEvent::TurnFinished {
            at_ms: timestamp_ms(),
            status,
            steps,
            tool_calls,
        })
    }
}

#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::{CancellationToken, SessionEvent, SessionId};

    #[test]
    fn cancellation_is_shared_without_waiting() {
        let token = CancellationToken::default();
        let observer = token.clone();
        token.cancel();
        assert!(observer.is_cancelled());
    }

    #[test]
    fn session_ids_are_directory_safe() {
        assert!(SessionId::new("work_2026-08-29").is_ok());
        assert!(SessionId::new("../escape").is_err());
    }

    #[test]
    fn turn_events_do_not_contain_raw_prompts() {
        let event = SessionEvent::TurnStarted {
            at_ms: 1,
            request_bytes: 12,
            request_digest: "digest".to_owned(),
        };
        let encoded = serde_json::to_string(&event).expect("event should serialize");
        assert!(!encoded.contains("prompt"));
        assert!(encoded.contains("request_digest"));
    }
}
