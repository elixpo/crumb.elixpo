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
    ChatEvent, ChatMessage, ChatRequest, ChatRole, FinishReason, LlmProvider, ProviderError,
    TokenUsage, ToolCall, ToolChoice, ToolDefinition,
};
use tokio::time::Instant;

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
    pub persona: Option<String>,
    pub trusted_context: Vec<String>,
    pub max_output_tokens: Option<u32>,
}

/// Transient event vocabulary emitted to a trusted caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessEvent {
    TurnStarted,
    TextDelta(String),
    ToolRequested {
        id: String,
        name: String,
    },
    ToolFinished {
        id: String,
        name: String,
        success: bool,
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

/// In-memory bounded conversation state. It is never persisted by this crate.
#[derive(Clone, Debug)]
pub struct Conversation {
    max_turns: NonZeroUsize,
    turns: VecDeque<ConversationTurn>,
}

impl Conversation {
    #[must_use]
    pub fn new(max_turns: NonZeroUsize) -> Self {
        Self {
            max_turns,
            turns: VecDeque::with_capacity(max_turns.get()),
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
        self.turns
            .iter()
            .flat_map(|turn| {
                [
                    ChatMessage::text(ChatRole::User, turn.user.clone()),
                    ChatMessage::text(ChatRole::Assistant, turn.assistant.clone()),
                ]
            })
            .collect()
    }

    fn commit(&mut self, user: String, assistant: String) {
        if self.turns.len() == self.max_turns.get() {
            self.turns.pop_front();
        }
        self.turns.push_back(ConversationTurn { user, assistant });
    }
}

/// Successful turn result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessOutcome {
    pub text: String,
    pub usage: TokenUsage,
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
}

struct ModelRound {
    text: String,
    calls: Vec<ToolCall>,
    finish: Option<FinishReason>,
    usage: TokenUsage,
}

/// Provider-neutral harness with Rust-owned tools and approval policy.
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
        messages.push(ChatMessage::text(ChatRole::User, turn.user_message.clone()));
        let definitions = self
            .tools
            .tools()
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            })
            .collect::<Vec<_>>();
        let mut text = String::new();
        let mut usage = TokenUsage::default();
        let mut output_bytes = 0_usize;

        loop {
            if metrics.model_rounds == self.limits.max_model_rounds.get() {
                return Err(HarnessError::new(
                    HarnessErrorKind::ModelRoundLimit,
                    "model round limit reached",
                ));
            }
            metrics.model_rounds += 1;
            let request = ChatRequest {
                model: turn.model.clone(),
                messages: messages.clone(),
                tools: definitions.clone(),
                tool_choice: if definitions.is_empty() {
                    ToolChoice::None
                } else {
                    ToolChoice::Auto
                },
                max_output_tokens: turn.max_output_tokens,
            };
            let round = self
                .run_model_round(request, cancellation, deadline, sink, &mut output_bytes)
                .await?;
            usage.input_tokens = usage.input_tokens.saturating_add(round.usage.input_tokens);
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(round.usage.output_tokens);
            text.push_str(&round.text);

            if round.calls.is_empty() {
                match round.finish {
                    Some(FinishReason::Stop | FinishReason::Other(_)) => {
                        conversation.commit(turn.user_message, text.clone());
                        return Ok(HarnessOutcome {
                            text,
                            usage,
                            model_rounds: metrics.model_rounds,
                            tool_calls: metrics.tool_calls,
                        });
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

            messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content: round.text,
                tool_call_id: None,
                tool_calls: round.calls.clone(),
            });
            for call in round.calls {
                if metrics.tool_calls == self.limits.max_tool_calls.get() {
                    return Err(HarnessError::new(
                        HarnessErrorKind::ToolCallLimit,
                        "tool call limit reached",
                    ));
                }
                metrics.tool_calls += 1;
                let result = self.execute_tool(session, &call, cancellation, deadline, sink)?;
                add_output_bytes(&mut output_bytes, result.len(), self.limits)?;
                messages.push(ChatMessage::tool_result(call.id, result));
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
                    sink.emit(HarnessEvent::TextDelta(delta));
                }
                ChatEvent::ToolCall(call) => round.calls.push(call),
                ChatEvent::Usage(usage) => {
                    round.usage.input_tokens =
                        round.usage.input_tokens.saturating_add(usage.input_tokens);
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
    ) -> Result<String, HarnessError> {
        ensure_active(cancellation, deadline)?;
        let risk = self
            .tools
            .descriptor(&call.name)
            .map_or(RiskClass::CredentialSensitive, |tool| tool.risk);
        let encoded = serde_json::to_vec(&call.arguments).map_err(|_| {
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
        });
        let result = self.tools.call(
            &call.name,
            &call.arguments,
            session.mode(),
            self.approvals.as_ref(),
            cancellation,
        );
        ensure_active(cancellation, deadline)?;
        let (content, success) = match result {
            Ok(output) => (output.text, !output.is_error),
            Err(error) if error.kind == ToolCallErrorKind::Cancelled => {
                return Err(HarnessError::new(
                    HarnessErrorKind::Cancelled,
                    "turn cancelled",
                ));
            }
            Err(error) => (error.to_string(), false),
        };
        session
            .record_tool_finish(call.name.clone(), success, content.len())
            .map_err(persistence_error)?;
        sink.emit(HarnessEvent::ToolFinished {
            id: call.id.clone(),
            name: call.name.clone(),
            success,
        });
        Ok(content)
    }
}

fn system_messages(turn: &HarnessTurn) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(persona) = turn
        .persona
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        messages.push(ChatMessage::text(
            ChatRole::System,
            format!("Persona:\n{persona}"),
        ));
    }
    if !turn.trusted_context.is_empty() {
        messages.push(ChatMessage::text(
            ChatRole::System,
            format!("Trusted context:\n{}", turn.trusted_context.join("\n\n")),
        ));
    }
    messages
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

fn provider_error(_error: ProviderError) -> HarnessError {
    HarnessError::new(HarnessErrorKind::Provider, "model provider failed")
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
        ChatEvent, ChatRequest, ChatStream, EmbeddingRequest, EmbeddingResponse, FinishReason,
        LlmProvider, ProviderError, ProviderErrorKind, ProviderFuture, TokenUsage, ToolCall,
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
            persona: Some("Be warm and brief.".to_owned()),
            trusted_context: vec!["Locale: en-IN".to_owned()],
            max_output_tokens: Some(64),
        }
    }

    fn tool_round() -> Vec<ChatEvent> {
        vec![
            ChatEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "weather".to_owned(),
                arguments: json!({"city":"Pune"}),
            }),
            ChatEvent::Finished(FinishReason::ToolCall),
        ]
    }

    fn answer_round() -> Vec<ChatEvent> {
        vec![
            ChatEvent::TextDelta("It is 24 C.".to_owned()),
            ChatEvent::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            }),
            ChatEvent::Finished(FinishReason::Stop),
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
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(conversation.len(), 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HarnessEvent::ToolFinished { success: true, .. }))
        );
        let requests = provider.requests.lock().expect("request lock");
        assert_eq!(requests[0].messages[0].role, crumb_llm::ChatRole::System);
        assert_eq!(
            requests[1].messages.last().expect("tool result").role,
            crumb_llm::ChatRole::Tool
        );
        drop(requests);
        let journal = export_session(root.path(), "harness-test").expect("session exports");
        let persisted = serde_json::to_string(&journal).expect("session serializes");
        for secret in ["weather in Pune", "Pune", "24 C", "Be warm and brief"] {
            assert!(!persisted.contains(secret));
        }
    }

    #[tokio::test]
    async fn denied_tool_never_reaches_its_handler() {
        let provider = Arc::new(ScriptedProvider::new(vec![tool_round(), answer_round()]));
        let calls = Arc::new(AtomicUsize::new(0));
        let harness = NativeHarness::new(
            provider.clone(),
            tools(calls.clone()),
            Arc::new(DenyAllApprovals),
            HarnessLimits::default(),
        );
        let (_root, mut session) = session(AgentMode::Negotiate);
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
        let requests = provider.requests.lock().expect("request lock");
        assert!(
            requests[1]
                .messages
                .last()
                .expect("denial result")
                .content
                .contains("denied")
        );
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
        conversation.commit("first".to_owned(), "one".to_owned());
        conversation.commit("second".to_owned(), "two".to_owned());

        assert_eq!(conversation.len(), 1);
        assert_eq!(conversation.messages()[0].content, "second");
    }
}
