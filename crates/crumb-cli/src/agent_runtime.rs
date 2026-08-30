use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crumb_agent::session::TurnStatus;
use crumb_agent::{
    AgentConfig, AgentMode, AgentSession, BackendDiscovery, CancellationToken, HarnessConfig,
    Modality, SessionId, SessionJournal, session_summary,
};
use crumb_harness_cli::{CodingCliLaunch, run_text as run_coding_cli_text};
use crumb_harness_dsh::{
    HarnessActivity, HarnessEnvironment, HarnessIdentity, HarnessLaunch, HarnessSupervisor,
    Notification, RunResult, SupervisorLimits,
};

use crate::provider_projection::project_provider;

type TurnThreadResult = (AgentRuntime, Result<RunResult>);
static ACTIVE_CANCELLATION: OnceLock<Arc<Mutex<Option<CancellationToken>>>> = OnceLock::new();

/// Background Harness turn whose public event stream is already redacted.
pub struct AgentTurnTask {
    activities: Receiver<HarnessActivity>,
    cancellation: CancellationToken,
    worker: JoinHandle<TurnThreadResult>,
}

impl AgentTurnTask {
    /// Waits for one redacted activity state without blocking indefinitely.
    ///
    /// # Errors
    ///
    /// Returns the standard timeout or disconnection state.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<HarnessActivity, RecvTimeoutError> {
        self.activities.recv_timeout(timeout)
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn detach_interrupt(&self) {
        if let Some(slot) = ACTIVE_CANCELLATION.get()
            && let Ok(mut active) = slot.lock()
            && active
                .as_ref()
                .is_some_and(|token| token.shares_signal_with(&self.cancellation))
        {
            *active = None;
        }
    }

    /// Rejoins the runtime after its turn completes.
    ///
    /// # Errors
    ///
    /// Returns an error only if the isolated worker panicked.
    pub fn finish(self) -> Result<TurnThreadResult> {
        self.worker
            .join()
            .map_err(|_| anyhow::anyhow!("agent turn worker panicked"))
    }
}

pub struct AgentRuntime {
    active_cancellation: Arc<Mutex<Option<CancellationToken>>>,
    session: Option<AgentSession>,
    supervisor: Option<HarnessSupervisor>,
    supervisor_limits: Option<SupervisorLimits>,
    environment_revision: u64,
    review_notes: Vec<ReviewNote>,
    review_note_bytes: usize,
}

struct ReviewNote {
    checkpoint: String,
    comment: String,
}

impl AgentRuntime {
    /// Installs the interrupt bridge without starting an AI process.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system signal handler cannot be
    /// installed.
    pub fn new() -> Result<Self> {
        let active_cancellation = if let Some(slot) = ACTIVE_CANCELLATION.get() {
            Arc::clone(slot)
        } else {
            let slot: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
            let signal_slot = Arc::clone(&slot);
            ctrlc::set_handler(move || {
                if let Ok(active) = signal_slot.lock()
                    && let Some(cancellation) = active.as_ref()
                {
                    cancellation.cancel();
                }
            })
            .context("failed to install agent cancellation handler")?;
            let _ = ACTIVE_CANCELLATION.set(Arc::clone(&slot));
            slot
        };
        Ok(Self {
            active_cancellation,
            session: None,
            supervisor: None,
            supervisor_limits: None,
            environment_revision: 1,
            review_notes: Vec::new(),
            review_note_bytes: 0,
        })
    }

    /// Queues bounded review feedback for the next agent turn only.
    ///
    /// The note remains process-local and is never written to a session journal
    /// or checkpoint manifest by Crumb.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured message or byte ceiling is reached.
    pub fn queue_review_note(
        &mut self,
        checkpoint: &str,
        comment: &str,
        max_messages: usize,
        max_bytes: usize,
    ) -> Result<()> {
        let comment = comment.trim();
        if comment.is_empty() {
            bail!("review comment cannot be empty");
        }
        if self.review_notes.len() >= max_messages {
            bail!("review comment queue is full");
        }
        let added = checkpoint.len().saturating_add(comment.len());
        if self.review_note_bytes.saturating_add(added) > max_bytes {
            bail!("review comments exceed the configured byte limit");
        }
        self.review_notes.push(ReviewNote {
            checkpoint: checkpoint.to_owned(),
            comment: comment.to_owned(),
        });
        self.review_note_bytes = self.review_note_bytes.saturating_add(added);
        Ok(())
    }

    /// Selects a previous redacted session journal for the next agent turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is missing, belongs to another
    /// workspace, or cannot be reopened safely.
    pub fn resume(&mut self, workspace: &Path, session_id: &str) -> Result<()> {
        let workspace = std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
        let root = session_root(&workspace);
        let summary = session_summary(&root, session_id)?;
        if summary.archived {
            bail!("archived sessions must be restored before resuming");
        }
        if summary.workspace != workspace {
            bail!("session belongs to a different workspace");
        }
        if let Some(supervisor) = self.supervisor.as_mut() {
            supervisor.shutdown()?;
        }
        self.supervisor = None;
        self.supervisor_limits = None;
        self.environment_revision = self.environment_revision.saturating_add(1);
        let journal = SessionJournal::open(&root, &summary.id)?;
        self.session = Some(AgentSession::resume(
            summary.id,
            summary.mode,
            workspace,
            journal,
        ));
        Ok(())
    }

    #[must_use]
    pub fn active_session_id(&self) -> Option<&str> {
        self.session.as_ref().map(|session| session.id().as_str())
    }

    /// Runs a turn off the terminal input thread and emits only bounded,
    /// redacted activity states.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero channel capacity or worker spawn failure.
    pub fn spawn_turn(
        mut self,
        request: String,
        config: AgentConfig,
        workspace: PathBuf,
        activity_capacity: usize,
    ) -> Result<AgentTurnTask> {
        if activity_capacity == 0 {
            bail!("agent activity channel capacity must be positive");
        }
        let request = self.attach_review_notes(request);
        let (activity_sender, activities) = sync_channel(activity_capacity);
        let cancellation = CancellationToken::default();
        let worker_cancellation = cancellation.clone();
        let worker = thread::Builder::new()
            .name("crumb-agent-turn".to_owned())
            .spawn(move || {
                let mut runtime = self;
                let result = runtime.run_with_events_using(
                    &request,
                    &config,
                    &workspace,
                    &worker_cancellation,
                    |notification| {
                        if let Some(activity) = notification.activity() {
                            activity_sender
                                .send(activity)
                                .map_err(|_| anyhow::anyhow!("agent activity receiver closed"))?;
                        }
                        Ok(())
                    },
                );
                (runtime, result)
            })
            .context("failed to start agent turn worker")?;
        Ok(AgentTurnTask {
            activities,
            cancellation,
            worker,
        })
    }

    /// Runs one explicitly selected local job with the foreground policy and
    /// cancellation boundary, but without terminal rendering.
    ///
    /// # Errors
    ///
    /// Returns Harness, policy, credential, or session failures.
    pub fn run_local_job(
        &mut self,
        request: &str,
        config: &AgentConfig,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RunResult> {
        self.run_with_events_using(request, config, workspace, cancellation, |_| Ok(()))
    }

    /// Creates or selects the redacted session journal before a local job runs.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace or session journal is unavailable.
    pub fn prepare_local_job(&mut self, mode: AgentMode, workspace: &Path) -> Result<SessionId> {
        let workspace = std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
        self.ensure_session(&workspace, mode)?;
        self.session
            .as_ref()
            .map(|session| session.id().clone())
            .context("agent session is unavailable")
    }

    fn attach_review_notes(&mut self, request: String) -> String {
        if self.review_notes.is_empty() {
            return request;
        }
        let mut combined = String::from("Review feedback for this turn:\n");
        for note in self.review_notes.drain(..) {
            combined.push_str("- checkpoint ");
            combined.push_str(&note.checkpoint);
            combined.push_str(": ");
            combined.push_str(&note.comment);
            combined.push('\n');
        }
        self.review_note_bytes = 0;
        combined.push_str("\nUser request:\n");
        combined.push_str(&request);
        combined
    }

    fn run_with_events_using(
        &mut self,
        request: &str,
        config: &AgentConfig,
        workspace: &Path,
        cancellation: &CancellationToken,
        on_notification: impl FnMut(&Notification) -> Result<()>,
    ) -> Result<RunResult> {
        let workspace = std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
        config.validate()?;
        self.ensure_session(&workspace, config.mode)?;
        let route = config
            .models
            .get(&Modality::Text)
            .and_then(|routes| routes.first())
            .context("agent config has no text model route")?;
        let effort = config.reasoning_effort_for(route).map(str::to_owned);
        let session_id = self.start_turn(config.mode, route, effort.clone(), request)?;
        let cancellation_slot = Arc::clone(&self.active_cancellation);
        let _active = ActiveCancellation::new(&cancellation_slot, cancellation.clone())?;
        let mut on_notification = on_notification;
        let result = match config
            .harness
            .as_ref()
            .context("agent Harness is not configured")?
        {
            HarnessConfig::Process { .. } => self.run_process_harness(
                config,
                &workspace,
                route,
                effort,
                &session_id,
                request,
                cancellation,
                &mut on_notification,
            ),
            HarnessConfig::CodingCli {
                backend, command, ..
            } => self.run_coding_cli(
                config,
                &workspace,
                route,
                *backend,
                command,
                effort.as_deref(),
                &session_id,
                request,
                cancellation,
                &mut on_notification,
            ),
            HarnessConfig::Native => {
                Err(anyhow::anyhow!("native agent Harness is not implemented"))
            }
        };
        let status = match &result {
            Ok(_) => TurnStatus::Complete,
            Err(_) if cancellation.is_cancelled() => TurnStatus::Cancelled,
            Err(_) => TurnStatus::Failed,
        };
        self.session
            .as_mut()
            .context("agent session unavailable")?
            .record_turn_end(status, 0, 0)?;
        result
    }

    fn start_turn(
        &mut self,
        mode: AgentMode,
        route: &crumb_agent::ModelRoute,
        effort: Option<String>,
        request: &str,
    ) -> Result<String> {
        let session = self.session.as_mut().context("agent session unavailable")?;
        if session.mode() != mode {
            session.set_mode(mode)?;
        }
        session.record_model_selection(route.provider.clone(), route.model.clone(), effort)?;
        session.record_turn_start(request)?;
        Ok(session.id().as_str().to_owned())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_process_harness(
        &mut self,
        config: &AgentConfig,
        workspace: &Path,
        route: &crumb_agent::ModelRoute,
        effort: Option<String>,
        session_id: &str,
        request: &str,
        cancellation: &CancellationToken,
        on_notification: &mut impl FnMut(&Notification) -> Result<()>,
    ) -> Result<RunResult> {
        let limits = supervisor_limits(config)?;
        self.ensure_supervisor(limits)?;
        let launch = harness_launch(config, workspace, route, effort, self.environment_revision)?;
        self.supervisor
            .as_mut()
            .context("Harness supervisor unavailable")?
            .run_text_with_events(launch, session_id, request, cancellation, on_notification)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_coding_cli(
        &mut self,
        config: &AgentConfig,
        workspace: &Path,
        route: &crumb_agent::ModelRoute,
        backend: crumb_agent::CodingBackend,
        command: &Path,
        effort: Option<&str>,
        session_id: &str,
        request: &str,
        cancellation: &CancellationToken,
        on_notification: &mut impl FnMut(&Notification) -> Result<()>,
    ) -> Result<RunResult> {
        self.clear_supervisor()?;
        let discovery = BackendDiscovery::discover(backend, command);
        let executable = discovery.executable.with_context(|| {
            format!(
                "selected {backend:?} CLI `{}` is unavailable",
                command.display()
            )
        })?;
        let running = Notification {
            method: "session.status".to_owned(),
            params: serde_json::json!({ "sessionId": session_id, "status": "running" }),
        };
        on_notification(&running)?;
        let crumb_program = std::env::current_exe().context("failed to locate crumb executable")?;
        let launch = CodingCliLaunch {
            backend,
            executable: &executable,
            workspace,
            mcp_command: &crumb_program,
            session_id,
            model: &route.model,
            reasoning_effort: effort,
            mode: config.mode,
            workspace_write: config
                .permissions
                .allow_workspace_tools
                .contains("write_file"),
            max_turns: config.limits.max_steps,
            timeout: Duration::from_secs(config.limits.max_wall_time_seconds),
            output_limit: usize::try_from(config.limits.max_output_bytes)
                .context("max_output_bytes exceeds this platform's address space")?,
        };
        let result = run_coding_cli_text(&launch, request, cancellation)?;
        for notification in &result.notifications {
            on_notification(notification)?;
        }
        Ok(result)
    }

    fn ensure_session(&mut self, workspace: &Path, mode: AgentMode) -> Result<()> {
        let changed = self
            .session
            .as_ref()
            .is_some_and(|session| session.workspace() != workspace);
        if changed {
            if let Some(supervisor) = self.supervisor.as_mut() {
                let _ = supervisor.shutdown();
            }
            self.supervisor = None;
            self.supervisor_limits = None;
            self.session = None;
            self.environment_revision = self.environment_revision.saturating_add(1);
        }
        if self.session.is_none() {
            let id = new_session_id()?;
            let journal = SessionJournal::open(&session_root(workspace), &id)?;
            self.session = Some(AgentSession::start(
                id,
                mode,
                workspace.to_path_buf(),
                journal,
            )?);
        }
        Ok(())
    }

    fn ensure_supervisor(&mut self, limits: SupervisorLimits) -> Result<()> {
        if self.supervisor_limits != Some(limits) {
            if let Some(supervisor) = self.supervisor.as_mut() {
                supervisor.shutdown()?;
            }
            self.supervisor = Some(HarnessSupervisor::new(limits));
            self.supervisor_limits = Some(limits);
        }
        Ok(())
    }

    fn clear_supervisor(&mut self) -> Result<()> {
        if let Some(mut supervisor) = self.supervisor.take() {
            supervisor.shutdown()?;
        }
        self.supervisor_limits = None;
        Ok(())
    }
}

fn session_root(workspace: &Path) -> PathBuf {
    workspace.join(".crumb").join("sessions").join("crumb")
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        if let Some(supervisor) = self.supervisor.as_mut() {
            let _ = supervisor.shutdown();
        }
    }
}

struct ActiveCancellation<'a> {
    slot: &'a Mutex<Option<CancellationToken>>,
    cancellation: CancellationToken,
}

impl<'a> ActiveCancellation<'a> {
    fn new(
        slot: &'a Mutex<Option<CancellationToken>>,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        *slot
            .lock()
            .map_err(|_| anyhow::anyhow!("agent cancellation state is unavailable"))? =
            Some(cancellation.clone());
        Ok(Self { slot, cancellation })
    }
}

impl Drop for ActiveCancellation<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.slot.lock()
            && active
                .as_ref()
                .is_some_and(|token| token.shares_signal_with(&self.cancellation))
        {
            *active = None;
        }
    }
}

fn supervisor_limits(config: &AgentConfig) -> Result<SupervisorLimits> {
    Ok(SupervisorLimits {
        initialize_timeout: Duration::from_secs(config.limits.max_harness_initialize_seconds),
        run_timeout: Duration::from_secs(config.limits.max_wall_time_seconds),
        shutdown_timeout: Duration::from_secs(config.limits.max_harness_shutdown_seconds),
        event_budget_bytes: usize::try_from(config.limits.max_output_bytes)
            .context("max_output_bytes exceeds this platform's address space")?,
    })
}

fn harness_launch(
    config: &AgentConfig,
    workspace: &Path,
    route: &crumb_agent::ModelRoute,
    reasoning_effort: Option<String>,
    environment_revision: u64,
) -> Result<HarnessLaunch> {
    let HarnessConfig::Process {
        command,
        arguments,
        cordis,
    } = config
        .harness
        .as_ref()
        .context("agent Harness is not configured")?
    else {
        bail!("native agent Harness is not implemented");
    };
    let composition = resolve_composition(workspace, cordis.as_deref())?;
    let session_root = workspace.join(".crumb").join("sessions").join("harness");
    std::fs::create_dir_all(&session_root).context("failed to create Harness session root")?;
    let session_root =
        std::fs::canonicalize(session_root).context("failed to resolve Harness session root")?;
    let crumb_program = std::env::current_exe().context("failed to locate crumb executable")?;
    let projection = project_provider(config, route)?;
    let mut environment = HarnessEnvironment::runtime_basics();
    environment.insert("DSH_CORDIS_CONFIG", composition.as_os_str());
    environment.insert("DSH_CWD", workspace.as_os_str());
    environment.insert("DSH_SESSION_ROOT", session_root.as_os_str());
    environment.insert("CRUMB_MCP_COMMAND", crumb_program.as_os_str());
    environment.insert(
        "CRUMB_HARNESS_PROVIDERS",
        projection.providers_json.as_str(),
    );
    for (name, secret) in &projection.environment {
        environment.insert(name.as_str(), secret.expose());
    }
    if let Some(search_credential) = crate::pollinations_environment_key() {
        environment.insert("POLLINATIONS_API_KEY", search_credential);
    }
    Ok(HarnessLaunch {
        identity: HarnessIdentity {
            program: command.clone(),
            arguments: arguments.clone(),
            cwd: workspace.to_path_buf(),
            composition,
            mode: config.mode,
            provider: route.provider.clone(),
            model: route.model.clone(),
            reasoning_effort,
            max_tokens: projection.max_tokens,
            environment_revision: environment_revision ^ projection.revision,
        },
        environment,
    })
}

fn resolve_composition(workspace: &Path, configured: Option<&Path>) -> Result<PathBuf> {
    let selected = configured
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("CRUMB_CORDIS_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("config/harness/crumb.cordis.yml"));
    let candidate = if selected.is_absolute() {
        selected
    } else {
        workspace.join(selected)
    };
    std::fs::canonicalize(&candidate)
        .with_context(|| format!("failed to resolve Cordis config `{}`", candidate.display()))
}

fn new_session_id() -> Result<SessionId> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    SessionId::new(format!("crumb-{}-{timestamp}", std::process::id()))
}
