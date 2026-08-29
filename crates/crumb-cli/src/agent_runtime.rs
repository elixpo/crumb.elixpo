use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crumb_agent::session::TurnStatus;
use crumb_agent::{
    AgentConfig, AgentMode, AgentSession, CancellationToken, HarnessConfig, Modality, SessionId,
    SessionJournal, session_summary,
};
use crumb_auth::{CredentialStore, OsCredentialStore, SecretString};
use crumb_harness_dsh::{
    HarnessEnvironment, HarnessIdentity, HarnessLaunch, HarnessSupervisor, Notification, RunResult,
    SupervisorLimits,
};

pub struct AgentRuntime {
    active_cancellation: Arc<Mutex<Option<CancellationToken>>>,
    session: Option<AgentSession>,
    supervisor: Option<HarnessSupervisor>,
    supervisor_limits: Option<SupervisorLimits>,
    environment_revision: u64,
}

impl AgentRuntime {
    /// Installs the interrupt bridge without starting an AI process.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system signal handler cannot be
    /// installed.
    pub fn new() -> Result<Self> {
        let active_cancellation: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
        let signal_slot = Arc::clone(&active_cancellation);
        ctrlc::set_handler(move || {
            if let Ok(active) = signal_slot.lock()
                && let Some(cancellation) = active.as_ref()
            {
                cancellation.cancel();
            }
        })
        .context("failed to install agent cancellation handler")?;
        Ok(Self {
            active_cancellation,
            session: None,
            supervisor: None,
            supervisor_limits: None,
            environment_revision: 1,
        })
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

    /// Executes one natural-language turn through the configured Harness.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when configuration, credentials, persistence,
    /// Harness startup, model execution, or cancellation fails.
    pub fn run(
        &mut self,
        request: &str,
        config: &AgentConfig,
        workspace: &Path,
    ) -> Result<RunResult> {
        self.run_with_events(request, config, workspace, |_| Ok(()))
    }

    /// Executes one turn and forwards bounded Harness notifications as they
    /// arrive.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::run`], including observer errors.
    pub fn run_with_events(
        &mut self,
        request: &str,
        config: &AgentConfig,
        workspace: &Path,
        on_notification: impl FnMut(&Notification) -> Result<()>,
    ) -> Result<RunResult> {
        let workspace = std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
        self.ensure_session(&workspace, config.mode)?;
        let route = config
            .models
            .get(&Modality::Text)
            .and_then(|routes| routes.first())
            .context("agent config has no text model route")?;
        let effort = config.reasoning_effort_for(route).map(str::to_owned);
        let limits = supervisor_limits(config)?;
        self.ensure_supervisor(limits)?;
        let launch = harness_launch(
            config,
            &workspace,
            route,
            effort.clone(),
            self.environment_revision,
        )?;

        let session = self.session.as_mut().context("agent session unavailable")?;
        if session.mode() != config.mode {
            session.set_mode(config.mode)?;
        }
        session.record_model_selection(
            route.provider.clone(),
            route.model.clone(),
            effort.clone(),
        )?;
        session.record_turn_start(request)?;
        let cancellation = session.cancellation_token();
        let _active = ActiveCancellation::new(&self.active_cancellation, cancellation.clone())?;
        let session_id = session.id().as_str().to_owned();
        let result = self
            .supervisor
            .as_mut()
            .context("Harness supervisor unavailable")?
            .run_text_with_events(launch, &session_id, request, &cancellation, on_notification);
        let status = match &result {
            Ok(_) => TurnStatus::Complete,
            Err(_) if cancellation.is_cancelled() => TurnStatus::Cancelled,
            Err(_) => TurnStatus::Failed,
        };
        session.record_turn_end(status, 0, 0)?;
        result
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
}

impl<'a> ActiveCancellation<'a> {
    fn new(
        slot: &'a Mutex<Option<CancellationToken>>,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        *slot
            .lock()
            .map_err(|_| anyhow::anyhow!("agent cancellation state is unavailable"))? =
            Some(cancellation);
        Ok(Self { slot })
    }
}

impl Drop for ActiveCancellation<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.slot.lock() {
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
    let credential = pollinations_credential()?;
    let mut environment = HarnessEnvironment::runtime_basics();
    environment.insert("DSH_CORDIS_CONFIG", composition.as_os_str());
    environment.insert("DSH_CWD", workspace.as_os_str());
    environment.insert("DSH_SESSION_ROOT", session_root.as_os_str());
    environment.insert("CRUMB_MCP_COMMAND", crumb_program.as_os_str());
    environment.insert("POLLINATIONS_API_KEY", credential.expose());
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
            max_tokens: None,
            environment_revision,
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

fn pollinations_credential() -> Result<SecretString> {
    if let Ok(value) = std::env::var("POLLINATIONS_API_KEY")
        && !value.trim().is_empty()
    {
        return Ok(SecretString::new(value));
    }
    OsCredentialStore::new()?
        .get()?
        .context("Pollinations is not connected; run `crumb auth login`")
}

fn new_session_id() -> Result<SessionId> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    SessionId::new(format!("crumb-{}-{timestamp}", std::process::id()))
}
