//! Bounded read-only parallel analysis exposed as one native tool.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crumb_agent::{
    AgentMode, AgentSession, CancellationToken, DenyAllApprovals, RiskClass, SessionId,
    SessionJournal, ToolDescriptor, ToolHandler, ToolHost, ToolOutput, ToolTransport,
};
use crumb_llm::LlmProvider;
use serde_json::{Value, json};

use crate::{EventSink, HarnessEvent, HarnessLimits, HarnessTurn, NativeHarness};

const SPAWN_AGENTS: &str = "spawn_agents";
const NAMES: [&str; 8] = ["Koi", "Nyx", "Rune", "Echo", "Lux", "Mica", "Rook", "Zed"];
static NEXT_TEAM: AtomicU64 = AtomicU64::new(1);

/// Hard ceilings for one parallel-analysis request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParallelAgentLimits {
    pub max_agents: NonZeroUsize,
    pub max_task_bytes: NonZeroUsize,
    pub max_result_bytes: NonZeroUsize,
}

impl Default for ParallelAgentLimits {
    fn default() -> Self {
        Self {
            max_agents: NonZeroUsize::new(4).expect("constant is non-zero"),
            max_task_bytes: NonZeroUsize::new(8 * 1024).expect("constant is non-zero"),
            max_result_bytes: NonZeroUsize::new(16 * 1024).expect("constant is non-zero"),
        }
    }
}

/// Provider and session settings inherited by read-only subagents.
#[derive(Clone)]
pub struct ParallelAgentConfig {
    pub workspace: PathBuf,
    pub journal_root: PathBuf,
    pub model: String,
    pub max_output_tokens: Option<u32>,
    pub harness_limits: HarnessLimits,
    pub team_limits: ParallelAgentLimits,
}

/// Adds a non-recursive `spawn_agents` tool backed by independent native Harness turns.
///
/// The supplied host is cloned before registration and narrowed to read-only tools,
/// so children cannot mutate the workspace, use network tools, or spawn more agents.
///
/// # Errors
///
/// Returns an error when paths, limits, or model configuration are invalid.
pub fn register_parallel_agent_tool(
    host: &mut ToolHost,
    provider: Arc<dyn LlmProvider>,
    config: ParallelAgentConfig,
) -> Result<()> {
    if config.model.trim().is_empty() {
        bail!("parallel agent model must not be empty");
    }
    let workspace = std::fs::canonicalize(&config.workspace).with_context(|| {
        format!(
            "failed to resolve parallel agent workspace `{}`",
            config.workspace.display()
        )
    })?;
    if !workspace.is_dir() {
        bail!("parallel agent workspace must be a directory");
    }
    std::fs::create_dir_all(&config.journal_root).with_context(|| {
        format!(
            "failed to create parallel agent journal root `{}`",
            config.journal_root.display()
        )
    })?;
    let child_tools = host.read_only();
    host.register(
        descriptor(config.team_limits),
        Arc::new(ParallelAgentTool {
            provider,
            child_tools,
            config: ParallelAgentConfig {
                workspace,
                ..config
            },
        }),
    )
}

struct ParallelAgentTool {
    provider: Arc<dyn LlmProvider>,
    child_tools: ToolHost,
    config: ParallelAgentConfig,
}

impl ToolHandler for ParallelAgentTool {
    fn call(&self, arguments: &Value, cancellation: &CancellationToken) -> Result<ToolOutput> {
        match run_team(self, arguments, cancellation) {
            Ok(output) => Ok(output),
            Err(error) if cancellation.is_cancelled() => Err(error),
            Err(error) => Ok(ToolOutput::error(error.to_string())),
        }
    }
}

fn run_team(
    tool: &ParallelAgentTool,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<ToolOutput> {
    ensure_active(cancellation)?;
    let tasks = parse_tasks(arguments, tool.config.team_limits)?;
    let team = team_id();
    let mut workers = Vec::with_capacity(tasks.len());
    for (index, task) in tasks.into_iter().enumerate() {
        let name = NAMES[index].to_owned();
        let provider = Arc::clone(&tool.provider);
        let child_tools = tool.child_tools.clone();
        let config = tool.config.clone();
        let worker = WorkerSpec {
            team: team.clone(),
            index,
            name,
            task,
            provider,
            tools: child_tools,
            config,
            cancellation: cancellation.clone(),
        };
        workers.push(
            thread::Builder::new()
                .name(format!("crumb-{}", worker.name.to_ascii_lowercase()))
                .spawn(move || run_worker(worker))
                .context("failed to start parallel agent")?,
        );
    }

    let mut findings = Vec::with_capacity(workers.len());
    for worker in workers {
        findings.push(
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("parallel agent worker panicked"))??,
        );
    }
    ensure_active(cancellation)?;
    let text = findings
        .iter()
        .map(|finding| format!("{}:\n{}", finding.name, finding.text))
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = bounded_text(text, tool.config.harness_limits.max_output_bytes.get());
    Ok(ToolOutput {
        text,
        structured: Some(json!({
            "agents": findings.iter().map(|finding| json!({
                "name": finding.name,
                "session_id": finding.session_id,
                "finding": finding.text,
            })).collect::<Vec<_>>()
        })),
        is_error: findings.iter().any(|finding| finding.failed),
    })
}

struct Finding {
    name: String,
    session_id: String,
    text: String,
    failed: bool,
}

struct WorkerSpec {
    team: String,
    index: usize,
    name: String,
    task: String,
    provider: Arc<dyn LlmProvider>,
    tools: ToolHost,
    config: ParallelAgentConfig,
    cancellation: CancellationToken,
}

fn run_worker(worker: WorkerSpec) -> Result<Finding> {
    let WorkerSpec {
        team,
        index,
        name,
        task,
        provider,
        tools,
        config,
        cancellation,
    } = worker;
    let session_id = SessionId::new(format!(
        "agent-{team}-{}-{}",
        index + 1,
        name.to_ascii_lowercase()
    ))?;
    let journal = SessionJournal::open(&config.journal_root, &session_id)?;
    let mut session = AgentSession::start(
        session_id.clone(),
        AgentMode::Auto,
        config.workspace,
        journal,
    )?;
    let harness = NativeHarness::new(
        provider,
        tools,
        Arc::new(DenyAllApprovals),
        config.harness_limits,
    );
    let mut conversation = harness.conversation();
    let mut events = NoopEvents;
    let turn = HarnessTurn {
        model: config.model,
        user_message: format!(
            "You are {name}, a read-only Crumb analysis agent. Investigate independently, cite concrete paths or evidence, and return concise findings for the parent agent. Do not request mutations.\n\nAssigned task:\n{task}"
        ),
        persona: None,
        trusted_context: Vec::new(),
        max_output_tokens: config.max_output_tokens,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start parallel agent runtime")?;
    let outcome = runtime.block_on(harness.run_turn(
        &mut session,
        &mut conversation,
        turn,
        &cancellation,
        &mut events,
    ));
    let (text, failed) = match outcome {
        Ok(outcome) => (
            bounded_text(outcome.text, config.team_limits.max_result_bytes.get()),
            false,
        ),
        Err(error) => (format!("analysis failed: {error}"), true),
    };
    Ok(Finding {
        name,
        session_id: session_id.as_str().to_owned(),
        text,
        failed,
    })
}

fn team_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let sequence = NEXT_TEAM.fetch_add(1, Ordering::Relaxed);
    format!("{}-{timestamp}-{sequence}", std::process::id())
}

fn parse_tasks(arguments: &Value, limits: ParallelAgentLimits) -> Result<Vec<String>> {
    let tasks = arguments
        .get("tasks")
        .and_then(Value::as_array)
        .context("tasks must be an array")?;
    if tasks.len() < 2 {
        bail!("parallel analysis requires at least two tasks");
    }
    if tasks.len() > limits.max_agents.get() {
        bail!("parallel analysis exceeds the configured agent limit");
    }
    tasks
        .iter()
        .map(|task| {
            let task = task.as_str().context("each task must be a string")?.trim();
            if task.is_empty() || task.len() > limits.max_task_bytes.get() {
                bail!("parallel agent task is empty or exceeds the byte limit");
            }
            Ok(task.to_owned())
        })
        .collect()
}

fn bounded_text(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut boundary = limit;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text
}

fn ensure_active(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        bail!("tool call cancelled");
    }
    Ok(())
}

struct NoopEvents;

impl EventSink for NoopEvents {
    fn emit(&mut self, _event: HarnessEvent) {}
}

fn descriptor(limits: ParallelAgentLimits) -> ToolDescriptor {
    ToolDescriptor {
        name: SPAWN_AGENTS.to_owned(),
        description: "Run two or more independent read-only analysis agents in parallel and return their bounded findings to the parent agent.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": limits.max_agents.get(),
                    "items": {"type": "string", "minLength": 1, "maxLength": limits.max_task_bytes.get()}
                }
            },
            "required": ["tasks"],
            "additionalProperties": false
        }),
        risk: RiskClass::NetworkAccess,
        transport: ToolTransport::Native,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Duration;

    use crumb_agent::{AgentMode, CancellationToken, ConfiguredApprovals, ToolHost};
    use crumb_llm::{ChatEvent, EmbeddingResponse, FinishReason, MockProvider, TokenUsage};
    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        NAMES, ParallelAgentConfig, ParallelAgentLimits, parse_tasks, register_parallel_agent_tool,
    };
    use crate::HarnessLimits;

    #[test]
    fn short_names_are_unique_and_terminal_friendly() {
        let unique = NAMES.into_iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), NAMES.len());
        assert!(NAMES.iter().all(|name| name.len() <= 4));
    }

    #[test]
    fn task_fanout_is_bounded() {
        let limits = ParallelAgentLimits {
            max_agents: NonZeroUsize::new(2).unwrap(),
            max_task_bytes: NonZeroUsize::new(8).unwrap(),
            max_result_bytes: NonZeroUsize::new(32).unwrap(),
        };
        assert!(parse_tasks(&json!({"tasks":["one", "two"]}), limits).is_ok());
        assert!(parse_tasks(&json!({"tasks":["one"]}), limits).is_err());
        assert!(parse_tasks(&json!({"tasks":["one", "two", "three"]}), limits).is_err());
        assert!(parse_tasks(&json!({"tasks":["too long!", "two"]}), limits).is_err());
    }

    #[test]
    fn registered_tool_runs_named_workers_and_journals_each_one() {
        let workspace = TempDir::new().expect("workspace");
        let journal_root = workspace.path().join("sessions/agents");
        let provider = MockProvider::new(
            Vec::new(),
            vec![
                ChatEvent::TextDelta("evidence".to_owned()),
                ChatEvent::Finished(FinishReason::Stop),
            ],
            EmbeddingResponse {
                vectors: Vec::new(),
                usage: TokenUsage::default(),
            },
        );
        let mut host = ToolHost::default();
        register_parallel_agent_tool(
            &mut host,
            Arc::new(provider),
            ParallelAgentConfig {
                workspace: workspace.path().to_path_buf(),
                journal_root: journal_root.clone(),
                model: "fixture".to_owned(),
                max_output_tokens: Some(128),
                harness_limits: HarnessLimits {
                    turn_timeout: Duration::from_secs(2),
                    ..HarnessLimits::default()
                },
                team_limits: ParallelAgentLimits::default(),
            },
        )
        .expect("parallel tool registers");
        let approvals = ConfiguredApprovals::new(BTreeSet::from(["spawn_agents".to_owned()]));

        let output = host
            .call(
                "spawn_agents",
                &json!({"tasks":["inspect one", "inspect two"]}),
                AgentMode::Auto,
                &approvals,
                &CancellationToken::default(),
            )
            .expect("parallel analysis succeeds");

        assert!(!output.is_error);
        assert!(output.text.contains("Koi:\nevidence"));
        assert!(output.text.contains("Nyx:\nevidence"));
        assert_eq!(
            std::fs::read_dir(journal_root)
                .expect("journals exist")
                .count(),
            2
        );
    }
}
