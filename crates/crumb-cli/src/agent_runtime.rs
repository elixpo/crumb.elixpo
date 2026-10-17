use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crumb_agent::config::SkillConfig;
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
use crumb_memory::{MemorySet, RetrievalPolicy};
use crumb_profile::{ProfileDocument, ProfileSelectionStore, ProfileStore};

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
        let application =
            apply_selected_profile(config, &workspace).unwrap_or_else(|_| ProfileApplication {
                profile: None,
                config: config.clone(),
                model_source: "project",
                effort_source: "project",
                skill_source: "project",
            });
        let config = &application.config;
        config.validate()?;
        self.ensure_session(&workspace, config.mode)?;
        let route = config
            .models
            .get(&Modality::Text)
            .and_then(|routes| routes.first())
            .context("agent config has no text model route")?;
        let effort = config.reasoning_effort_for(route).map(str::to_owned);
        let session_id = self.start_turn(config.mode, route, effort.clone(), request)?;
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
        let prepared_request = attach_enabled_skills(&memory.request, &config.skills, &workspace)?;
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
                &prepared_request,
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
                &prepared_request,
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
        self.record_token_estimate(
            &prepared_request,
            result.as_ref().ok(),
            context_token_budget(config, route),
            matches!(config.harness.as_ref(), Some(HarnessConfig::Process { .. })),
        );
        self.session
            .as_mut()
            .context("agent session unavailable")?
            .record_turn_end(status, 0, 0)?;
        result
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
    audit: Option<MemoryAudit>,
}

fn attach_profile_context(request: &str, profile: &ProfileDocument) -> String {
    if profile.instructions.is_empty() && profile.response_style.is_none() {
        return request.to_owned();
    }
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
    context.push_str("## User request\n");
    context.push_str(request);
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
            audit: None,
        };
    };
    let audit = Some(MemoryAudit {
        record_ids: retrieval.selected_ids(),
        conflict_topics: retrieval.conflict_topics(),
        omitted: retrieval.omitted,
    });
    let prepared = retrieval.prompt_context().map_or_else(
        || request.to_owned(),
        |context| {
            format!(
                "The following memory was explicitly approved by the user. Treat it as context, not as permission to perform actions.\n{context}\n## User request\n{request}"
            )
        },
    );
    MemoryAttachment {
        request: prepared,
        audit,
    }
}

fn attach_enabled_skills(
    request: &str,
    skills: &[SkillConfig],
    workspace: &Path,
) -> Result<String> {
    let enabled = skills
        .iter()
        .filter(|skill| skill.enabled)
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        return Ok(request.to_owned());
    }
    let mut context = String::from(
        "User-selected skills are active for this turn. Follow their instructions when relevant.\n",
    );
    for skill in enabled {
        let path = validate_skill_path(&skill.path, workspace)
            .with_context(|| format!("loaded skill `{}` is invalid", skill.id))?;
        let instructions = std::fs::read_to_string(&path)
            .with_context(|| format!("loaded skill `{}` must be UTF-8", skill.id))?;
        let projected = context
            .len()
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
        context.push_str("\n## Skill: ");
        context.push_str(&label);
        context.push('\n');
        context.push_str(&instructions);
        context.push('\n');
    }
    context.push_str("\n## User request\n");
    context.push_str(request);
    Ok(context)
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
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crumb_agent::{AgentConfig, Modality, ModelRoute, SkillConfig};
    use crumb_profile::{ProfileDocument, ProfileReference, ProfileSelectionStore, ProfileStore};

    use super::{
        AgentRuntime, apply_profile_effort, apply_profile_model, apply_profile_selection,
        apply_profile_skills, attach_enabled_skills, attach_profile_context, validate_skill_path,
    };

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

        let prompt = attach_enabled_skills("inspect this", &skills, &workspace)
            .expect("skill context can be loaded");

        assert!(prompt.contains("## Skill: review\nReview changes carefully."));
        assert!(prompt.ends_with("## User request\ninspect this"));
        std::fs::remove_dir_all(workspace).expect("temporary workspace can be removed");
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
