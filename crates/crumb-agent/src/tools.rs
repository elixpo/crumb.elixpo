//! Provider-neutral tools, MCP transports, and output optimization contracts.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{AgentMode, CancellationToken};

/// Deterministic risk assigned by Rust-owned tool metadata.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    ReadOnly,
    WriteWorkspace,
    ProcessExecution,
    NetworkAccess,
    SystemMutation,
    CredentialSensitive,
    Destructive,
}

/// External MCP server process configuration. Credentials must come from the
/// process environment or OS credential store, never this structure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpServer {
    pub id: String,
    pub command: PathBuf,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub enabled: bool,
}

/// Transport that owns execution for a tool.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolTransport {
    Native,
    Mcp { server_id: String },
}

/// Model-visible tool metadata with Rust-owned execution policy.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub risk: RiskClass,
    pub transport: ToolTransport,
}

/// Deterministic tool registry; registration order cannot affect lookup.
#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolDescriptor>,
}

/// Content returned to the model after policy-approved execution.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    pub structured: Option<serde_json::Value>,
    pub is_error: bool,
}

impl ToolOutput {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            structured: None,
            is_error: false,
        }
    }

    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            text: message.into(),
            structured: None,
            is_error: true,
        }
    }
}

/// Metadata shown to a trusted approval UI. Raw arguments are deliberately
/// replaced with a digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub risk: RiskClass,
    pub arguments_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalDecision {
    AllowOnce,
    Deny,
}

/// User-owned approval boundary. Model output cannot implement this trait.
pub trait ApprovalBroker: Send + Sync {
    fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision;
}

/// Safe default used when no interactive approval channel exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllApprovals;

impl ApprovalBroker for DenyAllApprovals {
    fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

/// Native implementation of one registered tool.
pub trait ToolHandler: Send + Sync {
    /// Executes with already-authorized arguments.
    ///
    /// # Errors
    ///
    /// Returns only redacted internal failures. Expected command failures
    /// should use `ToolOutput::error` so the model can self-correct.
    fn call(
        &self,
        arguments: &serde_json::Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallErrorKind {
    UnknownTool,
    Denied,
    Cancelled,
    Internal,
}

/// Redacted error safe to translate to an MCP response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCallError {
    pub kind: ToolCallErrorKind,
    message: String,
}

impl ToolCallError {
    fn new(kind: ToolCallErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ToolCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ToolCallError {}

/// Registry plus Rust-owned policy and native implementations.
#[derive(Default)]
pub struct ToolHost {
    registry: ToolRegistry,
    handlers: BTreeMap<String, Arc<dyn ToolHandler>>,
}

impl ToolHost {
    /// Registers metadata and its implementation atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or duplicate descriptor.
    pub fn register(
        &mut self,
        descriptor: ToolDescriptor,
        handler: Arc<dyn ToolHandler>,
    ) -> Result<()> {
        self.registry.register(descriptor.clone())?;
        self.handlers.insert(descriptor.name, handler);
        Ok(())
    }

    pub fn tools(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.registry.iter()
    }

    /// Authorizes and executes one tool call.
    ///
    /// # Errors
    ///
    /// Returns a typed, redacted error when the tool is unknown, denied,
    /// cancelled, or fails internally.
    pub fn call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        mode: AgentMode,
        approvals: &dyn ApprovalBroker,
        cancellation: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolCallError> {
        let descriptor = self
            .registry
            .get(name)
            .ok_or_else(|| ToolCallError::new(ToolCallErrorKind::UnknownTool, "unknown tool"))?;
        if cancellation.is_cancelled() {
            return Err(ToolCallError::new(
                ToolCallErrorKind::Cancelled,
                "tool call cancelled",
            ));
        }
        authorize(descriptor, arguments, mode, approvals)?;
        let handler = self.handlers.get(name).ok_or_else(|| {
            ToolCallError::new(
                ToolCallErrorKind::Internal,
                "tool implementation unavailable",
            )
        })?;
        let output = match handler.call(arguments, cancellation) {
            Ok(output) => output,
            Err(_) if cancellation.is_cancelled() => {
                return Err(ToolCallError::new(
                    ToolCallErrorKind::Cancelled,
                    "tool call cancelled",
                ));
            }
            Err(_) => {
                return Err(ToolCallError::new(
                    ToolCallErrorKind::Internal,
                    "tool execution failed",
                ));
            }
        };
        if cancellation.is_cancelled() {
            return Err(ToolCallError::new(
                ToolCallErrorKind::Cancelled,
                "tool call cancelled",
            ));
        }
        Ok(output)
    }
}

fn authorize(
    descriptor: &ToolDescriptor,
    arguments: &serde_json::Value,
    mode: AgentMode,
    approvals: &dyn ApprovalBroker,
) -> std::result::Result<(), ToolCallError> {
    if matches!(mode, AgentMode::Plan) {
        return Err(ToolCallError::new(
            ToolCallErrorKind::Denied,
            "plan mode does not execute tools",
        ));
    }
    if matches!(mode, AgentMode::Auto) && matches!(descriptor.risk, RiskClass::ReadOnly) {
        return Ok(());
    }
    let encoded = serde_json::to_vec(arguments).map_err(|_| {
        ToolCallError::new(
            ToolCallErrorKind::Internal,
            "tool arguments could not be validated",
        )
    })?;
    let request = ApprovalRequest {
        tool: descriptor.name.clone(),
        risk: descriptor.risk,
        arguments_digest: crate::session::digest(&encoded),
    };
    match approvals.decide(&request) {
        ApprovalDecision::AllowOnce => Ok(()),
        ApprovalDecision::Deny => Err(ToolCallError::new(
            ToolCallErrorKind::Denied,
            "tool call denied by policy",
        )),
    }
}

impl ToolRegistry {
    /// Registers one uniquely named tool.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or duplicate name.
    pub fn register(&mut self, tool: ToolDescriptor) -> Result<()> {
        if tool.name.trim().is_empty() {
            bail!("tool names cannot be empty");
        }
        if self.tools.contains_key(&tool.name) {
            bail!("tool `{}` is already registered", tool.name);
        }
        self.tools.insert(tool.name.clone(), tool);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.tools.values()
    }
}

/// Output category used by command-aware optimizers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputKind {
    Generic,
    Cargo,
    GitDiff,
    PackageInstall,
    Test,
}

/// Token optimizer contract. Implementations must preserve critical errors and
/// return the original bytes when unavailable.
pub trait TokenOptimizer: Send + Sync {
    fn name(&self) -> &str;
    fn available(&self) -> bool;

    /// Compresses already-redacted output to a byte budget.
    ///
    /// # Errors
    ///
    /// Returns an error when an available optimizer fails. Callers must fall
    /// back to the original input instead of blocking agent execution.
    fn optimize(&self, kind: OutputKind, input: &[u8], budget: usize) -> Result<Vec<u8>>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;

    use crate::{AgentMode, CancellationToken};

    use super::{
        ApprovalBroker, ApprovalDecision, ApprovalRequest, DenyAllApprovals, RiskClass,
        ToolCallErrorKind, ToolDescriptor, ToolHandler, ToolHost, ToolOutput, ToolRegistry,
        ToolTransport,
    };

    #[test]
    fn duplicate_tools_are_rejected_deterministically() {
        let mut registry = ToolRegistry::default();
        let tool = ToolDescriptor {
            name: "read_file".to_owned(),
            description: "Read a workspace file".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            risk: RiskClass::ReadOnly,
            transport: ToolTransport::Native,
        };
        registry
            .register(tool.clone())
            .expect("first registration should pass");
        assert!(registry.register(tool).is_err());
    }

    #[test]
    fn auto_allows_read_only_tools_without_an_approval() {
        let mut host = ToolHost::default();
        host.register(descriptor(RiskClass::ReadOnly), Arc::new(FixtureHandler))
            .expect("registration succeeds");
        let output = host
            .call(
                "fixture",
                &serde_json::json!({}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &CancellationToken::default(),
            )
            .expect("read-only auto call is allowed");
        assert_eq!(output.text, "called");
    }

    #[test]
    fn mutating_tools_require_user_owned_approval() {
        let mut host = ToolHost::default();
        host.register(
            descriptor(RiskClass::WriteWorkspace),
            Arc::new(FixtureHandler),
        )
        .expect("registration succeeds");
        let denied = host
            .call(
                "fixture",
                &serde_json::json!({"path":"file"}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &CancellationToken::default(),
            )
            .expect_err("missing approval denies the call");
        assert_eq!(denied.kind, ToolCallErrorKind::Denied);

        let allowed = host.call(
            "fixture",
            &serde_json::json!({"path":"file"}),
            AgentMode::Auto,
            &AllowOnce,
            &CancellationToken::default(),
        );
        assert!(allowed.is_ok());
    }

    #[test]
    fn plan_mode_never_calls_tools() {
        let mut host = ToolHost::default();
        host.register(descriptor(RiskClass::ReadOnly), Arc::new(FixtureHandler))
            .expect("registration succeeds");
        let error = host
            .call(
                "fixture",
                &serde_json::json!({}),
                AgentMode::Plan,
                &AllowOnce,
                &CancellationToken::default(),
            )
            .expect_err("plan mode denies tools");
        assert_eq!(error.kind, ToolCallErrorKind::Denied);
    }

    fn descriptor(risk: RiskClass) -> ToolDescriptor {
        ToolDescriptor {
            name: "fixture".to_owned(),
            description: "Fixture tool".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
            risk,
            transport: ToolTransport::Native,
        }
    }

    struct FixtureHandler;

    impl ToolHandler for FixtureHandler {
        fn call(
            &self,
            _arguments: &serde_json::Value,
            _cancellation: &CancellationToken,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput::text("called"))
        }
    }

    struct AllowOnce;

    impl ApprovalBroker for AllowOnce {
        fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
            ApprovalDecision::AllowOnce
        }
    }
}
