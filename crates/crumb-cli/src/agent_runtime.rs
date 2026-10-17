use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crumb_agent::config::SkillConfig;
use crumb_agent::session::TurnStatus;
use crumb_agent::{
    AgentConfig, AgentMode, AgentSession, ApprovalBroker, ApprovalDecision, ApprovalInbox,
    ApprovalRequest, BackendDiscovery, CancellationToken, ConfiguredApprovals, HarnessConfig,
    InteractiveApprovalBroker, Modality, SessionAttachment, SessionId, SessionJournal, ToolHost,
    approval_channel, attach_session_context, detach_session_context, session_attachments,
    session_summary,
};
use crumb_harness::{
    Conversation, EventSink, HarnessEvent, HarnessLimits, HarnessTurn, NativeHarness,
    ParallelAgentConfig, ParallelAgentLimits, register_parallel_agent_tool,
};
use crumb_harness_cli::{CodingCliLaunch, run_text as run_coding_cli_text};
use crumb_harness_dsh::{
    HarnessActivity, HarnessEnvironment, HarnessIdentity, HarnessLaunch, HarnessSupervisor,
    Notification, RunResult, SupervisorLimits,
};
use crumb_memory::{MemorySet, RetrievalPolicy};
use crumb_pollinations::{PollinationsConfig, PollinationsProvider};
use crumb_profile::{ProfileDocument, ProfileSelectionStore, ProfileStore};

use crate::provider_projection::project_provider;

type TurnThreadResult = (AgentRuntime, Result<RunResult>);
static ACTIVE_CANCELLATION: OnceLock<Arc<Mutex<Option<CancellationToken>>>> = OnceLock::new();

struct TurnApprovals {
    configured: ConfiguredApprovals,
    interactive: InteractiveApprovalBroker,
}

struct TurnResources {
    tools: ToolHost,
    approvals: Arc<dyn ApprovalBroker>,
}

struct TurnInteraction {
    interactive: Option<InteractiveApprovalBroker>,
}

struct PreparedTurn<'a> {
    config: &'a AgentConfig,
    workspace: &'a Path,
    route: &'a crumb_agent::ModelRoute,
    effort: Option<String>,
    session_id: &'a str,
    request: &'a str,
    persona: Option<&'a str>,
    trusted_context: &'a [String],
    cancellation: &'a CancellationToken,
}

impl ApprovalBroker for TurnApprovals {
    fn decide(
        &self,
        request: &ApprovalRequest,
        arguments: &serde_json::Value,
        cancellation: &CancellationToken,
    ) -> ApprovalDecision {
        let configured = self.configured.decide(request, arguments, cancellation);
        if configured == ApprovalDecision::AllowOnce {
            configured
        } else {
            self.interactive.decide(request, arguments, cancellation)
        }
    }
}

fn configured_approvals(config: &AgentConfig) -> ConfiguredApprovals {
    ConfiguredApprovals::new(config.permissions.allow_network_tools.clone())
        .with_mode(config.permissions.mode)
        .with_workspace_tools(config.permissions.allow_workspace_tools.clone())
        .with_process_tools(config.permissions.allow_execution_tools.clone())
        .with_mcp_tools(config.permissions.allow_mcp_tools.clone())
}

fn native_harness_limits(config: &AgentConfig) -> Result<HarnessLimits> {
    Ok(HarnessLimits {
        max_model_rounds: NonZeroUsize::new(
            usize::try_from(config.limits.max_steps)
                .context("max_steps exceeds this platform's address space")?,
        )
        .context("max_steps must be positive")?,
        max_tool_calls: NonZeroUsize::new(
            usize::try_from(config.limits.max_tool_calls)
                .context("max_tool_calls exceeds this platform's address space")?,
        )
        .context("max_tool_calls must be positive")?,
        max_output_bytes: NonZeroUsize::new(
            usize::try_from(config.limits.max_output_bytes)
                .context("max_output_bytes exceeds this platform's address space")?,
        )
        .context("max_output_bytes must be positive")?,
        max_history_turns: NonZeroUsize::new(12).expect("constant is non-zero"),
        turn_timeout: Duration::from_secs(config.limits.max_wall_time_seconds),
    })
}

fn parallel_harness_limits(parent: HarnessLimits) -> HarnessLimits {
    HarnessLimits {
        max_model_rounds: NonZeroUsize::new(parent.max_model_rounds.get().min(4))
            .expect("parent limits are non-zero"),
        max_tool_calls: NonZeroUsize::new(parent.max_tool_calls.get().min(8))
            .expect("parent limits are non-zero"),
        max_output_bytes: NonZeroUsize::new(parent.max_output_bytes.get().min(16 * 1024))
            .expect("parent limits are non-zero"),
        max_history_turns: NonZeroUsize::MIN,
        turn_timeout: parent.turn_timeout,
    }
}

fn configured_max_output_tokens(
    config: &AgentConfig,
    route: &crumb_agent::ModelRoute,
) -> Option<u32> {
    config
        .providers
        .get(&route.provider)
        .and_then(|provider| {
            provider
                .models
                .iter()
                .find(|model| model.id == route.model)
                .and_then(|model| model.max_output_tokens)
                .or(provider.default_max_output_tokens)
        })
        .map(|tokens| u32::try_from(tokens).unwrap_or(u32::MAX))
}

struct NativeEventBridge<'a> {
    on_activity: &'a mut dyn FnMut(HarnessActivity) -> Result<()>,
    error: Option<anyhow::Error>,
}

impl<'a> NativeEventBridge<'a> {
    fn new(on_activity: &'a mut dyn FnMut(HarnessActivity) -> Result<()>) -> Self {
        Self {
            on_activity,
            error: None,
        }
    }

    fn finish(self) -> Result<()> {
        self.error.map_or(Ok(()), Err)
    }
}

impl EventSink for NativeEventBridge<'_> {
    fn emit(&mut self, event: HarnessEvent) {
        if self.error.is_some() {
            return;
        }
        let activity = match event {
            HarnessEvent::TurnStarted => Some(HarnessActivity::RequestAccepted),
            HarnessEvent::TextDelta(text) => Some(HarnessActivity::ResponseDelta { text }),
            HarnessEvent::ToolRequested { name, detail, .. } => {
                Some(HarnessActivity::ToolStarted { name, detail })
            }
            HarnessEvent::ToolProgress { label, .. } => Some(HarnessActivity::Progress { label }),
            HarnessEvent::ToolFinished { name, success, .. } => {
                Some(HarnessActivity::ToolFinished { name, success })
            }
            HarnessEvent::TurnFinished => Some(HarnessActivity::Completed {
                reason: "stop".to_owned(),
            }),
            HarnessEvent::Usage(_) => None,
        };
        if let Some(activity) = activity
            && let Err(error) = (self.on_activity)(activity)
        {
            self.error = Some(error);
        }
    }
}

fn project_notification(
    notification: &Notification,
    on_activity: &mut dyn FnMut(HarnessActivity) -> Result<()>,
) -> Result<()> {
    if let Some(activity) = notification.activity() {
        on_activity(activity)?;
    }
    Ok(())
}

/// Background Harness turn whose public event stream is already redacted.
pub struct AgentTurnTask {
    activities: Receiver<HarnessActivity>,
    approvals: ApprovalInbox,
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

    pub fn recv_approval_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<crumb_agent::PendingApproval, RecvTimeoutError> {
        self.approvals.recv_timeout(timeout)
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
    native_conversation: Option<Conversation>,
    environment_revision: u64,
    context_tokens_estimate: u64,
    session_tokens_estimate: u64,
    compactions: u32,
    review_notes: Vec<ReviewNote>,
    review_note_bytes: usize,
}

struct ReviewNote {
    checkpoint: String,
    comment: String,
}

/// Effective, permission-neutral profile projection for one agent turn.
pub struct ProfileApplication {
    pub profile: Option<ProfileDocument>,
    pub config: AgentConfig,
    pub model_source: &'static str,
    pub effort_source: &'static str,
    pub skill_source: &'static str,
}

/// Resolves the project selection and narrows an agent config with profile
/// preferences. No field in this projection can grant permissions.
///
/// # Errors
///
/// Returns an error when selected profile storage is malformed or unavailable.
pub fn apply_selected_profile(
    config: &AgentConfig,
    workspace: &Path,
) -> Result<ProfileApplication> {
    let selection = ProfileSelectionStore::for_project(project_root(workspace));
    let profiles = ProfileStore::discover()?;
    apply_profile_selection(config, &profiles, &selection)
}

fn apply_profile_selection(
    config: &AgentConfig,
    profiles: &ProfileStore,
    selection: &ProfileSelectionStore,
) -> Result<ProfileApplication> {
    let selection = selection.load()?;
    let Some(profile_id) = selection.profile_id else {
        return Ok(ProfileApplication {
            profile: None,
            config: config.clone(),
            model_source: "project",
            effort_source: "project",
            skill_source: "project",
        });
    };
    let profile = profiles.load(&profile_id)?;
    let mut effective = config.clone();
    let model_source = apply_profile_model(&profile, &mut effective);
    let effort_source = apply_profile_effort(&profile, &mut effective);
    let skill_source = apply_profile_skills(&profile, &mut effective);
    effective.validate()?;
    Ok(ProfileApplication {
        profile: Some(profile),
        config: effective,
        model_source,
        effort_source,
        skill_source,
    })
}

fn project_root(workspace: &Path) -> &Path {
    workspace
        .ancestors()
        .find(|directory| directory.join(".crumb/agent.json").is_file())
        .unwrap_or(workspace)
}

fn apply_profile_model(profile: &ProfileDocument, config: &mut AgentConfig) -> &'static str {
    let Some(preferred) = profile.preferred_models.get("text") else {
        return "project";
    };
    let Some(routes) = config.models.get_mut(&Modality::Text) else {
        return "project";
    };
    let Some(index) = routes.iter().position(|route| {
        preferred == &route.model || preferred == &format!("{}/{}", route.provider, route.model)
    }) else {
        return "project";
    };
    routes.swap(0, index);
    "profile"
}

fn apply_profile_effort(profile: &ProfileDocument, config: &mut AgentConfig) -> &'static str {
    let Some(effort) = profile.effort.as_deref().and_then(profile_effort_wire_name) else {
        return "project";
    };
    let mut candidate = config.clone();
    let Some(route) = candidate
        .models
        .get_mut(&Modality::Text)
        .and_then(|routes| routes.first_mut())
    else {
        return "project";
    };
    route.reasoning_effort = Some(effort.to_owned());
    if candidate.validate().is_err() {
        return "project";
    }
    *config = candidate;
    "profile"
}

fn profile_effort_wire_name(effort: &str) -> Option<&'static str> {
    match effort {
        "light" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "extra high" => Some("xhigh"),
        "ultra" => Some("max"),
        _ => None,
    }
}

fn apply_profile_skills(profile: &ProfileDocument, config: &mut AgentConfig) -> &'static str {
    if profile.skills.is_empty() {
        return "project";
    }
    let enabled = profile
        .skills
        .iter()
        .filter(|reference| reference.enabled)
        .map(|reference| reference.id.as_str())
        .collect::<BTreeSet<_>>();
    for skill in &mut config.skills {
        skill.enabled &= enabled.contains(skill.id.as_str());
    }
    "profile"
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
            native_conversation: None,
            environment_revision: 1,
            context_tokens_estimate: 0,
            session_tokens_estimate: 0,
            compactions: 0,
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
        self.native_conversation = None;
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

    /// Ensures a session exists and persists one typed context reference.
    pub fn attach_context(
        &mut self,
        workspace: &Path,
        mode: AgentMode,
        attachment: SessionAttachment,
    ) -> Result<bool> {
        let workspace = std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
        self.ensure_session(&workspace, mode)?;
        let session = self
            .session
            .as_ref()
            .context("agent session is unavailable")?;
        attach_session_context(&session_root(&workspace), session.id().as_str(), attachment)
    }

    /// Removes one typed context reference from the active session.
    pub fn detach_context(
        &mut self,
        workspace: &Path,
        attachment: &SessionAttachment,
    ) -> Result<bool> {
        let workspace = std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
        let session = self.session.as_ref().context("no active agent session")?;
        if session.workspace() != workspace {
            bail!("active session belongs to a different workspace");
        }
        detach_session_context(&session_root(&workspace), session.id().as_str(), attachment)
    }

    /// Returns persistent context references for the active session.
    pub fn attached_context(&self) -> Result<Vec<SessionAttachment>> {
        let Some(session) = self.session.as_ref() else {
            return Ok(Vec::new());
        };
        session_attachments(&session_root(session.workspace()), session.id().as_str())
    }

    /// Returns estimated context, total session tokens, and compaction count.
    #[must_use]
    pub const fn token_usage(&self) -> (u64, u64, u32) {
        (
            self.context_tokens_estimate,
            self.session_tokens_estimate,
            self.compactions,
        )
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
        let (interactive, approval_inbox) = approval_channel(NonZeroUsize::MIN);
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
                    TurnInteraction {
                        interactive: Some(interactive),
                    },
                    &worker_cancellation,
                    |activity| {
                        activity_sender
                            .send(activity)
                            .map_err(|_| anyhow::anyhow!("agent activity receiver closed"))?;
                        Ok(())
                    },
                );
                (runtime, result)
            })
            .context("failed to start agent turn worker")?;
        Ok(AgentTurnTask {
            activities,
            approvals: approval_inbox,
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
        self.run_with_events_using(
            request,
            config,
            workspace,
            TurnInteraction { interactive: None },
            cancellation,
            |_| Ok(()),
        )
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
        interaction: TurnInteraction,
        cancellation: &CancellationToken,
        on_activity: impl FnMut(HarnessActivity) -> Result<()>,
    ) -> Result<RunResult> {
        let workspace = std::fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
        let application = apply_selected_profile(config, &workspace)
            .context("selected persona profile could not be applied")?;
        let config = &application.config;
        config.validate()?;
        let tools = crate::workspace_read_host(
            &workspace,
            config,
            crate::configured_pollinations_runtime_key(config),
        )?;
        let configured = configured_approvals(config);
        let approvals: Arc<dyn ApprovalBroker> = match interaction.interactive {
            Some(interactive) => Arc::new(TurnApprovals {
                configured,
                interactive,
            }),
            None => Arc::new(configured),
        };
        self.ensure_session(&workspace, config.mode)?;
        let route = config
            .models
            .get(&Modality::Text)
            .and_then(|routes| routes.first())
            .context("agent config has no text model route")?;
        let effort = config.reasoning_effort_for(route).map(str::to_owned);
        let native = matches!(config.harness, Some(HarnessConfig::Native));
        let session_id = if native {
            self.prepare_native_turn(config.mode, route, effort.clone())?
        } else {
            self.start_turn(config.mode, route, effort.clone(), request)?
        };
        let persona = application.profile.as_ref().map(profile_persona_context);
        let profiled_request = application.profile.as_ref().map_or_else(
            || request.to_owned(),
            |profile| attach_profile_context(request, profile),
        );
        let memory = attach_approved_memory(
            &profiled_request,
            request,
            &workspace,
            application.profile.as_ref(),
        );
        if let Some(audit) = memory.audit {
            self.session
                .as_mut()
                .context("agent session is unavailable")?
                .record_memory_retrieval(audit.record_ids, audit.conflict_topics, audit.omitted)?;
        }
        let skill_context = load_enabled_skill_context(&config.skills, &workspace)?;
        let prepared_request = attach_skill_context(&memory.request, &skill_context);
        let mut trusted_context = memory.context.into_iter().collect::<Vec<_>>();
        trusted_context.extend(skill_context);
        let dispatched_request = if native {
            request
        } else {
            prepared_request.as_str()
        };
        let cancellation_slot = Arc::clone(&self.active_cancellation);
        let _active = ActiveCancellation::new(&cancellation_slot, cancellation.clone())?;
        let mut on_activity = on_activity;
        let result = self.dispatch_harness_turn(
            PreparedTurn {
                config,
                workspace: &workspace,
                route,
                effort,
                session_id: &session_id,
                request: dispatched_request,
                persona: persona.as_deref(),
                trusted_context: &trusted_context,
                cancellation,
            },
            TurnResources { tools, approvals },
            &mut on_activity,
        );
        let status = match &result {
            Ok(_) => TurnStatus::Complete,
            Err(_) if cancellation.is_cancelled() => TurnStatus::Cancelled,
            Err(_) => TurnStatus::Failed,
        };
        self.record_token_estimate(
            &prepared_request,
            result.as_ref().ok(),
            context_token_budget(config, route),
            matches!(config.harness.as_ref(), Some(HarnessConfig::Process { .. })),
        );
        if !native {
            self.session
                .as_mut()
                .context("agent session unavailable")?
                .record_turn_end(status, 0, 0)?;
        }
        result
    }

    fn dispatch_harness_turn(
        &mut self,
        turn: PreparedTurn<'_>,
        resources: TurnResources,
        on_activity: &mut dyn FnMut(HarnessActivity) -> Result<()>,
    ) -> Result<RunResult> {
        match turn
            .config
            .harness
            .as_ref()
            .context("agent Harness is not configured")?
        {
            HarnessConfig::Process { .. } => self.run_process_harness(
                turn.config,
                turn.workspace,
                turn.route,
                turn.effort,
                turn.session_id,
                turn.request,
                turn.cancellation,
                &mut |notification| project_notification(notification, on_activity),
            ),
            HarnessConfig::CodingCli {
                backend, command, ..
            } => self.run_coding_cli(
                turn.config,
                turn.workspace,
                turn.route,
                *backend,
                command,
                turn.effort.as_deref(),
                turn.session_id,
                turn.request,
                turn.cancellation,
                &mut |notification| project_notification(notification, on_activity),
            ),
            HarnessConfig::Native => self.run_native_harness(
                turn.config,
                turn.route,
                turn.session_id,
                turn.request,
                turn.persona,
                turn.trusted_context,
                resources.tools,
                resources.approvals,
                turn.cancellation,
                on_activity,
            ),
        }
    }

    fn record_token_estimate(
        &mut self,
        request: &str,
        result: Option<&RunResult>,
        limit: u64,
        auto_compaction: bool,
    ) {
        let response = result.map_or("", |result| result.final_response.as_str());
        let tokens = estimate_tokens(request).saturating_add(estimate_tokens(response));
        self.session_tokens_estimate = self.session_tokens_estimate.saturating_add(tokens);
        self.context_tokens_estimate = self.context_tokens_estimate.saturating_add(tokens);
        let threshold = limit.saturating_mul(4) / 5;
        if auto_compaction && threshold > 0 && self.context_tokens_estimate >= threshold {
            self.context_tokens_estimate = limit.saturating_mul(16) / 100;
            self.compactions = self.compactions.saturating_add(1);
        }
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

    fn prepare_native_turn(
        &mut self,
        mode: AgentMode,
        route: &crumb_agent::ModelRoute,
        effort: Option<String>,
    ) -> Result<String> {
        let session = self.session.as_mut().context("agent session unavailable")?;
        if session.mode() != mode {
            session.set_mode(mode)?;
        }
        session.record_model_selection(route.provider.clone(), route.model.clone(), effort)?;
        Ok(session.id().as_str().to_owned())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_native_harness(
        &mut self,
        config: &AgentConfig,
        route: &crumb_agent::ModelRoute,
        session_id: &str,
        request: &str,
        persona: Option<&str>,
        trusted_context: &[String],
        mut tools: ToolHost,
        approvals: Arc<dyn ApprovalBroker>,
        cancellation: &CancellationToken,
        on_activity: &mut dyn FnMut(HarnessActivity) -> Result<()>,
    ) -> Result<RunResult> {
        if route.provider != "pollinations" {
            bail!(
                "native Harness currently supports the Pollinations provider; selected `{}`",
                route.provider
            );
        }
        let provider = config
            .providers
            .get(&route.provider)
            .context("native Harness provider is not configured")?;
        let credential = provider
            .credential
            .as_ref()
            .map(crate::provider_projection::resolve_credential_secret)
            .transpose()?
            .map_or_else(crate::pollinations_runtime_key, |secret| {
                Some(secret.expose().to_owned())
            })
            .context("Pollinations credential is unavailable")?;
        let base_url = provider
            .base_url
            .trim_end_matches('/')
            .strip_suffix("/v1")
            .unwrap_or(provider.base_url.trim_end_matches('/'));
        let provider: Arc<dyn crumb_llm::LlmProvider> = Arc::new(PollinationsProvider::new(
            PollinationsConfig::new(credential)?.with_base_url(base_url)?,
        )?);
        let max_output_tokens = configured_max_output_tokens(config, route);
        let team_limits = ParallelAgentLimits::default();
        let child_output_tokens = max_output_tokens.map(|tokens| {
            tokens.div_ceil(u32::try_from(team_limits.max_agents.get()).unwrap_or(u32::MAX))
        });
        let workspace = self
            .session
            .as_ref()
            .context("agent session is unavailable")?
            .workspace()
            .to_path_buf();
        let harness_limits = native_harness_limits(config)?;
        register_parallel_agent_tool(
            &mut tools,
            Arc::clone(&provider),
            ParallelAgentConfig {
                journal_root: session_root(&workspace).join("agents"),
                workspace,
                model: route.model.clone(),
                max_output_tokens: child_output_tokens,
                harness_limits: parallel_harness_limits(harness_limits),
                team_limits,
            },
        )?;
        let harness = NativeHarness::new(provider, tools, approvals, harness_limits);
        let conversation = self
            .native_conversation
            .get_or_insert_with(|| harness.conversation());
        let turn = HarnessTurn {
            model: route.model.clone(),
            user_message: request.to_owned(),
            persona: persona.map(str::to_owned),
            trusted_context: trusted_context.to_vec(),
            max_output_tokens,
        };
        let mut events = NativeEventBridge::new(on_activity);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to start native Harness runtime")?;
        let outcome = runtime.block_on(
            harness.run_turn(
                self.session
                    .as_mut()
                    .context("agent session is unavailable")?,
                conversation,
                turn,
                cancellation,
                &mut events,
            ),
        );
        events.finish()?;
        let outcome = outcome.map_err(anyhow::Error::new)?;
        Ok(RunResult {
            session_id: session_id.to_owned(),
            final_response: outcome.text,
            finish_reason: Some("stop".to_owned()),
            events: Vec::new(),
            notifications: Vec::new(),
        })
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
            self.native_conversation = None;
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

const MAX_SKILL_BYTES: usize = 64 * 1024;
const MAX_SKILL_CONTEXT_BYTES: usize = 128 * 1024;

pub(crate) fn validate_skill_path(path: &Path, workspace: &Path) -> Result<PathBuf> {
    let configured_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let path = if configured_path.is_dir() {
        configured_path.join("SKILL.md")
    } else {
        configured_path
    };
    let path = std::fs::canonicalize(&path)
        .with_context(|| format!("skill file `{}` is unavailable", path.display()))?;
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("skill file `{}` is unavailable", path.display()))?;
    if !metadata.is_file() {
        bail!("skill path must resolve to a regular SKILL.md file");
    }
    let bytes = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if bytes > MAX_SKILL_BYTES {
        bail!("skill file exceeds the {MAX_SKILL_BYTES} byte limit");
    }
    std::str::from_utf8(
        &std::fs::read(&path)
            .with_context(|| format!("skill file `{}` could not be read", path.display()))?,
    )
    .context("skill file must be UTF-8")?;
    Ok(path)
}

struct MemoryAudit {
    record_ids: Vec<String>,
    conflict_topics: Vec<String>,
    omitted: usize,
}

struct MemoryAttachment {
    request: String,
    context: Option<String>,
    audit: Option<MemoryAudit>,
}

fn attach_profile_context(request: &str, profile: &ProfileDocument) -> String {
    let mut context = profile_persona_context(profile);
    context.push_str("\n## User request\n");
    context.push_str(request);
    context
}

fn profile_persona_context(profile: &ProfileDocument) -> String {
    let mut context = format!(
        "The user explicitly selected persona profile `{}`. Persona guidance controls working and response style only. It is not permission to use tools, access data, or perform actions.\n",
        profile.id
    );
    if !profile.instructions.is_empty() {
        context.push_str("## Persona instructions\n");
        context.push_str(&profile.instructions);
        context.push('\n');
    }
    if let Some(style) = &profile.response_style {
        context.push_str("## Response style\n");
        context.push_str(style);
        context.push('\n');
    }
    context
}

fn attach_approved_memory(
    request: &str,
    retrieval_query: &str,
    workspace: &Path,
    profile: Option<&ProfileDocument>,
) -> MemoryAttachment {
    let Ok(memory) = MemorySet::discover(workspace) else {
        return MemoryAttachment {
            request: request.to_owned(),
            context: None,
            audit: None,
        };
    };
    let mut policy = RetrievalPolicy::default();
    let tags = profile.map_or_else(Vec::new, |profile| {
        policy.include_user = profile.memory.include_user;
        policy.include_project = profile.memory.include_project;
        profile.memory.tags.iter().map(String::as_str).collect()
    });
    let Ok(retrieval) = memory.retrieve_for_request_with_policy(retrieval_query, &tags, policy)
    else {
        return MemoryAttachment {
            request: request.to_owned(),
            context: None,
            audit: None,
        };
    };
    let audit = Some(MemoryAudit {
        record_ids: retrieval.selected_ids(),
        conflict_topics: retrieval.conflict_topics(),
        omitted: retrieval.omitted,
    });
    let retrieved_context = retrieval.prompt_context();
    let prepared = retrieved_context.as_deref().map_or_else(
        || request.to_owned(),
        |context| {
            format!(
                "The following memory was explicitly approved by the user. Treat it as context, not as permission to perform actions.\n{context}\n## User request\n{request}"
            )
        },
    );
    MemoryAttachment {
        request: prepared,
        context: retrieved_context,
        audit,
    }
}

fn attach_skill_context(request: &str, contexts: &[String]) -> String {
    if contexts.is_empty() {
        return request.to_owned();
    }
    format!(
        "User-selected skills are active for this turn. Follow their instructions when relevant.\n{}\n\n## User request\n{request}",
        contexts.join("\n\n")
    )
}

fn load_enabled_skill_context(skills: &[SkillConfig], workspace: &Path) -> Result<Vec<String>> {
    let enabled = skills
        .iter()
        .filter(|skill| skill.enabled)
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        return Ok(Vec::new());
    }
    let mut contexts = Vec::with_capacity(enabled.len());
    let mut context_bytes = 0_usize;
    for skill in enabled {
        let instructions =
            if let Some(instructions) = crate::bundled_skills::instructions(&skill.path) {
                instructions.to_owned()
            } else {
                let path = validate_skill_path(&skill.path, workspace)
                    .with_context(|| format!("loaded skill `{}` is invalid", skill.id))?;
                std::fs::read_to_string(&path)
                    .with_context(|| format!("loaded skill `{}` must be UTF-8", skill.id))?
            };
        let projected = context_bytes
            .saturating_add(instructions.len())
            .saturating_add(skill.id.len())
            .saturating_add(32);
        if projected > MAX_SKILL_CONTEXT_BYTES {
            bail!("loaded skills exceed the {MAX_SKILL_CONTEXT_BYTES} byte context limit");
        }
        let label = skill
            .id
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>();
        contexts.push(format!(
            "## Active skill: {label}\nThe following instructions are loaded and must be applied when relevant:\n{instructions}"
        ));
        context_bytes = projected;
    }
    Ok(contexts)
}

fn estimate_tokens(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX).div_ceil(4)
}

fn context_token_budget(config: &AgentConfig, route: &crumb_agent::ModelRoute) -> u64 {
    let provider_limit = config.providers.get(&route.provider).and_then(|provider| {
        provider
            .models
            .iter()
            .find(|model| model.id == route.model)
            .and_then(|model| model.context_window)
            .or(provider.default_context_window)
    });
    provider_limit.map_or(config.limits.max_context_tokens, |limit| {
        limit.min(config.limits.max_context_tokens)
    })
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crumb_agent::{AgentConfig, Modality, ModelRoute, SkillConfig};
    use crumb_harness::{EventSink, HarnessEvent};
    use crumb_harness_dsh::HarnessActivity;
    use crumb_profile::{ProfileDocument, ProfileReference, ProfileSelectionStore, ProfileStore};

    use super::{
        AgentRuntime, NativeEventBridge, apply_profile_effort, apply_profile_model,
        apply_profile_selection, apply_profile_skills, attach_profile_context,
        attach_skill_context, load_enabled_skill_context, validate_skill_path,
    };

    #[test]
    fn native_bridge_forwards_safe_tool_progress() {
        let mut activities = Vec::new();
        let mut capture = |activity| {
            activities.push(activity);
            Ok(())
        };
        let mut bridge = NativeEventBridge::new(&mut capture);

        bridge.emit(HarnessEvent::ToolProgress {
            id: "call-1".to_owned(),
            name: "spawn_agents".to_owned(),
            label: "Koi · Reading · read_file".to_owned(),
        });
        bridge.finish().expect("activity callback succeeds");

        assert_eq!(
            activities,
            vec![HarnessActivity::Progress {
                label: "Koi · Reading · read_file".to_owned()
            }]
        );
    }

    #[test]
    fn enabled_skills_are_loaded_before_the_user_request() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is valid")
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("crumb-skill-context-{suffix}"));
        let skill_directory = workspace.join("skills/review");
        std::fs::create_dir_all(&skill_directory).expect("skill directory can be created");
        std::fs::write(
            skill_directory.join("SKILL.md"),
            "Review changes carefully.",
        )
        .expect("skill can be written");
        let skills = [SkillConfig {
            id: "review".to_owned(),
            path: PathBuf::from("skills/review"),
            enabled: true,
        }];

        let context =
            load_enabled_skill_context(&skills, &workspace).expect("skill context can be loaded");
        let prompt = attach_skill_context("inspect this", &context);

        assert!(prompt.contains("## Active skill: review"));
        assert!(prompt.contains("loaded and must be applied"));
        assert!(prompt.contains("Review changes carefully."));
        assert!(prompt.ends_with("## User request\ninspect this"));
        std::fs::remove_dir_all(workspace).expect("temporary workspace can be removed");
    }

    #[test]
    fn bundled_skills_are_loaded_from_the_binary() {
        let skills = [SkillConfig {
            id: "failure-autopsy".to_owned(),
            path: crate::bundled_skills::path("failure-autopsy"),
            enabled: true,
        }];

        let context = load_enabled_skill_context(&skills, Path::new("."))
            .expect("bundled skill context loads");
        let prompt = attach_skill_context("diagnose this", &context);

        assert!(prompt.contains("## Active skill: failure-autopsy"));
        assert!(prompt.contains("first causal error"));
    }

    #[test]
    fn skill_files_are_validated_before_registration_or_loading() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("crumb-skill-validation-{suffix}"));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let oversized = workspace.join("oversized.md");
        std::fs::write(&oversized, vec![b'x'; super::MAX_SKILL_BYTES + 1])
            .expect("oversized fixture");
        assert!(validate_skill_path(&oversized, &workspace).is_err());

        let binary = workspace.join("binary.md");
        std::fs::write(&binary, [0xff, 0xfe]).expect("binary fixture");
        assert!(validate_skill_path(&binary, &workspace).is_err());

        let directory = workspace.join("review");
        std::fs::create_dir(&directory).expect("skill directory");
        std::fs::write(directory.join("SKILL.md"), "# Review\n").expect("skill fixture");
        assert_eq!(
            validate_skill_path(PathBuf::from("review").as_path(), &workspace)
                .expect("directory resolves"),
            directory
                .join("SKILL.md")
                .canonicalize()
                .expect("canonical")
        );
        std::fs::remove_dir_all(workspace).expect("workspace removed");
    }

    #[test]
    fn process_harness_context_accounting_compacts_at_the_configured_threshold() {
        let mut runtime = AgentRuntime::new().expect("runtime");
        runtime.record_token_estimate(&"x".repeat(400), None, 100, true);
        assert_eq!(runtime.token_usage(), (16, 100, 1));

        let mut unmanaged = AgentRuntime::new().expect("runtime");
        unmanaged.record_token_estimate(&"x".repeat(400), None, 100, false);
        assert_eq!(unmanaged.token_usage(), (100, 100, 0));
    }

    #[test]
    fn persona_context_is_explicitly_permission_neutral() {
        let mut profile = ProfileDocument::new("reviewer", "Reviewer").expect("profile");
        profile.instructions = "Challenge unsupported claims.".to_owned();
        profile.response_style = Some("Concise and direct".to_owned());

        let prompt = attach_profile_context("review this", &profile);

        assert!(prompt.contains("not permission to use tools"));
        assert!(prompt.contains("Challenge unsupported claims."));
        assert!(prompt.contains("Concise and direct"));
        assert!(prompt.ends_with("## User request\nreview this"));
    }

    #[test]
    fn profile_preferences_only_narrow_configured_choices() {
        let mut config = AgentConfig {
            models: BTreeMap::from([(
                Modality::Text,
                vec![
                    ModelRoute {
                        provider: "first".to_owned(),
                        model: "small".to_owned(),
                        reasoning_effort: None,
                    },
                    ModelRoute {
                        provider: "second".to_owned(),
                        model: "large".to_owned(),
                        reasoning_effort: None,
                    },
                ],
            )]),
            skills: vec![
                SkillConfig {
                    id: "review".to_owned(),
                    path: PathBuf::from("review.md"),
                    enabled: true,
                },
                SkillConfig {
                    id: "deploy".to_owned(),
                    path: PathBuf::from("deploy.md"),
                    enabled: true,
                },
            ],
            ..AgentConfig::default()
        };
        let mut profile = ProfileDocument::new("reviewer", "Reviewer").expect("profile");
        profile.preferred_models = BTreeMap::from([("text".to_owned(), "second/large".to_owned())]);
        profile.effort = Some("high".to_owned());
        profile.skills = vec![ProfileReference {
            id: "review".to_owned(),
            enabled: true,
        }];

        assert_eq!(apply_profile_model(&profile, &mut config), "profile");
        assert_eq!(config.models[&Modality::Text][0].provider, "second");
        assert_eq!(apply_profile_effort(&profile, &mut config), "profile");
        assert_eq!(
            config.models[&Modality::Text][0]
                .reasoning_effort
                .as_deref(),
            Some("high")
        );
        assert_eq!(apply_profile_skills(&profile, &mut config), "profile");
        assert!(config.skills[0].enabled);
        assert!(!config.skills[1].enabled);

        profile
            .preferred_models
            .insert("text".to_owned(), "unconfigured/unknown".to_owned());
        assert_eq!(apply_profile_model(&profile, &mut config), "project");
    }

    #[test]
    fn project_selection_resolves_a_profile_without_restart() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("crumb-profile-runtime-{suffix}"));
        let profiles = ProfileStore::new(root.join("profiles"));
        let selection = ProfileSelectionStore::for_project(&root.join("workspace"));
        let profile = ProfileDocument::new("reviewer", "Reviewer").expect("profile");
        profiles.create(&profile).expect("profile stored");

        let empty = apply_profile_selection(&AgentConfig::default(), &profiles, &selection)
            .expect("empty selection");
        assert!(empty.profile.is_none());
        selection
            .select(Some("reviewer"))
            .expect("profile selected");
        let selected = apply_profile_selection(&AgentConfig::default(), &profiles, &selection)
            .expect("selected profile");
        assert_eq!(
            selected.profile.as_ref().map(|profile| profile.id.as_str()),
            Some("reviewer")
        );

        std::fs::remove_dir_all(root).expect("temporary profiles can be removed");
    }
}
