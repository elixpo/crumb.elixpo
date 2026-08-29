//! Provider-neutral tools, MCP transports, and output optimization contracts.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

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
    use super::{RiskClass, ToolDescriptor, ToolRegistry, ToolTransport};

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
}
