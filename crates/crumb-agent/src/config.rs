//! Live, user-editable agent configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{CodingBackend, ModelCapabilities};

/// Controls how much autonomy an agent receives for a turn.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Execute policy-approved steps without pausing for harmless operations.
    #[default]
    Auto,
    /// Present tool choices and negotiate actions with the user.
    Negotiate,
    /// Produce a plan without executing tools.
    Plan,
}

/// Controls optional AI assistance after a native command fails.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MistakePolicy {
    /// Render deterministic diagnostics and let the user request AI help.
    #[default]
    Prompt,
    /// Start cancellable AI diagnosis automatically after deterministic checks.
    Automatic,
    /// Never offer AI assistance for native command failures.
    Disabled,
}

/// Encoding used when structured context is sent to a model.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredEncoding {
    Json,
    Toon,
    /// Select TOON only after a deterministic size comparison.
    #[default]
    Auto,
}

/// Replaceable agent-loop implementation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessConfig {
    Native,
    Process {
        command: PathBuf,
        #[serde(default)]
        arguments: Vec<String>,
        #[serde(default)]
        cordis: Option<PathBuf>,
    },
    CodingCli {
        backend: CodingBackend,
        command: PathBuf,
        capabilities: Vec<ModelCapabilities>,
    },
}

/// Provider-neutral generation modality.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    WebSearch,
    Image,
    Video,
    Audio,
    ThreeD,
    Transcription,
    Embeddings,
}

/// One configurable provider/model route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRoute {
    pub provider: String,
    pub model: String,
    /// Adapter-defined exact-model effort identifier, such as `high` or `max`.
    #[serde(default, rename = "effort", alias = "reasoning_effort")]
    pub reasoning_effort: Option<String>,
}

/// Hard runtime ceilings enforced outside model prompts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AgentLimits {
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_wall_time_seconds: u64,
    pub max_context_tokens: u64,
    pub max_output_bytes: u64,
    pub max_directory_entries: u64,
    pub max_harness_initialize_seconds: u64,
    pub max_harness_shutdown_seconds: u64,
    pub max_shell_command_seconds: u64,
    pub max_file_write_bytes: u64,
    pub max_steering_messages: u32,
    pub max_steering_bytes: u64,
    pub max_activity_events: u32,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_steps: 24,
            max_tool_calls: 48,
            max_wall_time_seconds: 900,
            max_context_tokens: 64_000,
            max_output_bytes: 1_048_576,
            max_directory_entries: 4_096,
            max_harness_initialize_seconds: 30,
            max_harness_shutdown_seconds: 5,
            max_shell_command_seconds: 300,
            max_file_write_bytes: 1_048_576,
            max_steering_messages: 8,
            max_steering_bytes: 32_768,
            max_activity_events: 128,
        }
    }
}

/// Configuration for a discoverable skill.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillConfig {
    pub id: String,
    pub path: PathBuf,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

const fn enabled() -> bool {
    true
}

/// Optional output optimizer executable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OptimizerConfig {
    pub id: String,
    pub command: PathBuf,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

/// User-owned grants for tools that cross a trust boundary.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolPermissions {
    /// Network tools allowed without an interactive approval bridge.
    pub allow_network_tools: BTreeSet<String>,
    /// Workspace-write tools explicitly enabled by the user.
    pub allow_workspace_tools: BTreeSet<String>,
}

/// Complete live configuration. Secrets are deliberately not representable.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub mode: AgentMode,
    pub mistakes: MistakePolicy,
    pub routing: crate::routing::RoutePolicy,
    pub structured_encoding: StructuredEncoding,
    /// Session preference used only when the selected model advertises it.
    #[serde(rename = "effort", alias = "reasoning_effort")]
    pub reasoning_effort: Option<String>,
    pub limits: AgentLimits,
    pub harness: Option<HarnessConfig>,
    pub models: BTreeMap<Modality, Vec<ModelRoute>>,
    pub skills: Vec<SkillConfig>,
    pub mcp_servers: Vec<crate::tools::McpServer>,
    pub optimizers: Vec<OptimizerConfig>,
    pub permissions: ToolPermissions,
}

impl AgentConfig {
    /// Rejects incomplete routes and unsafe executable definitions.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured identifier or required value is empty.
    pub fn validate(&self) -> Result<()> {
        for routes in self.models.values() {
            for route in routes {
                if route.provider.trim().is_empty() || route.model.trim().is_empty() {
                    bail!("model routes require non-empty provider and model identifiers");
                }
                validate_effort(route.reasoning_effort.as_deref())?;
            }
        }
        validate_effort(self.reasoning_effort.as_deref())?;
        if self.limits.max_output_bytes == 0
            || self.limits.max_directory_entries == 0
            || self.limits.max_wall_time_seconds == 0
            || self.limits.max_harness_initialize_seconds == 0
            || self.limits.max_harness_shutdown_seconds == 0
            || self.limits.max_steering_messages == 0
            || self.limits.max_steering_bytes == 0
            || self.limits.max_activity_events == 0
        {
            bail!("agent runtime limits must be positive");
        }
        if self.skills.iter().any(|skill| skill.id.trim().is_empty()) {
            bail!("skill identifiers cannot be empty");
        }
        match &self.harness {
            Some(HarnessConfig::Process { command, .. }) if command.as_os_str().is_empty() => {
                bail!("process harness requires a command");
            }
            Some(HarnessConfig::CodingCli {
                command,
                capabilities,
                ..
            }) => self.validate_coding_cli(command, capabilities)?,
            _ => {}
        }
        if self
            .mcp_servers
            .iter()
            .any(|server| server.id.trim().is_empty() || server.command.as_os_str().is_empty())
        {
            bail!("MCP servers require an identifier and command");
        }
        if self.optimizers.iter().any(|optimizer| {
            optimizer.id.trim().is_empty() || optimizer.command.as_os_str().is_empty()
        }) {
            bail!("optimizers require an identifier and command");
        }
        if self
            .permissions
            .allow_network_tools
            .iter()
            .chain(&self.permissions.allow_workspace_tools)
            .any(|tool| !valid_identifier(tool))
        {
            bail!("tool permission entries must be valid identifiers");
        }
        Ok(())
    }

    /// Resolves a per-model override before the session default.
    #[must_use]
    pub fn reasoning_effort_for<'a>(&'a self, route: &'a ModelRoute) -> Option<&'a str> {
        route
            .reasoning_effort
            .as_deref()
            .or(self.reasoning_effort.as_deref())
    }

    fn validate_coding_cli(
        &self,
        command: &Path,
        capabilities: &[ModelCapabilities],
    ) -> Result<()> {
        if command.as_os_str().is_empty() {
            bail!("coding CLI harness requires an explicit command");
        }
        let routes = self
            .models
            .get(&Modality::Text)
            .filter(|routes| routes.len() == 1)
            .context("coding CLI harness requires exactly one explicit text model route")?;
        let route = &routes[0];
        let mut identities = BTreeSet::new();
        for capability in capabilities {
            if capability.provider.trim().is_empty() || capability.model.trim().is_empty() {
                bail!("coding CLI capabilities require provider and model identifiers");
            }
            if !identities.insert((&capability.provider, &capability.model)) {
                bail!("coding CLI capabilities cannot contain duplicate models");
            }
            for effort in &capability.reasoning_efforts {
                validate_effort(Some(effort))?;
            }
        }
        let capability = capabilities
            .iter()
            .find(|capability| {
                capability.provider == route.provider && capability.model == route.model
            })
            .context("selected coding CLI model is not present in its capability metadata")?;
        if let Some(effort) = self.reasoning_effort_for(route)
            && !capability
                .reasoning_efforts
                .iter()
                .any(|supported| supported == effort)
        {
            bail!("selected coding CLI model does not support effort `{effort}`");
        }
        Ok(())
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn validate_effort(effort: Option<&str>) -> Result<()> {
    if let Some(effort) = effort
        && (effort.is_empty()
            || effort.len() > 32
            || !effort.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            }))
    {
        bail!("reasoning effort must be a 1-32 character adapter-defined identifier");
    }
    Ok(())
}

/// Reloads the configuration file for every new turn, making edits immediately live.
#[derive(Clone, Debug)]
pub struct LiveConfig {
    path: PathBuf,
}

impl LiveConfig {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads and validates the latest file contents.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, parsed, or validated.
    pub fn load(&self) -> Result<AgentConfig> {
        let bytes = fs::read(&self.path)
            .with_context(|| format!("failed to read agent config at {}", self.path.display()))?;
        let config: AgentConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse agent config at {}", self.path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Loads the file when present, otherwise returns safe defaults.
    ///
    /// # Errors
    ///
    /// Returns an error only when an existing file is invalid or unreadable.
    pub fn load_or_default(&self) -> Result<AgentConfig> {
        if self.path.exists() {
            self.load()
        } else {
            Ok(AgentConfig::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{AgentConfig, AgentMode, HarnessConfig, MistakePolicy, Modality, ModelRoute};
    use crate::{CodingBackend, ModelCapabilities};

    #[test]
    fn model_routes_are_data_driven() {
        let mut models = BTreeMap::new();
        models.insert(
            Modality::Text,
            vec![ModelRoute {
                provider: "fixture".to_owned(),
                model: "fixture-text".to_owned(),
                reasoning_effort: Some("high".to_owned()),
            }],
        );
        let config = AgentConfig {
            mode: AgentMode::Negotiate,
            mistakes: MistakePolicy::Automatic,
            models,
            ..AgentConfig::default()
        };

        assert!(config.validate().is_ok());
        assert_eq!(config.models[&Modality::Text][0].model, "fixture-text");
        assert_eq!(
            config.reasoning_effort_for(&config.models[&Modality::Text][0]),
            Some("high")
        );
    }

    #[test]
    fn secrets_are_not_part_of_the_config_schema() {
        let result = serde_json::from_str::<AgentConfig>(r#"{"api_key":"secret"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn coding_cli_requires_one_explicit_supported_model_and_effort() {
        let mut models = BTreeMap::new();
        models.insert(
            Modality::Text,
            vec![ModelRoute {
                provider: "openai".to_owned(),
                model: "fixture-codex".to_owned(),
                reasoning_effort: Some("high".to_owned()),
            }],
        );
        let config = AgentConfig {
            harness: Some(HarnessConfig::CodingCli {
                backend: CodingBackend::Codex,
                command: "codex".into(),
                capabilities: vec![ModelCapabilities {
                    provider: "openai".to_owned(),
                    model: "fixture-codex".to_owned(),
                    reasoning_efforts: vec!["low".to_owned(), "high".to_owned()],
                }],
            }),
            models,
            ..AgentConfig::default()
        };
        assert!(config.validate().is_ok());

        let mut unsupported = config;
        unsupported.models.get_mut(&Modality::Text).expect("route")[0].reasoning_effort =
            Some("max".to_owned());
        assert!(unsupported.validate().is_err());
    }
}
