//! Bounded, non-interactive agent harness for embedded Crumb consumers.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use crumb_agent::{
    AgentSession, ApprovalBroker, CancellationToken, RiskClass, ToolCallErrorKind, ToolHost,
    TurnStatus,
};
use crumb_llm::{
    ChatEvent, ChatMessage, ChatRequest, ChatRole, FinishReason, ImageInput, LlmProvider,
    ProviderError, TokenUsage, ToolCall, ToolChoice, ToolDefinition,
};
use tokio::time::Instant;

mod parallel;

pub use parallel::{ParallelAgentConfig, ParallelAgentLimits, register_parallel_agent_tool};

/// Hard limits applied to one user turn and its model/tool rounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarnessLimits {
    pub max_model_rounds: NonZeroUsize,
    pub max_tool_calls: NonZeroUsize,
    pub max_output_bytes: NonZeroUsize,
    pub max_history_turns: NonZeroUsize,
    pub turn_timeout: Duration,
}

impl Default for HarnessLimits {
    fn default() -> Self {
        Self {
            max_model_rounds: NonZeroUsize::new(8).expect("constant is non-zero"),
            max_tool_calls: NonZeroUsize::new(16).expect("constant is non-zero"),
            max_output_bytes: NonZeroUsize::new(64 * 1024).expect("constant is non-zero"),
            max_history_turns: NonZeroUsize::new(12).expect("constant is non-zero"),
            turn_timeout: Duration::from_secs(90),
        }
    }
}

/// One user turn with transient persona and trusted context injection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessTurn {
    pub model: String,
    pub user_message: String,
    pub images: Vec<ImageInput>,
    pub persona: Option<String>,
    pub static_context: Vec<String>,
    pub trusted_context: Vec<String>,
    pub max_output_tokens: Option<u32>,
}

impl HarnessTurn {
    fn user_chat_message(&self) -> ChatMessage {
        ChatMessage::user_with_images(self.user_message.clone(), self.images.clone())
    }
}

/// Transient event vocabulary emitted to a trusted caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessEvent {
    TurnStarted,
    TextDelta(String),
    ToolRequested {
        id: String,
        name: String,
        detail: Option<String>,
    },
    ToolProgress {
        id: String,
        name: String,
        label: String,
    },
    ToolFinished {
        id: String,
        name: String,
        success: bool,
    },
    ToolOutput {
        id: String,
        name: String,
        content: String,
    },
    FileChanged {
        path: String,
        additions: u64,
        deletions: u64,
    },
    Usage(TokenUsage),
    TurnFinished,
}

/// Receives transient response data. Implementations decide how to render it.
pub trait EventSink {
    fn emit(&mut self, event: HarnessEvent);
}

impl EventSink for Vec<HarnessEvent> {
    fn emit(&mut self, event: HarnessEvent) {
        self.push(event);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConversationTurn {
    user: String,
    assistant: String,
}

const MAX_HISTORY_BYTES: usize = 24 * 1024;
const MAX_ACTION_FACT_BYTES: usize = 8 * 1024;
const MAX_ACTION_FACTS: usize = 64;
const TRUNCATION_MARKER: &str = "\n[... earlier turn content compacted ...]\n";

/// In-memory bounded conversation state. It is never persisted by this crate.
#[derive(Clone, Debug)]
pub struct Conversation {
    max_turns: NonZeroUsize,
    turns: VecDeque<ConversationTurn>,
    action_facts: VecDeque<String>,
}

impl Conversation {
    #[must_use]
    pub fn new(max_turns: NonZeroUsize) -> Self {
        Self {
            max_turns,
            turns: VecDeque::with_capacity(max_turns.get()),
            action_facts: VecDeque::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    fn messages(&self) -> Vec<ChatMessage> {
        if self.turns.is_empty() && self.action_facts.is_empty() {
            return Vec::new();
        }

        let history_budget = MAX_HISTORY_BYTES.saturating_sub(self.action_fact_bytes());
        let message_budget = history_budget
            .checked_div(self.turns.len().saturating_mul(2))
            .unwrap_or_default();
        let mut messages = Vec::with_capacity(self.turns.len().saturating_mul(2) + 1);
        for turn in &self.turns {
            messages.push(ChatMessage::text(
                ChatRole::User,
                compact_history_message(&turn.user, message_budget),
            ));
            messages.push(ChatMessage::text(
                ChatRole::Assistant,
                compact_history_message(&turn.assistant, message_budget),
            ));
        }
        if !self.action_facts.is_empty() {
            messages.push(ChatMessage::text(
                ChatRole::System,
                format!(
                    "Authoritative successful actions from earlier turns. Reuse these exact path anchors for follow-up references; do not relocate them relative to the current workspace:\n- {}",
                    self.action_facts
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join("\n- ")
                ),
            ));
        }
        messages
    }

    fn commit(&mut self, user: String, assistant: String, action_facts: Vec<String>) {
        if self.turns.len() == self.max_turns.get() {
            self.turns.pop_front();
        }
        self.turns.push_back(ConversationTurn { user, assistant });
        for fact in action_facts {
            self.retain_action_fact(fact);
        }
    }

    fn retain_action_fact(&mut self, fact: String) {
        if fact.is_empty() || self.action_facts.contains(&fact) {
            return;
        }
        while !self.action_facts.is_empty()
            && (self.action_facts.len() == MAX_ACTION_FACTS
                || self.action_fact_bytes().saturating_add(fact.len()) > MAX_ACTION_FACT_BYTES)
        {
            self.action_facts.pop_front();
        }
        if fact.len() <= MAX_ACTION_FACT_BYTES {
            self.action_facts.push_back(fact);
        }
    }

    fn action_fact_bytes(&self) -> usize {
        self.action_facts.iter().map(String::len).sum()
    }
}

fn compact_history_message(message: &str, max_bytes: usize) -> String {
    if message.len() <= max_bytes {
        return message.to_owned();
    }
    if max_bytes <= TRUNCATION_MARKER.len() {
        return String::new();
    }
    let content_budget = max_bytes - TRUNCATION_MARKER.len();
    let head_budget = content_budget.div_ceil(2);
    let tail_budget = content_budget / 2;
    let head_end = floor_char_boundary(message, head_budget);
    let tail_start = ceil_char_boundary(message, message.len().saturating_sub(tail_budget));
    format!(
        "{}{}{}",
        &message[..head_end],
        TRUNCATION_MARKER,
        &message[tail_start..]
    )
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Successful turn result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessOutcome {
    pub text: String,
    pub usage: TokenUsage,
    /// Tokens occupying the model context at the end of the turn. Unlike
    /// `usage`, this is not summed across tool-call rounds.
    pub context_tokens: u64,
    pub model_rounds: usize,
    pub tool_calls: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarnessErrorKind {
    InvalidRequest,
    Provider,
    Cancelled,
    DeadlineExceeded,
    OutputLimit,
    ModelRoundLimit,
    ToolCallLimit,
    Persistence,
}

/// Redacted harness failure safe to expose to an embedded caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessError {
    pub kind: HarnessErrorKind,
    message: String,
}

impl HarnessError {
    fn new(kind: HarnessErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HarnessError {}

#[derive(Default)]
struct TurnMetrics {
    model_rounds: usize,
    tool_calls: usize,
    action_facts: Vec<String>,
}

impl TurnMetrics {
    fn record_successful_action(&mut self, call: &ToolCall) {
        if let Some(fact) = safe_successful_action_fact(call)
            && !self.action_facts.contains(&fact)
        {
            self.action_facts.push(fact);
        }
    }
}

struct ModelRound {
    text: String,
    text_chunks: Vec<String>,
    calls: Vec<ToolCall>,
    finish: Option<FinishReason>,
    usage: TokenUsage,
}

struct ExecutedTool {
    content: String,
    structured: Option<serde_json::Value>,
    success: bool,
}

/// Provider-neutral harness with Rust-owned tools and approval policy.
#[derive(Clone)]
pub struct NativeHarness {
    provider: Arc<dyn LlmProvider>,
    tools: ToolHost,
    approvals: Arc<dyn ApprovalBroker>,
    limits: HarnessLimits,
}

impl NativeHarness {
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: ToolHost,
        approvals: Arc<dyn ApprovalBroker>,
        limits: HarnessLimits,
    ) -> Self {
        Self {
            provider,
            tools,
            approvals,
            limits,
        }
    }

    #[must_use]
    pub fn conversation(&self) -> Conversation {
        Conversation::new(self.limits.max_history_turns)
    }

    /// Runs one bounded user turn and streams transient events to `sink`.
    ///
    /// Raw prompts, model text, tool arguments, and tool output are never
    /// written to the session journal.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for invalid input, provider failure,
    /// cancellation, deadline, a configured limit, or journal I/O.
    pub async fn run_turn(
        &self,
        session: &mut AgentSession,
        conversation: &mut Conversation,
        turn: HarnessTurn,
        cancellation: &CancellationToken,
        sink: &mut dyn EventSink,
    ) -> Result<HarnessOutcome, HarnessError> {
        if turn.model.trim().is_empty() || turn.user_message.trim().is_empty() {
            return Err(HarnessError::new(
                HarnessErrorKind::InvalidRequest,
                "model and user message must not be empty",
            ));
        }
        if self.limits.turn_timeout.is_zero() {
            return Err(HarnessError::new(
                HarnessErrorKind::InvalidRequest,
                "turn timeout must not be zero",
            ));
        }

        session
            .record_turn_start(&turn.user_message)
            .map_err(persistence_error)?;
        sink.emit(HarnessEvent::TurnStarted);
        let mut metrics = TurnMetrics::default();
        let result = self
            .run_inner(
                session,
                conversation,
                turn,
                cancellation,
                sink,
                &mut metrics,
            )
            .await;
        let status = match result.as_ref().map_err(|error| error.kind) {
            Ok(_) => TurnStatus::Complete,
            Err(HarnessErrorKind::Cancelled) => TurnStatus::Cancelled,
            Err(
                HarnessErrorKind::DeadlineExceeded
                | HarnessErrorKind::OutputLimit
                | HarnessErrorKind::ModelRoundLimit
                | HarnessErrorKind::ToolCallLimit,
            ) => TurnStatus::LimitReached,
            Err(_) => TurnStatus::Failed,
        };
        session
            .record_turn_end(
                status,
                u32::try_from(metrics.model_rounds).unwrap_or(u32::MAX),
                u32::try_from(metrics.tool_calls).unwrap_or(u32::MAX),
            )
            .map_err(persistence_error)?;
        if result.is_ok() {
            sink.emit(HarnessEvent::TurnFinished);
        }
        result
    }

    async fn run_inner(
        &self,
        session: &mut AgentSession,
        conversation: &mut Conversation,
        turn: HarnessTurn,
        cancellation: &CancellationToken,
        sink: &mut dyn EventSink,
        metrics: &mut TurnMetrics,
    ) -> Result<HarnessOutcome, HarnessError> {
        let deadline = Instant::now() + self.limits.turn_timeout;
        let mut messages = system_messages(&turn);
        messages.extend(conversation.messages());
        messages.push(turn.user_chat_message());
        let definitions = tool_definitions_for_turn(&self.tools, &turn.user_message);
        let mut usage = TokenUsage::default();
        let mut output_bytes = 0_usize;
        let mut retries = RequiredToolRetries::new(&definitions);
        let mut require_tool = initial_tool_requirement(&definitions, &turn.user_message);
        let (mut successful_tool_evidence, mut failed_tool_evidence) = (false, false);
        let (mut grounding_retry_used, mut failure_retry_used) = (false, false);
        let (mut completeness_retry_used, mut suppress_tools_once) = (false, false);

        loop {
            if metrics.model_rounds == self.limits.max_model_rounds.get() {
                return Err(HarnessError::new(
                    HarnessErrorKind::ModelRoundLimit,
                    "model round limit reached",
                ));
            }
            metrics.model_rounds += 1;
            let tool_choice = (require_tool, suppress_tools_once);
            let request = build_request(&turn, &messages, &definitions, tool_choice);
            let mut round = self
                .run_model_round(request, cancellation, deadline, sink, &mut output_bytes)
                .await?;
            require_tool = false;
            suppress_tools_once = false;
            record_usage(&mut usage, round.usage);
            if round.calls.is_empty() {
                match round.finish {
                    Some(FinishReason::Stop | FinishReason::Other(_)) => {
                        if let Some((next_required, next_suppressed)) = prepare_response_retry(
                            &mut messages,
                            &round.text,
                            &turn.user_message,
                            successful_tool_evidence,
                            failed_tool_evidence,
                            &mut grounding_retry_used,
                            &mut failure_retry_used,
                            &mut completeness_retry_used,
                            &mut suppress_tools_once,
                            &mut retries,
                        ) {
                            require_tool = next_required;
                            suppress_tools_once = next_suppressed;
                            continue;
                        }
                        return Ok(finish_turn(round, turn, conversation, sink, usage, metrics));
                    }
                    Some(FinishReason::Length) => {
                        return Err(HarnessError::new(
                            HarnessErrorKind::OutputLimit,
                            "model output token limit reached",
                        ));
                    }
                    Some(FinishReason::ToolCall) | None => {
                        return Err(HarnessError::new(
                            HarnessErrorKind::Provider,
                            "provider ended without a complete response",
                        ));
                    }
                }
            }

            for call in &mut round.calls {
                preserve_web_search_terms(call, &turn.user_message);
            }
            messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content: round.text,
                images: Vec::new(),
                tool_call_id: None,
                tool_calls: round.calls.clone(),
            });
            let mut failed_tools = Vec::new();
            let mut round_succeeded = false;
            for call in round.calls {
                if metrics.tool_calls == self.limits.max_tool_calls.get() {
                    return Err(HarnessError::new(
                        HarnessErrorKind::ToolCallLimit,
                        "tool call limit reached",
                    ));
                }
                metrics.tool_calls += 1;
                let result = self.execute_tool(session, &call, cancellation, deadline, sink)?;
                add_output_bytes(&mut output_bytes, result.content.len(), self.limits)?;
                emit_tool_output(sink, &call, &result);
                if result.success {
                    round_succeeded = true;
                    metrics.record_successful_action(&call);
                } else {
                    failed_tool_evidence = true;
                    failed_tools.push(call.name.clone());
                }
                messages.push(ChatMessage::tool_result(call.id, result.content));
            }
            successful_tool_evidence |=
                record_successful_tool_evidence(&mut messages, round_succeeded);
            suppress_tools_once = round_succeeded;
            if retries.prepare_failed_execution(&mut messages, &turn.user_message, &failed_tools) {
                require_tool = true;
            }
        }
    }

    async fn run_model_round(
        &self,
        request: ChatRequest,
        cancellation: &CancellationToken,
        deadline: Instant,
        sink: &mut dyn EventSink,
        output_bytes: &mut usize,
    ) -> Result<ModelRound, HarnessError> {
        let mut stream = controlled(self.provider.chat_stream(request), cancellation, deadline)
            .await?
            .map_err(provider_error)?;
        let mut round = ModelRound {
            text: String::new(),
            text_chunks: Vec::new(),
            calls: Vec::new(),
            finish: None,
            usage: TokenUsage::default(),
        };
        loop {
            let event = controlled(stream.next(), cancellation, deadline)
                .await?
                .map_err(provider_error)?;
            let Some(event) = event else { break };
            match event {
                ChatEvent::TextDelta(delta) => {
                    add_output_bytes(output_bytes, delta.len(), self.limits)?;
                    round.text.push_str(&delta);
                    round.text_chunks.push(delta);
                }
                ChatEvent::ToolCall(call) => round.calls.push(call),
                ChatEvent::Usage(usage) => {
                    round.usage.input_tokens =
                        round.usage.input_tokens.saturating_add(usage.input_tokens);
                    round.usage.cached_input_tokens = round
                        .usage
                        .cached_input_tokens
                        .saturating_add(usage.cached_input_tokens);
                    round.usage.output_tokens = round
                        .usage
                        .output_tokens
                        .saturating_add(usage.output_tokens);
                    sink.emit(HarnessEvent::Usage(usage));
                }
                ChatEvent::Finished(reason) => round.finish = Some(reason),
            }
        }
        Ok(round)
    }

    fn execute_tool(
        &self,
        session: &mut AgentSession,
        call: &ToolCall,
        cancellation: &CancellationToken,
        deadline: Instant,
        sink: &mut dyn EventSink,
    ) -> Result<ExecutedTool, HarnessError> {
        ensure_active(cancellation, deadline)?;
        let arguments = self
            .tools
            .prepare_arguments(&call.name, &call.arguments, cancellation)
            .map_err(|error| {
                HarnessError::new(HarnessErrorKind::InvalidRequest, error.to_string())
            })?;
        let risk = self
            .tools
            .descriptor(&call.name)
            .map_or(RiskClass::CredentialSensitive, |tool| tool.risk);
        let encoded = serde_json::to_vec(&arguments).map_err(|_| {
            HarnessError::new(
                HarnessErrorKind::InvalidRequest,
                "tool arguments could not be encoded",
            )
        })?;
        session
            .record_tool_request(call.name.clone(), risk, &encoded)
            .map_err(persistence_error)?;
        sink.emit(HarnessEvent::ToolRequested {
            id: call.id.clone(),
            name: call.name.clone(),
            detail: tool_request_detail(&call.name, &arguments),
        });
        let mut report_progress = |label: &str| {
            sink.emit(HarnessEvent::ToolProgress {
                id: call.id.clone(),
                name: call.name.clone(),
                label: label.to_owned(),
            });
        };
        let result = self.tools.call_with_progress(
            &call.name,
            &arguments,
            session.mode(),
            self.approvals.as_ref(),
            cancellation,
            &mut report_progress,
        );
        ensure_active(cancellation, deadline)?;
        let (content, structured, success) = match result {
            Ok(output) => (output.text, output.structured, !output.is_error),
            Err(error) if error.kind == ToolCallErrorKind::Cancelled => {
                return Err(HarnessError::new(
                    HarnessErrorKind::Cancelled,
                    "turn cancelled",
                ));
            }
            Err(error) => (error.to_string(), None, false),
        };
        session
            .record_tool_finish(call.name.clone(), success, content.len())
            .map_err(persistence_error)?;
        sink.emit(HarnessEvent::ToolFinished {
            id: call.id.clone(),
            name: call.name.clone(),
            success,
        });
        Ok(ExecutedTool {
            content,
            structured,
            success,
        })
    }
}

fn tool_definitions_for_turn(tools: &ToolHost, user_message: &str) -> Vec<ToolDefinition> {
    if is_casual_conversation(user_message) {
        return Vec::new();
    }
    let mut definitions = tools
        .tools()
        .map(|tool| ToolDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        })
        .collect::<Vec<_>>();
    if user_requires_host_shell(user_message)
        && definitions.iter().any(|tool| tool.name == "run_shell")
    {
        definitions.retain(|tool| tool.name == "run_shell");
    }
    definitions
}

fn is_casual_conversation(user_message: &str) -> bool {
    let normalized = user_message
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '\'' {
                character
            } else {
                ' '
            }
        })
        .collect::<String>();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty()
        || normalized.split_whitespace().count() > 16
        || user_requires_execution(&normalized)
    {
        return false;
    }
    let padded = format!(" {normalized} ");
    let has_work_request = [
        " search ",
        " weather ",
        " time ",
        " date ",
        " news ",
        " price ",
        " festival ",
        " online ",
        " website ",
        " url ",
        " latest ",
        " current ",
        " today ",
        " explain ",
        " write ",
        " create ",
        " change ",
        " fix ",
        " code ",
        " file ",
        " folder ",
        " repo ",
        " project ",
        " command ",
    ]
    .iter()
    .any(|term| padded.contains(term));
    if has_work_request || normalized.contains("http://") || normalized.contains("https://") {
        return false;
    }
    [
        "hello",
        "hi",
        "hey",
        "yo",
        "sup",
        "wassup",
        "what's up",
        "whats up",
        "what is up",
        "what's going on",
        "whats going on",
        "how are you",
        "good morning",
        "good afternoon",
        "good evening",
    ]
    .iter()
    .any(|greeting| {
        normalized == *greeting
            || padded.contains(&format!(" {greeting} "))
            || normalized.starts_with(&format!("{greeting} "))
    })
}

struct RequiredToolRetries {
    execution: RetryStatus,
    research: RetryStatus,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RetryStatus {
    Unavailable,
    Ready,
    Used,
}

impl RequiredToolRetries {
    fn new(definitions: &[ToolDefinition]) -> Self {
        Self {
            execution: retry_status(definitions, "run_shell"),
            research: retry_status(definitions, "web_search"),
        }
    }

    fn prepare(
        &mut self,
        messages: &mut Vec<ChatMessage>,
        user_message: &str,
        response: &str,
    ) -> bool {
        if self.execution == RetryStatus::Ready
            && prepare_execution_retry(messages, user_message, response)
        {
            self.execution = RetryStatus::Used;
            return true;
        }
        if self.research == RetryStatus::Ready
            && prepare_research_retry(messages, user_message, response)
        {
            self.research = RetryStatus::Used;
            return true;
        }
        false
    }

    fn prepare_failed_execution(
        &mut self,
        messages: &mut Vec<ChatMessage>,
        user_message: &str,
        failed_tools: &[String],
    ) -> bool {
        if self.execution != RetryStatus::Ready
            || failed_tools.is_empty()
            || !user_requires_execution(user_message)
        {
            return false;
        }
        self.execution = RetryStatus::Used;
        messages.push(ChatMessage::text(
            ChatRole::System,
            "The previous inspection tool failed, so it is not evidence that the requested file, directory, program, or state is absent. Call run_shell now and perform a direct host inspection. If execution also fails, report that failure accurately instead of inventing a negative result.",
        ));
        true
    }
}

fn retry_status(definitions: &[ToolDefinition], name: &str) -> RetryStatus {
    if definitions.iter().any(|tool| tool.name == name) {
        RetryStatus::Ready
    } else {
        RetryStatus::Unavailable
    }
}

fn finish_turn(
    round: ModelRound,
    turn: HarnessTurn,
    conversation: &mut Conversation,
    sink: &mut dyn EventSink,
    usage: TokenUsage,
    metrics: &TurnMetrics,
) -> HarnessOutcome {
    let context_tokens = round
        .usage
        .input_tokens
        .saturating_add(round.usage.output_tokens);
    let final_text = sanitize_agent_response(&round.text, &turn.user_message);
    let chunks = if final_text == round.text {
        round.text_chunks
    } else {
        vec![final_text.clone()]
    };
    for chunk in chunks {
        sink.emit(HarnessEvent::TextDelta(chunk));
    }
    conversation.commit(
        turn.user_message,
        final_text.clone(),
        metrics.action_facts.clone(),
    );
    HarnessOutcome {
        text: final_text,
        usage,
        context_tokens,
        model_rounds: metrics.model_rounds,
        tool_calls: metrics.tool_calls,
    }
}

fn safe_successful_action_fact(call: &ToolCall) -> Option<String> {
    if call.name != "run_shell" {
        return None;
    }
    let command = call.arguments.get("command")?.as_str()?.trim();
    if command.is_empty()
        || command.chars().any(|character| {
            matches!(
                character,
                ';' | '|' | '&' | '>' | '<' | '$' | '`' | '\n' | '\r'
            )
        })
    {
        return None;
    }
    let mut words = command.split_whitespace();
    if words.next()? != "mkdir" {
        return None;
    }
    let paths = words
        .filter(|word| !word.starts_with('-'))
        .filter(|word| safe_path_anchor(word))
        .take(8)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }
    Some(format!("Created {}.", paths.join(", ")))
}

fn safe_path_anchor(path: &str) -> bool {
    path.len() <= 512
        && !path.is_empty()
        && path
            .chars()
            .all(|character| character.is_alphanumeric() || "/._~-".contains(character))
}

fn prepare_execution_retry(
    messages: &mut Vec<ChatMessage>,
    user_message: &str,
    response: &str,
) -> bool {
    if !should_retry_with_execution(user_message, response) {
        return false;
    }
    messages.push(ChatMessage::text(ChatRole::Assistant, response));
    messages.push(ChatMessage::text(
        ChatRole::System,
        "Execution is required for this turn. Do not give the user commands or instructions to perform the requested check. Call an available tool now, prefer run_shell for local system or workspace inspection, then report the verified result. run_shell can inspect the host filesystem and installed programs; its starting directory is not an access boundary. The approval broker will ask the user if this exact operation needs permission.",
    ));
    true
}

fn should_retry_with_execution(user_message: &str, response: &str) -> bool {
    if !user_requires_execution(user_message) {
        return false;
    }
    let response = response.to_ascii_lowercase();
    [
        "open your terminal",
        "type the following command",
        "you can use the terminal",
        "you can run",
        "try running",
        "run the following command",
        "here's how you can",
        "here is how you can",
        "i can't directly access",
        "i cannot directly access",
        "i can't access your local",
        "i cannot access your local",
        "i can't inspect your local",
        "i cannot inspect your local",
        "i don't have access to your local",
        "i do not have access to your local",
        "should i proceed",
        "shall i proceed",
        "would you like me to proceed",
        "do you want me to proceed",
        "would you like me to run",
        "do you want me to run",
    ]
    .iter()
    .any(|phrase| response.contains(phrase))
}

fn response_needs_grounding_correction(response: &str, user_message: &str) -> bool {
    let response = response.to_ascii_lowercase();
    let contradicts_evidence = [
        "permission restriction",
        "permission restrictions",
        "due to permissions",
        "due to permission",
        "don't have permission",
        "do not have permission",
        "can't provide the contents",
        "cannot provide the contents",
        "can't provide the details",
        "cannot provide the details",
        "no specific details are available",
    ]
    .iter()
    .any(|phrase| response.contains(phrase));
    let persona_only = strip_persona_boilerplate(&response, asks_about_identity(user_message))
        .trim()
        .is_empty();
    contradicts_evidence || persona_only
}

fn build_request(
    turn: &HarnessTurn,
    messages: &[ChatMessage],
    definitions: &[crumb_llm::ToolDefinition],
    tool_choice: (bool, bool),
) -> ChatRequest {
    let (require_tool, suppress_tools) = tool_choice;
    ChatRequest {
        model: turn.model.clone(),
        messages: messages.to_vec(),
        tools: definitions.to_vec(),
        tool_choice: if definitions.is_empty() || suppress_tools {
            ToolChoice::None
        } else if require_tool {
            ToolChoice::Required
        } else {
            ToolChoice::Auto
        },
        max_output_tokens: turn.max_output_tokens,
    }
}

fn record_successful_tool_evidence(messages: &mut Vec<ChatMessage>, round_succeeded: bool) -> bool {
    if !round_succeeded {
        return false;
    }
    messages.push(ChatMessage::text(
        ChatRole::System,
        "Use the successful tool result as authoritative evidence. Answer every requested item directly; do not invent an access or permission failure and do not add identity or unrelated information.",
    ));
    true
}

fn record_usage(total: &mut TokenUsage, round: TokenUsage) {
    total.input_tokens = total.input_tokens.saturating_add(round.input_tokens);
    total.cached_input_tokens = total
        .cached_input_tokens
        .saturating_add(round.cached_input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(round.output_tokens);
}

fn emit_tool_output(sink: &mut dyn EventSink, call: &ToolCall, result: &ExecutedTool) {
    sink.emit(HarnessEvent::ToolOutput {
        id: call.id.clone(),
        name: call.name.clone(),
        content: result.content.clone(),
    });
    if call.name == "write_file"
        && result.success
        && let Some(change) = result.structured.as_ref()
        && let (Some(path), Some(additions), Some(deletions)) = (
            change.get("path").and_then(serde_json::Value::as_str),
            change.get("additions").and_then(serde_json::Value::as_u64),
            change.get("deletions").and_then(serde_json::Value::as_u64),
        )
    {
        sink.emit(HarnessEvent::FileChanged {
            path: path.to_owned(),
            additions,
            deletions,
        });
    }
}

fn initial_tool_requirement(definitions: &[crumb_llm::ToolDefinition], user_message: &str) -> bool {
    retry_status(definitions, "run_shell") == RetryStatus::Ready
        && user_requires_execution(user_message)
}

#[allow(clippy::too_many_arguments)]
fn prepare_response_retry(
    messages: &mut Vec<ChatMessage>,
    response: &str,
    user_message: &str,
    successful_tool_evidence: bool,
    failed_tool_evidence: bool,
    grounding_retry_used: &mut bool,
    failure_retry_used: &mut bool,
    completeness_retry_used: &mut bool,
    suppress_tools_once: &mut bool,
    retries: &mut RequiredToolRetries,
) -> Option<(bool, bool)> {
    if prepare_grounding_retry(
        messages,
        response,
        user_message,
        successful_tool_evidence,
        grounding_retry_used,
        suppress_tools_once,
    ) {
        return Some((false, *suppress_tools_once));
    }
    if prepare_failure_grounding_retry(
        messages,
        response,
        successful_tool_evidence,
        failed_tool_evidence,
        failure_retry_used,
        suppress_tools_once,
    ) {
        return Some((false, *suppress_tools_once));
    }
    let terminal_tool_failure = failed_tool_evidence
        && !successful_tool_evidence
        && response_reports_execution_failure(response);
    if !terminal_tool_failure
        && prepare_completeness_retry(messages, response, user_message, completeness_retry_used)
    {
        return Some((true, false));
    }
    retries
        .prepare(messages, user_message, response)
        .then_some((true, false))
}

fn prepare_failure_grounding_retry(
    messages: &mut Vec<ChatMessage>,
    response: &str,
    successful_tool_evidence: bool,
    failed_tool_evidence: bool,
    retry_used: &mut bool,
    suppress_tools_once: &mut bool,
) -> bool {
    if successful_tool_evidence
        || !failed_tool_evidence
        || *retry_used
        || response_reports_execution_failure(response)
    {
        return false;
    }
    messages.push(ChatMessage::text(ChatRole::Assistant, response));
    messages.push(ChatMessage::text(
        ChatRole::System,
        "Every attempted tool call failed, so the draft cannot claim a verified result. Correct it using the tool errors already present: state the exact blocker and the smallest actionable recovery. Do not invent success, absence, or permission state, and do not call another tool.",
    ));
    *retry_used = true;
    *suppress_tools_once = true;
    true
}

fn response_reports_execution_failure(response: &str) -> bool {
    let response = response.to_ascii_lowercase();
    [
        "failed",
        "error",
        "permission denied",
        "access denied",
        "not found",
        "unavailable",
        "timed out",
        "timeout",
        "cancelled",
        "canceled",
        "exit code",
        "exit status",
        "could not",
        "couldn't",
        "unable to",
    ]
    .iter()
    .any(|phrase| response.contains(phrase))
}

fn prepare_grounding_retry(
    messages: &mut Vec<ChatMessage>,
    response: &str,
    user_message: &str,
    successful_tool_evidence: bool,
    grounding_retry_used: &mut bool,
    suppress_tools_once: &mut bool,
) -> bool {
    if !successful_tool_evidence
        || *grounding_retry_used
        || !response_needs_grounding_correction(response, user_message)
    {
        return false;
    }
    messages.push(ChatMessage::text(ChatRole::Assistant, response));
    messages.push(ChatMessage::text(
        ChatRole::System,
        "The prior tool execution succeeded. Correct the answer using its result. Do not claim a permission or access restriction, do not repeat identity, and answer only the user's requested items. Do not call another tool.",
    ));
    *grounding_retry_used = true;
    *suppress_tools_once = true;
    true
}

fn user_requires_execution(user_message: &str) -> bool {
    let user_message = user_message.to_ascii_lowercase();
    if [
        "how do i",
        "how can i",
        "what command",
        "show me the command",
        "explain how",
        "do not execute",
        "don't execute",
        "just tell me",
    ]
    .iter()
    .any(|phrase| user_message.contains(phrase))
    {
        return false;
    }
    [
        "check",
        "verify",
        "inspect",
        "find me",
        "find out",
        "locate",
        "can you see",
        "can you access",
        "is there",
        "does this exist",
        "do i have",
        "on my laptop",
        "on my computer",
        "on my system",
        "here?",
        "folder named",
        "file named",
        "in this repo",
        "in this project",
        "for me",
        "run ",
        "execute ",
        "build ",
        "test ",
        "wait for",
        "wait until",
        "watch ",
        "monitor ",
        "keep checking",
        "poll ",
    ]
    .iter()
    .any(|phrase| user_message.contains(phrase))
}

fn user_requires_host_shell(user_message: &str) -> bool {
    let user_message = user_message.to_ascii_lowercase();
    [
        "home directory",
        "my home",
        "on my laptop",
        "on my computer",
        "on my system",
        "local filesystem",
        "outside the workspace",
        "outside this workspace",
        "outside the repo",
        "outside this repo",
        "authenticated via github",
        "authenticated with github",
        "github authentication",
        "github auth status",
        "logged in to github",
        "logged into github",
        "find me the",
        "locate the",
    ]
    .iter()
    .any(|phrase| user_message.contains(phrase))
}

fn prepare_completeness_retry(
    messages: &mut Vec<ChatMessage>,
    response: &str,
    user_message: &str,
    retry_used: &mut bool,
) -> bool {
    if *retry_used || !user_requires_execution(user_message) {
        return false;
    }
    let missing = missing_requested_checks(user_message, response);
    if missing.is_empty() {
        return false;
    }
    messages.push(ChatMessage::text(ChatRole::Assistant, response));
    messages.push(ChatMessage::text(
        ChatRole::System,
        format!(
            "The draft omitted requested result(s): {}. Use the available tool to verify every missing item, then give one concise answer covering all requested checks. Do not silently drop an item.",
            missing.join(", ")
        ),
    ));
    *retry_used = true;
    true
}

fn missing_requested_checks(user_message: &str, response: &str) -> Vec<&'static str> {
    let request = user_message.to_ascii_lowercase();
    let response = response.to_ascii_lowercase();
    let asks_auth = [
        "authenticat",
        "logged in",
        "logged into",
        "signed in",
        "login status",
        "auth status",
    ]
    .iter()
    .any(|term| request.contains(term));
    let answers_auth = [
        "authenticat",
        "logged in",
        "logged into",
        "signed in",
        "not logged",
        "not authenticat",
        "credential",
    ]
    .iter()
    .any(|term| response.contains(term));
    let asks_branch = request.contains("branch");
    let answers_branch = response.contains("branch");
    let mut missing = Vec::new();
    if asks_auth && !answers_auth {
        missing.push(if request.contains("github") {
            "GitHub authentication status"
        } else {
            "authentication status"
        });
    }
    if asks_branch && !answers_branch {
        missing.push("current Git branch");
    }
    let checks = [
        (
            "test result",
            &["test ", "tests", "test suite"][..],
            &["test", "pass", "fail", "error"][..],
        ),
        (
            "build result",
            &["build ", "compile "][..],
            &["build", "compiled", "pass", "fail", "error"][..],
        ),
        (
            "installation or version status",
            &["installed", "version of", "--version"][..],
            &[
                "installed",
                "not installed",
                "version",
                "found",
                "not found",
            ][..],
        ),
        (
            "requested file or directory result",
            &[
                "file named",
                "folder named",
                "directory named",
                "does this exist",
                "is there",
                "locate ",
                "find me",
            ][..],
            &[
                "file",
                "folder",
                "directory",
                "path",
                "found",
                "exists",
                "not found",
                "does not exist",
                "could not",
            ][..],
        ),
        (
            "terminal wait/watch outcome",
            &[
                "wait for",
                "wait until",
                "watch ",
                "monitor ",
                "keep checking",
                "poll ",
            ][..],
            &[
                "completed",
                "finished",
                "passed",
                "failed",
                "timed out",
                "deadline",
                "cancelled",
                "canceled",
            ][..],
        ),
    ];
    for (label, request_terms, response_terms) in checks {
        let requested = request_terms.iter().any(|term| request.contains(term));
        let answered = response_terms.iter().any(|term| response.contains(term));
        if requested && !answered && !missing.contains(&label) {
            missing.push(label);
        }
    }
    missing
}

fn prepare_research_retry(
    messages: &mut Vec<ChatMessage>,
    user_message: &str,
    response: &str,
) -> bool {
    if !should_retry_with_research(user_message, response) {
        return false;
    }
    messages.push(ChatMessage::text(ChatRole::Assistant, response));
    messages.push(ChatMessage::text(
        ChatRole::System,
        "The response exposed uncertainty or deferred web research to the user. Call web_search now using the specific subjects, location, date or time period, and qualifiers from the user's request. Inspect the returned evidence as needed, then answer with verified findings. Do not repeat the uncertainty disclaimer or tell the user to search.",
    ));
    true
}

fn should_retry_with_research(user_message: &str, response: &str) -> bool {
    let user_message = user_message.to_ascii_lowercase();
    if [
        "without searching",
        "without web search",
        "don't search",
        "do not search",
        "offline only",
    ]
    .iter()
    .any(|phrase| user_message.contains(phrase))
    {
        return false;
    }
    let response = response.to_ascii_lowercase();
    [
        "i don't know",
        "i do not know",
        "i'm not sure",
        "i am not sure",
        "i can't verify",
        "i cannot verify",
        "i don't have real-time access",
        "i do not have real-time access",
        "i can't access the internet",
        "i cannot access the internet",
        "knowledge cutoff",
        "use a search engine",
        "check a search engine",
        "check online",
        "check local tourism",
    ]
    .iter()
    .any(|phrase| response.contains(phrase))
}

fn preserve_web_search_terms(call: &mut ToolCall, user_message: &str) {
    if call.name != "web_search" {
        return;
    }
    let Some(query) = call
        .arguments
        .get("query")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let query = compact_search_query(query);
    let query_terms = search_terms(&query);
    let missing = search_terms(user_message)
        .into_iter()
        .filter(|term| !query_terms.contains(term) && !search_term_is_redundant(term, &query_terms))
        .take(8)
        .collect::<Vec<_>>();
    let optimized = if missing.is_empty() {
        query
    } else if query.is_empty() {
        missing.join(" ")
    } else {
        format!("{query} {}", missing.join(" "))
    };
    call.arguments["query"] = serde_json::Value::String(optimized);
}

fn compact_search_query(query: &str) -> String {
    const MAX_TERMS: usize = 16;
    const MAX_BYTES: usize = 192;
    let mut compact = String::new();
    for term in query
        .split_whitespace()
        .filter(|term| !search_query_noise(term))
        .take(MAX_TERMS)
    {
        if !compact.is_empty() {
            compact.push(' ');
        }
        if compact.len().saturating_add(term.len()) > MAX_BYTES {
            break;
        }
        compact.push_str(term);
    }
    compact
}

fn search_query_noise(term: &str) -> bool {
    let normalized = term
        .trim_matches(|character: char| !character.is_alphanumeric())
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "please" | "search" | "find" | "tell" | "me" | "thinking" | "need"
    )
}

fn search_term_is_redundant(term: &str, query_terms: &std::collections::BTreeSet<String>) -> bool {
    let weather_query = query_terms.contains("weather") || query_terms.contains("forecast");
    weather_query && matches!(term, "umbrella" | "raincoat" | "jacket" | "sunscreen")
}

fn search_terms(input: &str) -> std::collections::BTreeSet<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "about", "am", "an", "and", "are", "at", "be", "been", "being", "can", "could", "did",
        "do", "does", "find", "for", "from", "give", "had", "has", "have", "how", "i", "in", "is",
        "it", "know", "look", "me", "might", "need", "of", "on", "please", "search", "should",
        "short", "tell", "the", "thinking", "to", "up", "was", "web", "were", "what", "when",
        "where", "which", "who", "why", "will", "would", "you",
    ];
    input
        .split(|character: char| {
            !character.is_alphanumeric() && character != '-' && character != '_'
        })
        .filter_map(|term| {
            let term = term.to_lowercase();
            (term.chars().count() > 1 && !STOP_WORDS.contains(&term.as_str())).then_some(term)
        })
        .collect()
}

fn tool_request_detail(name: &str, arguments: &serde_json::Value) -> Option<String> {
    let field = match name {
        "run_shell" => "command",
        "web_search" => "query",
        "web_fetch" => "url",
        "read_file" | "list_directory" | "write_file" => "path",
        _ => return None,
    };
    let raw = arguments.get(field)?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut detail = String::with_capacity(raw.len().min(512));
    for character in raw.chars() {
        if detail.len() >= 512 {
            detail.push('…');
            break;
        }
        if character == '\n' || character == '\r' || character == '\t' {
            detail.push(' ');
        } else if !character.is_control() {
            detail.push(character);
        }
    }
    Some(detail)
}

const BASE_SYSTEM_PROMPT: &str = "Your name is Crumb, Elixpo's execution-oriented terminal agent. This identity overrides the provider: never claim to be an unnamed generic AI, ChatGPT, Claude, Amazon's assistant, or the underlying model. Mention your identity or creator only when asked; then answer once: Crumb by Elixpo. Child-agent names are temporary call signs. Sound like a clever terminal sidekick: warm, mischievous, confidently useful, a little sassy, and genuinely funny when the moment invites it. Use natural emojis for flavor, usually zero to two per response; favor one fitting emoji in casual greetings. Never force a punchline, become noisy, or mock the user. For greetings use one short playful line. Avoid corporate support filler, repeated role descriptions, and repeated conclusions. Use a serious clear tone for security, permissions, destructive actions, data loss, and failures; clarity wins every time. Complete the user's latest request yourself. For requests to check, inspect, find, verify, build, test, or operate local state, use tools and report evidence; Do not tell the user to run a command you can run. Use run_shell for bounded Bash. Submit valid ordinary shell syntax: Crumb applies RTK optimization automatically, so never invent RTK syntax. Never ask in prose whether to proceed; call the tool and let Crumb request interactive approval when needed. Treat older turns only as context. Use only authoritative runtime context for claims about session, memory, paths, and capabilities. Before answering, compare the evidence with every requested item; continue until each item is answered or report the exact blocker. Never offer to 'dive deeper' for work already requested. For changing facts use web_search; use web_fetch for known pages. Public-source research about named people is allowed when non-sensitive. Preserve every person name, organization, product, place, quoted phrase, and qualifier in search queries; never broaden away the subject. On tool failure, inspect the error and try a safe suitable alternative. Never invent or claim an unverified result. Permissions are user-owned: request approval, respect denial, and never attempt to widen them yourself. Native shell semantics remain separate from agent tools. Use concise terminal Markdown (headings, lists, links, inline/fenced code, Mermaid) without HTML or image-only answers; Mermaid must remain readable as source.";

fn system_messages(turn: &HarnessTurn) -> Vec<ChatMessage> {
    let mut stable = String::from(BASE_SYSTEM_PROMPT);
    if let Some(persona) = turn
        .persona
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        stable.push_str("\n\n## Selected persona\n");
        stable.push_str(persona);
    }
    let mut messages = vec![ChatMessage::text(ChatRole::System, stable)];
    if !turn.static_context.is_empty() {
        messages.push(ChatMessage::text(
            ChatRole::System,
            format!("## Active skills\n{}", turn.static_context.join("\n\n")),
        ));
    }
    if !turn.trusted_context.is_empty() {
        let mut context = String::from("## Trusted runtime context\n");
        context.push_str(&turn.trusted_context.join("\n\n"));
        messages.push(ChatMessage::text(ChatRole::System, context));
    }
    messages
}

fn sanitize_agent_response(text: &str, user_message: &str) -> String {
    let text = strip_private_reasoning(text);
    let normalized = [
        "I am Crumb, an AI system",
        "I'm Crumb, an AI system",
        "I am an AI system",
        "I'm an AI system",
        "built by a team of inventors at Amazon",
        "created by a team of inventors at Amazon",
        "built by Amazon",
        "created by Amazon",
    ]
    .into_iter()
    .enumerate()
    .fold(text, |output, (index, claim)| {
        let replacement = if index < 4 {
            "I am Crumb, an AI agent"
        } else {
            "built by Elixpo"
        };
        replace_ascii_case_insensitive(&output, claim, replacement)
    });
    let identity_requested = asks_about_identity(user_message);
    let normalized_lowercase = normalized.to_ascii_lowercase();
    if identity_requested
        && (normalized_lowercase.contains("i don't have a name")
            || normalized_lowercase.contains("i do not have a name")
            || normalized_lowercase.contains("i don't have a \"name\"")
            || normalized_lowercase.contains("i do not have a \"name\""))
    {
        return "I’m Crumb, built by Elixpo.".to_owned();
    }
    let filtered = strip_persona_boilerplate(&normalized, identity_requested);
    if filtered.trim().is_empty() {
        if identity_requested {
            "I’m Crumb, built by Elixpo.".to_owned()
        } else {
            "Ready when you are—give me something interesting.".to_owned()
        }
    } else {
        filtered.trim().to_owned()
    }
}

fn asks_about_identity(user_message: &str) -> bool {
    let message = user_message.to_ascii_lowercase();
    let concise_question = message.trim().trim_end_matches(['?', '!', '.']).trim();
    if matches!(concise_question, "what are you" | "what r u") {
        return true;
    }
    [
        "who are you",
        "who r u",
        "your name",
        "ur name",
        "who is crumb",
        "who made you",
        "who built you",
        "who created you",
        "what is your role",
        "what's your role",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

fn strip_persona_boilerplate(text: &str, identity_requested: bool) -> String {
    text.split('\n')
        .map(|line| {
            let sentences = line
                .split_inclusive(['.', '?', '!'])
                .map(str::trim)
                .filter(|sentence| !sentence.is_empty())
                .collect::<Vec<_>>();
            if !sentences
                .iter()
                .any(|sentence| is_persona_boilerplate(sentence, identity_requested))
            {
                return line.to_owned();
            }
            sentences
                .into_iter()
                .filter(|sentence| !is_persona_boilerplate(sentence, identity_requested))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_persona_boilerplate(sentence: &str, identity_requested: bool) -> bool {
    let sentence = sentence
        .trim_start_matches(['-', '•', '*', ' '])
        .to_ascii_lowercase();
    let identity_preamble = sentence.starts_with("i am crumb")
        || sentence.starts_with("i'm crumb")
        || sentence.starts_with("i’m crumb")
        || sentence.starts_with("i am an ai")
        || sentence.starts_with("i'm an ai")
        || sentence.starts_with("i’m an ai")
        || sentence.starts_with("you can think of me as an ai");
    let unsolicited_identity_aside = !identity_requested
        && sentence.contains("identity")
        && (sentence.contains("i am crumb")
            || sentence.contains("i'm crumb")
            || sentence.contains("i’m crumb"));
    let false_identity_denial = identity_requested
        && (sentence.contains("i don't have a name")
            || sentence.contains("i do not have a name")
            || sentence.contains("i don't have a \"name\"")
            || sentence.contains("i do not have a \"name\""));
    let generic_invitation = (sentence.starts_with("if you have")
        || sentence.starts_with("if you need"))
        && (sentence.contains("let me know") || sentence.contains("feel free to ask"));
    (!identity_requested && identity_preamble)
        || unsolicited_identity_aside
        || false_identity_denial
        || generic_invitation
        || sentence.starts_with("how can i help")
        || sentence.starts_with("i am here to assist")
        || sentence.starts_with("i'm here to assist")
        || sentence.starts_with("i’m here to assist")
        || sentence.starts_with("i am here to help")
        || sentence.starts_with("i'm here to help")
        || sentence.starts_with("i’m here to help")
        || sentence.starts_with("my role is to assist")
        || sentence.starts_with("my role is to help")
        || sentence.starts_with("if you need assistance")
        || sentence.starts_with("if you have any specific questions")
        || sentence.starts_with("if your message was informal")
        || sentence.starts_with("if there's anything else")
        || sentence.starts_with("if there’s anything else")
        || sentence.starts_with("feel free to ask")
        || sentence.contains("in a safe and respectful manner")
        || sentence.contains("in a polite, respectful, and safe manner")
}

fn strip_private_reasoning(text: &str) -> String {
    let mut output = text.to_owned();
    for (opening, closing) in [("<thinking>", "</thinking>"), ("<analysis>", "</analysis>")] {
        loop {
            let lowercase = output.to_ascii_lowercase();
            let Some(start) = lowercase.find(opening) else {
                break;
            };
            let Some(relative_end) = lowercase[start + opening.len()..].find(closing) else {
                output.truncate(start);
                break;
            };
            let end = start + opening.len() + relative_end + closing.len();
            output.replace_range(start..end, "");
        }
    }
    output
}

fn replace_ascii_case_insensitive(text: &str, needle: &str, replacement: &str) -> String {
    let lowercase = text.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut output = String::with_capacity(text.len());
    let mut start = 0;
    while let Some(offset) = lowercase[start..].find(&needle) {
        let match_start = start + offset;
        let match_end = match_start + needle.len();
        output.push_str(&text[start..match_start]);
        output.push_str(replacement);
        start = match_end;
    }
    output.push_str(&text[start..]);
    output
}

fn add_output_bytes(
    consumed: &mut usize,
    additional: usize,
    limits: HarnessLimits,
) -> Result<(), HarnessError> {
    *consumed = consumed.checked_add(additional).ok_or_else(|| {
        HarnessError::new(HarnessErrorKind::OutputLimit, "output byte limit reached")
    })?;
    if *consumed > limits.max_output_bytes.get() {
        return Err(HarnessError::new(
            HarnessErrorKind::OutputLimit,
            "output byte limit reached",
        ));
    }
    Ok(())
}

async fn controlled<F, T>(
    future: F,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<T, HarnessError>
where
    F: Future<Output = T>,
{
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(HarnessError::new(
            HarnessErrorKind::Cancelled,
            "turn cancelled",
        )),
        () = tokio::time::sleep_until(deadline) => Err(HarnessError::new(
            HarnessErrorKind::DeadlineExceeded,
            "turn deadline exceeded",
        )),
        value = future => Ok(value),
    }
}

fn ensure_active(cancellation: &CancellationToken, deadline: Instant) -> Result<(), HarnessError> {
    if cancellation.is_cancelled() {
        Err(HarnessError::new(
            HarnessErrorKind::Cancelled,
            "turn cancelled",
        ))
    } else if Instant::now() >= deadline {
        Err(HarnessError::new(
            HarnessErrorKind::DeadlineExceeded,
            "turn deadline exceeded",
        ))
    } else {
        Ok(())
    }
}

fn provider_error(error: ProviderError) -> HarnessError {
    let ProviderError { message, .. } = error;
    HarnessError::new(
        HarnessErrorKind::Provider,
        format!("model provider failed: {message}"),
    )
}

fn persistence_error(_error: anyhow::Error) -> HarnessError {
    HarnessError::new(
        HarnessErrorKind::Persistence,
        "session metadata could not be persisted",
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::Result;
    use crumb_agent::{
        AgentMode, AgentSession, CancellationToken, DenyAllApprovals, RiskClass, SessionId,
        SessionJournal, ToolDescriptor, ToolHandler, ToolHost, ToolOutput, ToolTransport,
        export_session,
    };
    use crumb_llm::{
        ChatEvent, ChatRequest, ChatRole, ChatStream, EmbeddingRequest, EmbeddingResponse,
        FinishReason, LlmProvider, ProviderError, ProviderErrorKind, ProviderFuture, TokenUsage,
        ToolCall, ToolChoice,
    };
    use serde_json::json;

    use super::{
        Conversation, HarnessErrorKind, HarnessEvent, HarnessLimits, HarnessTurn, NativeHarness,
    };

    #[derive(Default)]
    struct ScriptedProvider {
        rounds: Mutex<VecDeque<Vec<ChatEvent>>>,
        requests: Mutex<Vec<ChatRequest>>,
        pending: bool,
    }

    impl ScriptedProvider {
        fn new(rounds: Vec<Vec<ChatEvent>>) -> Self {
            Self {
                rounds: Mutex::new(rounds.into()),
                requests: Mutex::new(Vec::new()),
                pending: false,
            }
        }

        fn pending() -> Self {
            Self {
                pending: true,
                ..Self::default()
            }
        }
    }

    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn list_models(&self) -> ProviderFuture<'_, Vec<crumb_llm::ModelInfo>> {
            Box::pin(future::ready(Ok(Vec::new())))
        }

        fn chat_stream(&self, request: ChatRequest) -> ProviderFuture<'_, Box<dyn ChatStream>> {
            self.requests.lock().expect("request lock").push(request);
            if self.pending {
                return Box::pin(future::ready(Ok(
                    Box::new(PendingStream) as Box<dyn ChatStream>
                )));
            }
            let events = self
                .rounds
                .lock()
                .expect("round lock")
                .pop_front()
                .expect("scripted round");
            Box::pin(future::ready(Ok(Box::new(EventStream {
                events: events.into(),
            }) as Box<dyn ChatStream>)))
        }

        fn embeddings(&self, _request: EmbeddingRequest) -> ProviderFuture<'_, EmbeddingResponse> {
            Box::pin(future::ready(Err(ProviderError::new(
                ProviderErrorKind::Other,
                "not implemented by fixture",
                false,
            ))))
        }
    }

    struct EventStream {
        events: VecDeque<ChatEvent>,
    }

    impl ChatStream for EventStream {
        fn next(&mut self) -> ProviderFuture<'_, Option<ChatEvent>> {
            Box::pin(future::ready(Ok(self.events.pop_front())))
        }
    }

    struct PendingStream;

    impl ChatStream for PendingStream {
        fn next(&mut self) -> ProviderFuture<'_, Option<ChatEvent>> {
            Box::pin(future::pending())
        }
    }

    struct CountingTool(Arc<AtomicUsize>);

    impl ToolHandler for CountingTool {
        fn call(
            &self,
            _arguments: &serde_json::Value,
            _cancellation: &CancellationToken,
        ) -> Result<ToolOutput> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(ToolOutput::text("24 C"))
        }
    }

    struct FailingTool(Arc<AtomicUsize>);

    impl ToolHandler for FailingTool {
        fn call(
            &self,
            _arguments: &serde_json::Value,
            _cancellation: &CancellationToken,
        ) -> Result<ToolOutput> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(ToolOutput::error("permission denied by host policy"))
        }
    }

    fn tools(calls: Arc<AtomicUsize>) -> ToolHost {
        let mut tools = ToolHost::default();
        tools
            .register(
                ToolDescriptor {
                    name: "weather".to_owned(),
                    description: "Read weather".to_owned(),
                    input_schema: json!({"type":"object"}),
                    risk: RiskClass::ReadOnly,
                    transport: ToolTransport::Native,
                },
                Arc::new(CountingTool(calls)),
            )
            .expect("tool registers");
        tools
    }

    fn shell_tools(calls: Arc<AtomicUsize>) -> ToolHost {
        let mut tools = ToolHost::default();
        tools
            .register(
                ToolDescriptor {
                    name: "run_shell".to_owned(),
                    description: "Run a shell command".to_owned(),
                    input_schema: json!({"type":"object"}),
                    risk: RiskClass::ReadOnly,
                    transport: ToolTransport::Native,
                },
                Arc::new(CountingTool(calls)),
            )
            .expect("tool registers");
        tools
    }

    fn failing_shell_tools(calls: Arc<AtomicUsize>) -> ToolHost {
        let mut tools = ToolHost::default();
        tools
            .register(
                ToolDescriptor {
                    name: "run_shell".to_owned(),
                    description: "Run a shell command".to_owned(),
                    input_schema: json!({"type":"object"}),
                    risk: RiskClass::ReadOnly,
                    transport: ToolTransport::Native,
                },
                Arc::new(FailingTool(calls)),
            )
            .expect("tool registers");
        tools
    }

    fn web_tools(calls: Arc<AtomicUsize>) -> ToolHost {
        let mut tools = ToolHost::default();
        tools
            .register(
                ToolDescriptor {
                    name: "web_search".to_owned(),
                    description: "Search the public web".to_owned(),
                    input_schema: json!({"type":"object","required":["query"]}),
                    risk: RiskClass::ReadOnly,
                    transport: ToolTransport::Native,
                },
                Arc::new(CountingTool(calls)),
            )
            .expect("tool registers");
        tools
    }

    fn session(mode: AgentMode) -> (tempfile::TempDir, AgentSession) {
        let root = tempfile::tempdir().expect("session root");
        let id = SessionId::new("harness-test").expect("session id");
        let journal = SessionJournal::open(root.path(), &id).expect("journal opens");
        let session = AgentSession::start(id, mode, root.path().to_path_buf(), journal)
            .expect("session starts");
        (root, session)
    }

    fn turn() -> HarnessTurn {
        HarnessTurn {
            model: "fixture".to_owned(),
            user_message: "weather in Pune".to_owned(),
            images: Vec::new(),
            persona: Some("Be warm and brief.".to_owned()),
            static_context: vec!["Skill: web research".to_owned()],
            trusted_context: vec!["Locale: en-IN".to_owned()],
            max_output_tokens: Some(64),
        }
    }

    #[test]
    fn base_prompt_requires_execution_instead_of_command_instructions() {
        let messages = super::system_messages(&turn());

        assert!(
            messages[0]
                .content
                .contains("Complete the user's latest request yourself")
        );
        assert!(
            messages[0]
                .content
                .contains("Do not tell the user to run a command")
        );
        assert!(messages[0].content.contains("never attempt to widen"));
        assert!(messages[0].content.contains("Your name is Crumb"));
        assert!(messages[0].content.contains("Crumb by Elixpo"));
        assert!(messages[0].content.contains("a little sassy"));
        assert!(messages[0].content.contains("natural emojis"));
        assert!(messages[0].content.contains("clever terminal sidekick"));
        assert!(messages[0].content.contains("Never force a punchline"));
        assert!(messages[0].content.contains("clarity wins every time"));
        assert!(messages[0].content.contains("Preserve every person name"));
        assert!(
            messages[0]
                .content
                .contains("Public-source research about named people is allowed")
        );
        assert!(
            messages[0]
                .content
                .contains("authoritative runtime context")
        );
        assert!(messages[0].content.contains("latest request"));
        assert!(messages[0].content.contains("every requested item"));
        assert!(messages[0].content.contains("Never offer to 'dive deeper'"));
        assert!(super::BASE_SYSTEM_PROMPT.len() < 2_500);
        assert_eq!(messages.len(), 3);
        assert!(
            messages
                .iter()
                .all(|message| message.role == ChatRole::System)
        );
        assert!(messages[0].content.contains("Be warm and brief."));
        assert!(!messages[0].content.contains("Skill: web research"));
        assert!(messages[1].content.contains("Skill: web research"));
        assert!(messages[2].content.contains("Locale: en-IN"));
    }

    #[test]
    fn dynamic_skills_do_not_invalidate_the_stable_system_cache_prefix() {
        let first = turn();
        let mut second = turn();
        second.static_context = vec!["Skill: safe refactor".to_owned()];

        let first_messages = super::system_messages(&first);
        let second_messages = super::system_messages(&second);

        assert_eq!(first_messages[0], second_messages[0]);
        assert_ne!(first_messages[1], second_messages[1]);
    }

    #[test]
    fn casual_chat_omits_the_tool_catalog() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tools(calls);

        for greeting in [
            "hello",
            "Hey there!",
            "heck yah what's up buddy what's going on",
        ] {
            assert!(
                super::tool_definitions_for_turn(&tools, greeting).is_empty(),
                "casual greeting should not spend tokens on tools: {greeting}"
            );
        }
    }

    #[test]
    fn greetings_with_real_work_keep_the_tool_catalog() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tools(calls);

        for request in [
            "hey, check the weather",
            "hello, explain this project",
            "yo, search for today's news",
        ] {
            assert_eq!(
                super::tool_definitions_for_turn(&tools, request).len(),
                1,
                "work request must retain available tools: {request}"
            );
        }
    }

    #[test]
    fn provider_failures_keep_the_safe_upstream_diagnostic() {
        let error = super::provider_error(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Pollinations request failed with HTTP 400 Bad Request",
            false,
        ));

        assert_eq!(error.kind, HarnessErrorKind::Provider);
        assert_eq!(
            error.to_string(),
            "model provider failed: Pollinations request failed with HTTP 400 Bad Request"
        );
    }

    fn tool_round() -> Vec<ChatEvent> {
        vec![
            ChatEvent::TextDelta("Unverified placeholder draft.".to_owned()),
            ChatEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "weather".to_owned(),
                arguments: json!({"city":"Pune"}),
            }),
            ChatEvent::Usage(TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 80,
                output_tokens: 2,
            }),
            ChatEvent::Finished(FinishReason::ToolCall),
        ]
    }

    fn answer_round() -> Vec<ChatEvent> {
        vec![
            ChatEvent::TextDelta("It is 24 C.".to_owned()),
            ChatEvent::Usage(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 5,
                output_tokens: 5,
            }),
            ChatEvent::Finished(FinishReason::Stop),
        ]
    }

    fn web_search_round() -> Vec<ChatEvent> {
        vec![
            ChatEvent::ToolCall(ToolCall {
                id: "search_1".to_owned(),
                name: "web_search".to_owned(),
                arguments: json!({"query":"current festival Kolkata"}),
            }),
            ChatEvent::Finished(FinishReason::ToolCall),
        ]
    }

    #[tokio::test]
    async fn approved_tool_runs_once_in_a_multi_round_turn() {
        let provider = Arc::new(ScriptedProvider::new(vec![tool_round(), answer_round()]));
        let calls = Arc::new(AtomicUsize::new(0));
        let harness = NativeHarness::new(
            provider.clone(),
            tools(calls.clone()),
            Arc::new(DenyAllApprovals),
            HarnessLimits::default(),
        );
        let (root, mut session) = session(AgentMode::Auto);
        let mut conversation = harness.conversation();
        let mut events = Vec::new();

        let outcome = harness
            .run_turn(
                &mut session,
                &mut conversation,
                turn(),
                &CancellationToken::default(),
                &mut events,
            )
            .await
            .expect("turn succeeds");

        assert_eq!(outcome.text, "It is 24 C.");
        assert_eq!(outcome.model_rounds, 2);
        assert_eq!(outcome.tool_calls, 1);
        assert_eq!(outcome.usage.input_tokens, 110);
        assert_eq!(outcome.usage.cached_input_tokens, 85);
        assert_eq!(outcome.usage.output_tokens, 7);
        assert_eq!(outcome.context_tokens, 15);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(conversation.len(), 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HarnessEvent::ToolFinished { success: true, .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            HarnessEvent::ToolOutput { name, content, .. }
                if name == "weather" && content == "24 C"
        )));
        let streamed = events
            .iter()
            .filter_map(|event| match event {
                HarnessEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(streamed, "It is 24 C.");
        assert!(!streamed.contains("placeholder"));
        let requests = provider.requests.lock().expect("request lock");
        assert_eq!(requests[0].messages[0].role, crumb_llm::ChatRole::System);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.role == crumb_llm::ChatRole::Tool)
        );
        assert_eq!(requests[1].tool_choice, ToolChoice::None);
        assert_eq!(requests[1].tools.len(), 1);
        drop(requests);
        let journal = export_session(root.path(), "harness-test").expect("session exports");
        assert!(journal.events.iter().any(|event| matches!(
            event,
            crumb_agent::SessionEvent::ToolRequested {
                name,
                arguments_digest,
                ..
            } if name == "weather" && arguments_digest.len() == 64
        )));
        assert!(journal.events.iter().any(|event| matches!(
            event,
            crumb_agent::SessionEvent::ToolFinished {
                name,
                success: true,
                ..
            } if name == "weather"
        )));
        let persisted = serde_json::to_string(&journal).expect("session serializes");
        for secret in ["weather in Pune", "Pune", "24 C", "Be warm and brief"] {
            assert!(!persisted.contains(secret));
        }
    }

    #[tokio::test]
    async fn successful_tool_result_corrects_false_permission_refusal_without_rerunning() {
        let refusal_round = vec![
            ChatEvent::TextDelta(
                "Sorry, I cannot provide the contents due to permission restrictions.\n\nAs for my identity, I am Crumb, an AI agent built by Elixpo."
                    .to_owned(),
            ),
            ChatEvent::Finished(FinishReason::Stop),
        ];
        let corrected_round = vec![
            ChatEvent::TextDelta("The requested folders are `one` and `two`.".to_owned()),
            ChatEvent::Finished(FinishReason::Stop),
        ];
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_round(),
            refusal_round,
            corrected_round,
        ]));
        let calls = Arc::new(AtomicUsize::new(0));
        let harness = NativeHarness::new(
            provider.clone(),
            tools(calls.clone()),
            Arc::new(DenyAllApprovals),
            HarnessLimits::default(),
        );
        let (root, mut session) = session(AgentMode::Auto);
        let mut conversation = harness.conversation();
        let mut events = Vec::new();

        let outcome = harness
            .run_turn(
                &mut session,
                &mut conversation,
                turn(),
                &CancellationToken::default(),
                &mut events,
            )
            .await
            .expect("turn succeeds");

        assert_eq!(outcome.text, "The requested folders are `one` and `two`.");
        assert_eq!(outcome.model_rounds, 3);
        assert_eq!(outcome.tool_calls, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let requests = provider.requests.lock().expect("request lock");
        assert_eq!(requests[2].tool_choice, ToolChoice::None);
        drop(root);
    }

    #[tokio::test]
    async fn uncertainty_about_current_facts_retries_with_web_search() {
        let refusal = vec![
            ChatEvent::TextDelta(
                "I don't have real-time access. Check a search engine for Kolkata festivals."
                    .to_owned(),
            ),
            ChatEvent::Finished(FinishReason::Stop),
        ];
        let answer = vec![
            ChatEvent::TextDelta("Kolkata is celebrating Durga Puja.".to_owned()),
            ChatEvent::Finished(FinishReason::Stop),
        ];
        let provider = Arc::new(ScriptedProvider::new(vec![
            refusal,
            web_search_round(),
            answer,
        ]));
        let calls = Arc::new(AtomicUsize::new(0));
        let harness = NativeHarness::new(
            provider.clone(),
            web_tools(calls.clone()),
            Arc::new(DenyAllApprovals),
            HarnessLimits::default(),
        );
        let (root, mut session) = session(AgentMode::Auto);
        let mut conversation = harness.conversation();
        let mut events = Vec::new();
        let mut research_turn = turn();
        research_turn.user_message = "What festival is in Kolkata now?".to_owned();

        let outcome = harness
            .run_turn(
                &mut session,
                &mut conversation,
                research_turn,
                &CancellationToken::default(),
                &mut events,
            )
            .await
            .expect("turn succeeds");

        assert_eq!(outcome.text, "Kolkata is celebrating Durga Puja.");
        assert_eq!(outcome.model_rounds, 3);
        assert_eq!(outcome.tool_calls, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let requests = provider.requests.lock().expect("request lock");
        assert_eq!(requests[1].tool_choice, ToolChoice::Required);
        assert!(requests[1].messages.iter().any(|message| {
            message.role == ChatRole::System && message.content.contains("Call web_search now")
        }));
        assert!(!outcome.text.contains("search engine"));
        drop(requests);
        drop(root);
    }

    #[test]
    fn explicit_offline_request_does_not_force_research() {
        assert!(!super::should_retry_with_research(
            "Answer from memory without searching",
            "I'm not sure; check online."
        ));
    }

    #[tokio::test]
    async fn execution_request_retries_with_a_required_tool_instead_of_instructions() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                ChatEvent::TextDelta(
                    "Open your terminal and type the following command: g++ --version".to_owned(),
                ),
                ChatEvent::Finished(FinishReason::Stop),
            ],
            vec![
                ChatEvent::ToolCall(ToolCall {
                    id: "shell_1".to_owned(),
                    name: "run_shell".to_owned(),
                    arguments: json!({"command":"command -v g++ && g++ --version"}),
                }),
                ChatEvent::Finished(FinishReason::ToolCall),
            ],
            vec![
                ChatEvent::TextDelta("g++ is installed and available.".to_owned()),
                ChatEvent::Finished(FinishReason::Stop),
            ],
        ]));
        let calls = Arc::new(AtomicUsize::new(0));
        let harness = NativeHarness::new(
            provider.clone(),
            shell_tools(calls.clone()),
            Arc::new(DenyAllApprovals),
            HarnessLimits::default(),
        );
        let (root, mut session) = session(AgentMode::Auto);
        let mut conversation = harness.conversation();
        let mut events = Vec::new();
        let mut request = turn();
        request.user_message = "do i have g++ here? check my laptop".to_owned();

        let outcome = harness
            .run_turn(
                &mut session,
                &mut conversation,
                request,
                &CancellationToken::default(),
                &mut events,
            )
            .await
            .expect("turn succeeds");

        assert_eq!(outcome.text, "g++ is installed and available.");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let streamed = events
            .iter()
            .filter_map(|event| match event {
                HarnessEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(streamed, "g++ is installed and available.");
        let requests = provider.requests.lock().expect("request lock");
        assert_eq!(requests[0].tool_choice, ToolChoice::Required);
        assert_eq!(
            requests[0]
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["run_shell"]
        );
        assert_eq!(requests[1].tool_choice, ToolChoice::Required);
        assert_eq!(requests[2].tool_choice, ToolChoice::None);
        assert!(requests[1].messages.iter().any(|message| {
            message.role == ChatRole::System
                && message
                    .content
                    .contains("Execution is required for this turn")
        }));
        drop(requests);
        drop(root);
    }

    #[test]
    fn instructional_questions_do_not_force_execution() {
        assert!(!super::should_retry_with_execution(
            "how do I check whether g++ is installed?",
            "Open your terminal and type the following command: g++ --version"
        ));
        assert!(super::should_retry_with_execution(
            "do I have g++ here? check my laptop",
            "Open your terminal and type the following command: g++ --version"
        ));
        assert!(super::should_retry_with_execution(
            "find me the elixpo_projects folder and the subfolder inside it",
            "You can run find to locate it."
        ));
        assert!(super::should_retry_with_execution(
            "can you see a folder named elixpo_project here?",
            "Sorry, but I can't directly access or inspect your local files or folders."
        ));
        assert!(super::should_retry_with_execution(
            "is there a file named Cargo.toml in this project?",
            "I do not have access to your local filesystem."
        ));
        assert!(super::should_retry_with_execution(
            "delete the generated build directory",
            "I found it. Should I proceed?"
        ));
        assert!(super::user_requires_host_shell(
            "Can you see a folder named elixpo_projects in my home directory?"
        ));
        assert!(super::user_requires_host_shell(
            "check i am authenticated via github in this session and tell me which branch i am in"
        ));
        assert!(!super::user_requires_host_shell(
            "Can you see a folder named src in this project?"
        ));
    }

    #[test]
    fn github_and_branch_checks_cannot_silently_drop_half_the_request() {
        let request = "check i am authenticated via github and tell me which branch i am in";

        assert_eq!(
            super::missing_requested_checks(request, "You are currently in the feat/agent branch."),
            ["GitHub authentication status"]
        );
        assert_eq!(
            super::missing_requested_checks(request, "GitHub CLI is authenticated as elixpo."),
            ["current Git branch"]
        );
        assert_eq!(
            super::missing_requested_checks(
                request,
                "GitHub CLI is authenticated as elixpo; the current branch is feat/agent."
            ),
            Vec::<&'static str>::new()
        );
    }

    #[test]
    fn general_multi_part_checks_report_each_missing_outcome() {
        let request = "build the project, run the tests, and tell me whether g++ is installed";

        assert_eq!(
            super::missing_requested_checks(request, "The build completed successfully."),
            ["test result", "installation or version status"]
        );
        assert_eq!(
            super::missing_requested_checks(
                request,
                "The build completed, all tests passed, and g++ is installed."
            ),
            Vec::<&'static str>::new()
        );
        assert_eq!(
            super::missing_requested_checks(
                "check whether I am authenticated to npm and run the tests",
                "All tests passed."
            ),
            ["authentication status"]
        );
    }

    #[test]
    fn wait_requests_require_a_terminal_outcome() {
        let request = "watch the checks and wait for them to finish";

        assert!(super::user_requires_execution(request));
        assert_eq!(
            super::missing_requested_checks(request, "The checks are still running."),
            ["terminal wait/watch outcome"]
        );
        assert!(super::missing_requested_checks(request, "The checks passed.").is_empty());
    }

    #[test]
    fn explicit_failure_reports_are_accepted_but_unverified_success_is_not() {
        assert!(super::response_reports_execution_failure(
            "The command failed with permission denied."
        ));
        assert!(!super::response_reports_execution_failure(
            "The requested directory exists."
        ));
    }

    #[tokio::test]
    async fn completely_failed_tool_chain_cannot_be_reported_as_success() {
        let shell_call = |id: &str| {
            vec![
                ChatEvent::ToolCall(ToolCall {
                    id: id.to_owned(),
                    name: "run_shell".to_owned(),
                    arguments: json!({"command":"command -v g++"}),
                }),
                ChatEvent::Finished(FinishReason::ToolCall),
            ]
        };
        let provider = Arc::new(ScriptedProvider::new(vec![
            shell_call("shell_1"),
            shell_call("shell_2"),
            vec![
                ChatEvent::TextDelta("g++ is installed and available.".to_owned()),
                ChatEvent::Finished(FinishReason::Stop),
            ],
            vec![
                ChatEvent::TextDelta(
                    "The check failed because host policy denied the command.".to_owned(),
                ),
                ChatEvent::Finished(FinishReason::Stop),
            ],
        ]));
        let calls = Arc::new(AtomicUsize::new(0));
        let harness = NativeHarness::new(
            provider.clone(),
            failing_shell_tools(calls.clone()),
            Arc::new(DenyAllApprovals),
            HarnessLimits::default(),
        );
        let (root, mut session) = session(AgentMode::Auto);
        let mut conversation = harness.conversation();
        let mut events = Vec::new();
        let mut request = turn();
        request.user_message = "check whether g++ is installed".to_owned();

        let outcome = harness
            .run_turn(
                &mut session,
                &mut conversation,
                request,
                &CancellationToken::default(),
                &mut events,
            )
            .await
            .expect("turn succeeds with an honest blocker");

        assert_eq!(
            outcome.text,
            "The check failed because host policy denied the command."
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let requests = provider.requests.lock().expect("request lock");
        assert_eq!(requests[3].tool_choice, ToolChoice::None);
        assert!(requests[3].messages.iter().any(|message| {
            message.role == ChatRole::System
                && message.content.contains("Every attempted tool call failed")
        }));
        drop(requests);
        drop(root);
    }

    #[test]
    fn provider_identity_claims_are_corrected_at_the_product_boundary() {
        assert_eq!(
            super::sanitize_agent_response(
                "I am Crumb, an AI system built by a team of inventors at Amazon.",
                "who made you?"
            ),
            "I am Crumb, an AI agent built by Elixpo."
        );
        assert_eq!(
            super::sanitize_agent_response(
                "Amazon Web Services is a cloud platform.",
                "what is AWS?"
            ),
            "Amazon Web Services is a cloud platform."
        );
        assert_eq!(
            super::sanitize_agent_response(
                "<thinking>private chain of thought</thinking>\n\nI am an AI system built by Elixpo.",
                "who are you?"
            ),
            "I am Crumb, an AI agent built by Elixpo."
        );
    }

    #[test]
    fn repetitive_persona_boilerplate_is_removed() {
        assert!(super::response_needs_grounding_correction(
            "I am Crumb, an AI agent built by Elixpo.",
            "find the folder"
        ));
        assert_eq!(
            super::sanitize_agent_response(
                "I'm Crumb, an AI system built by Elixpo. I'm here to assist you in a safe and respectful manner. How can I help you today? If you need assistance with a specific task, just let me know!",
                "hello"
            ),
            "Ready when you are—give me something interesting."
        );
        assert_eq!(
            super::sanitize_agent_response(
                "I am Crumb, an AI system built by Elixpo. The test suite passes. If there's anything else, feel free to ask.",
                "did the tests pass?"
            ),
            "The test suite passes."
        );
        assert_eq!(
            super::sanitize_agent_response(
                "The folder is `/tmp`.\n\nAs for my identity, I am Crumb, an AI agent built by Elixpo.",
                "find the folder"
            ),
            "The folder is `/tmp`."
        );
        assert_eq!(
            super::sanitize_agent_response(
                "I am Crumb, an AI system built by Elixpo. How can I help you today?",
                "who are you?"
            ),
            "I am Crumb, an AI agent built by Elixpo."
        );
        assert_eq!(
            super::sanitize_agent_response(
                "I'm Crumb, an AI system built by Elixpo. If you have a specific question or need help with something, please let me know how I can assist you today! If your message was informal or unclear, I'm here to understand and help.",
                "yo oy"
            ),
            "Ready when you are—give me something interesting."
        );
        assert_eq!(
            super::sanitize_agent_response(
                "You can think of me as an AI system built by Elixpo. My role is to assist with codebase mapping and failure diagnosis. If you have specific tasks in mind, please let me know.",
                "what are you doing?"
            ),
            "Ready when you are—give me something interesting."
        );
        assert_eq!(
            super::sanitize_agent_response(
                "I am an AI system. I don't have a \"name\" in the human sense.",
                "do you know ur name?"
            ),
            "I’m Crumb, built by Elixpo."
        );
        let markdown = "Result:\n\n```rust\nvalue.method();\n```";
        assert_eq!(
            super::sanitize_agent_response(markdown, "show the code"),
            markdown
        );
    }

    #[test]
    fn web_search_preserves_content_terms_dropped_by_the_model() {
        let mut call = ToolCall {
            id: "search_1".to_owned(),
            name: "web_search".to_owned(),
            arguments: json!({"query": "Elixpo overview"}),
        };

        super::preserve_web_search_terms(
            &mut call,
            "search about ayushman bhattacharya elixpo and tell me what you find in short",
        );

        assert_eq!(
            call.arguments["query"].as_str(),
            Some("Elixpo overview ayushman bhattacharya")
        );
    }

    #[test]
    fn web_search_does_not_append_question_filler() {
        let mut call = ToolCall {
            id: "search_2".to_owned(),
            name: "web_search".to_owned(),
            arguments: json!({"query": "Ayushman Bhattachaya Elixpo public information"}),
        };

        super::preserve_web_search_terms(
            &mut call,
            "what do you know about ayushman bhattachaya elixpo",
        );

        assert_eq!(
            call.arguments["query"].as_str(),
            Some("Ayushman Bhattachaya Elixpo public information")
        );
    }

    #[test]
    fn weather_query_does_not_inherit_conversational_decision_words() {
        let mut call = ToolCall {
            id: "search_3".to_owned(),
            name: "web_search".to_owned(),
            arguments: json!({"query": "please search weather forecast for Kolkata tomorrow"}),
        };

        super::preserve_web_search_terms(
            &mut call,
            "I was thinking, do I need an umbrella in Kolkata tomorrow?",
        );

        assert_eq!(
            call.arguments["query"].as_str(),
            Some("weather forecast for Kolkata tomorrow")
        );
    }

    #[test]
    fn web_search_query_has_a_bounded_term_budget() {
        let mut call = ToolCall {
            id: "search_4".to_owned(),
            name: "web_search".to_owned(),
            arguments: json!({
                "query": "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen"
            }),
        };

        super::preserve_web_search_terms(&mut call, "");

        assert_eq!(
            call.arguments["query"]
                .as_str()
                .expect("query remains text")
                .split_whitespace()
                .count(),
            16
        );
    }

    #[test]
    fn visible_tool_activity_includes_only_safe_transient_fields() {
        assert_eq!(
            super::tool_request_detail(
                "run_shell",
                &serde_json::json!({"command":"cargo test\n--workspace"})
            )
            .as_deref(),
            Some("cargo test --workspace")
        );
        assert_eq!(
            super::tool_request_detail(
                "web_search",
                &serde_json::json!({"query":"current Rust release"})
            )
            .as_deref(),
            Some("current Rust release")
        );
        assert_eq!(
            super::tool_request_detail(
                "write_file",
                &serde_json::json!({"path":"secret.txt", "content":"never expose this"})
            )
            .as_deref(),
            Some("secret.txt")
        );
        assert_eq!(
            super::tool_request_detail(
                "run_disposable",
                &serde_json::json!({"script":"echo never-expose-this"})
            ),
            None
        );
    }

    #[tokio::test]
    async fn denied_tool_never_reaches_its_handler() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_round(),
            answer_round(),
            vec![
                ChatEvent::TextDelta(
                    "The weather lookup failed because permission was denied.".to_owned(),
                ),
                ChatEvent::Finished(FinishReason::Stop),
            ],
        ]));
        let calls = Arc::new(AtomicUsize::new(0));
        let harness = NativeHarness::new(
            provider.clone(),
            tools(calls.clone()),
            Arc::new(DenyAllApprovals),
            HarnessLimits::default(),
        );
        let (root, mut session) = session(AgentMode::Negotiate);
        let mut conversation = harness.conversation();

        harness
            .run_turn(
                &mut session,
                &mut conversation,
                turn(),
                &CancellationToken::default(),
                &mut Vec::new(),
            )
            .await
            .expect("model can recover from denial");

        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let journal = export_session(root.path(), "harness-test").expect("session exports");
        assert!(journal.events.iter().any(|event| matches!(
            event,
            crumb_agent::SessionEvent::ToolFinished {
                name,
                success: false,
                output_bytes,
                ..
            } if name == "weather" && *output_bytes <= 64
        )));
        let requests = provider.requests.lock().expect("request lock");
        assert!(
            requests[1]
                .messages
                .last()
                .expect("denial result")
                .content
                .contains("denied")
        );
        assert_eq!(requests[2].tool_choice, ToolChoice::None);
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_pending_stream() {
        let provider = Arc::new(ScriptedProvider::pending());
        let harness = NativeHarness::new(
            provider,
            ToolHost::default(),
            Arc::new(DenyAllApprovals),
            HarnessLimits::default(),
        );
        let (_root, mut session) = session(AgentMode::Auto);
        let mut conversation =
            Conversation::new(NonZeroUsize::new(2).expect("constant is non-zero"));
        let cancellation = CancellationToken::default();
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            trigger.cancel();
        });

        let error = harness
            .run_turn(
                &mut session,
                &mut conversation,
                turn(),
                &cancellation,
                &mut Vec::new(),
            )
            .await
            .expect_err("turn is cancelled");

        assert_eq!(error.kind, HarnessErrorKind::Cancelled);
    }

    #[tokio::test]
    async fn deadline_interrupts_a_pending_stream() {
        let provider = Arc::new(ScriptedProvider::pending());
        let limits = HarnessLimits {
            turn_timeout: Duration::from_millis(10),
            ..HarnessLimits::default()
        };
        let harness = NativeHarness::new(
            provider,
            ToolHost::default(),
            Arc::new(DenyAllApprovals),
            limits,
        );
        let (_root, mut session) = session(AgentMode::Auto);
        let mut conversation = harness.conversation();

        let error = harness
            .run_turn(
                &mut session,
                &mut conversation,
                turn(),
                &CancellationToken::default(),
                &mut Vec::new(),
            )
            .await
            .expect_err("turn times out");

        assert_eq!(error.kind, HarnessErrorKind::DeadlineExceeded);
    }

    #[tokio::test]
    async fn output_limit_stops_an_oversized_stream() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ChatEvent::TextDelta("too long".to_owned()),
            ChatEvent::Finished(FinishReason::Stop),
        ]]));
        let limits = HarnessLimits {
            max_output_bytes: NonZeroUsize::new(4).expect("constant is non-zero"),
            ..HarnessLimits::default()
        };
        let harness = NativeHarness::new(
            provider,
            ToolHost::default(),
            Arc::new(DenyAllApprovals),
            limits,
        );
        let (_root, mut session) = session(AgentMode::Auto);
        let mut conversation = harness.conversation();

        let error = harness
            .run_turn(
                &mut session,
                &mut conversation,
                turn(),
                &CancellationToken::default(),
                &mut Vec::new(),
            )
            .await
            .expect_err("oversized output is rejected");

        assert_eq!(error.kind, HarnessErrorKind::OutputLimit);
    }

    #[test]
    fn conversation_retains_only_its_configured_turn_bound() {
        let mut conversation =
            Conversation::new(NonZeroUsize::new(1).expect("constant is non-zero"));
        conversation.commit("first".to_owned(), "one".to_owned(), Vec::new());
        conversation.commit("second".to_owned(), "two".to_owned(), Vec::new());

        assert_eq!(conversation.len(), 1);
        assert_eq!(conversation.messages()[0].content, "second");
    }

    #[test]
    fn conversation_prompt_compacts_each_turn_without_dropping_the_sequence() {
        let mut conversation = Conversation::new(NonZeroUsize::new(3).expect("non-zero"));
        conversation.commit("old".repeat(5_000), "answer".repeat(5_000), Vec::new());
        conversation.commit("latest".to_owned(), "kept".to_owned(), Vec::new());

        let messages = conversation.messages();

        assert_eq!(conversation.len(), 2);
        assert_eq!(messages.len(), 4);
        assert!(
            messages[0]
                .content
                .contains("earlier turn content compacted")
        );
        assert!(
            messages[1]
                .content
                .contains("earlier turn content compacted")
        );
        assert_eq!(messages[2].content, "latest");
        assert_eq!(messages[3].content, "kept");
        assert!(
            messages
                .iter()
                .map(|message| message.content.len())
                .sum::<usize>()
                <= super::MAX_HISTORY_BYTES
        );
    }

    #[test]
    fn authoritative_action_facts_survive_turn_eviction() {
        let mut conversation = Conversation::new(NonZeroUsize::MIN);
        conversation.commit(
            "create it".to_owned(),
            "done".to_owned(),
            vec!["Created ~/test_folder.".to_owned()],
        );
        conversation.commit(
            "now work inside it".to_owned(),
            "okay".to_owned(),
            Vec::new(),
        );

        let messages = conversation.messages();

        assert_eq!(conversation.len(), 1);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "now work inside it");
        assert!(messages[2].content.contains("Created ~/test_folder."));
    }

    #[test]
    fn conversation_compaction_preserves_utf8_boundaries() {
        let compacted = super::compact_history_message(&"🦀".repeat(100), 96);

        assert!(compacted.len() <= 96);
        assert!(compacted.contains("earlier turn content compacted"));
        assert!(compacted.starts_with('🦀'));
        assert!(compacted.ends_with('🦀'));
    }

    #[test]
    fn successful_directory_creation_is_retained_as_an_authoritative_path_anchor() {
        let call = ToolCall {
            id: "mkdir-1".to_owned(),
            name: "run_shell".to_owned(),
            arguments: json!({"command":"mkdir ~/test_folder"}),
        };
        let fact = super::safe_successful_action_fact(&call).expect("safe mkdir is retained");
        let mut conversation = Conversation::new(NonZeroUsize::MIN);
        conversation.commit(
            "create a folder in ~/".to_owned(),
            "Created it in your home directory.".to_owned(),
            vec![fact],
        );

        let messages = conversation.messages();
        assert_eq!(messages.len(), 3);
        assert!(messages[2].content.contains("Created ~/test_folder."));
        assert!(messages[2].content.contains("do not relocate"));
    }

    #[test]
    fn compound_shell_commands_are_not_retained_as_cross_turn_facts() {
        let call = ToolCall {
            id: "mkdir-1".to_owned(),
            name: "run_shell".to_owned(),
            arguments: json!({"command":"mkdir safe; printf secret"}),
        };

        assert_eq!(super::safe_successful_action_fact(&call), None);
    }

    #[test]
    fn successful_workspace_writes_emit_file_change_events() {
        let call = ToolCall {
            id: "write-1".to_owned(),
            name: "write_file".to_owned(),
            arguments: json!({}),
        };
        let result = super::ExecutedTool {
            content: "wrote source.rs".to_owned(),
            structured: Some(json!({
                "path": "source.rs",
                "additions": 4,
                "deletions": 1
            })),
            success: true,
        };
        let mut events = Vec::new();

        super::emit_tool_output(&mut events, &call, &result);

        assert!(events.contains(&HarnessEvent::FileChanged {
            path: "source.rs".to_owned(),
            additions: 4,
            deletions: 1,
        }));
    }
}
