//! Native, provider-neutral foundations for Crumb agent execution.
//!
//! This crate owns deterministic routing, live configuration, session events,
//! tool metadata, and token-optimization contracts. It performs no network
//! requests and is not part of native-shell startup.

pub mod config;
pub mod harness;
pub mod routing;
pub mod session;
pub mod tools;

pub use config::{
    AgentConfig, AgentLimits, AgentMode, HarnessConfig, LiveConfig, MistakePolicy, Modality,
    ModelRoute, StructuredEncoding,
};
pub use harness::{HarnessTurnRequest, ModelCapabilities};
pub use routing::{
    CommandCatalog, InputRoute, RouteDecision, RoutePolicy, RouteReason, UnknownInputPolicy,
};
pub use session::{AgentSession, CancellationToken, SessionEvent, SessionId, SessionJournal};
pub use tools::{
    ApprovalBroker, ApprovalDecision, ApprovalRequest, DenyAllApprovals, McpServer, OutputKind,
    RiskClass, TokenOptimizer, ToolCallError, ToolCallErrorKind, ToolDescriptor, ToolHandler,
    ToolHost, ToolOutput, ToolRegistry, ToolTransport,
};
