//! Live, user-editable agent configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
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

/// Wire protocol spoken by a configurable Harness provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    OpenAiCompletions,
    OpenAiResponses,
    AnthropicMessages,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

/// Non-secret reference used to resolve a provider credential at turn time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialReference {
    Environment { name: String },
    Keyring { service: String, account: String },
}

/// Header material that is either explicitly public or resolved from an
/// environment variable. Secret header values cannot be represented directly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderHeader {
    Public { value: String },
    Environment { name: String },
}

/// Boolean compatibility switch understood by the provider adapter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityFlag {
    Store,
    DeveloperRole,
    ReasoningEffort,
    UsageInStreaming,
    StrictTools,
    Temperature,
}

/// OpenAI-compatible transport behavior advertised by a provider or model.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderCompatibility {
    pub flags: BTreeMap<CompatibilityFlag, bool>,
    pub max_tokens_field: Option<String>,
    pub thinking_format: Option<String>,
    pub cache_control_format: Option<String>,
}

/// Configurable retry policy owned by the provider adapter.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderRetryPolicy {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

/// Exact model metadata used for local compatibility checks.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModel {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub input: BTreeSet<Modality>,
    #[serde(default)]
    pub tool_calling: bool,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_efforts: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub compatibility: ProviderCompatibility,
}

impl ProviderModel {
    /// Creates explicit model metadata with no inferred capabilities.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            input: BTreeSet::new(),
            tool_calling: false,
            context_window: None,
            max_output_tokens: None,
            reasoning_efforts: BTreeMap::new(),
            compatibility: ProviderCompatibility::default(),
        }
    }
}

/// Provider-neutral endpoint configuration. It contains references and public
/// metadata only; credential and secret header values live outside this file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub display_name: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    #[serde(default)]
    pub credential: Option<CredentialReference>,
    #[serde(default)]
    pub headers: BTreeMap<String, ProviderHeader>,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    #[serde(default)]
    pub default_input: BTreeSet<Modality>,
    #[serde(default)]
    pub default_context_window: Option<u64>,
    #[serde(default)]
    pub default_max_output_tokens: Option<u64>,
    #[serde(default)]
    pub compatibility: ProviderCompatibility,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub thinking_budgets: BTreeMap<String, u64>,
    #[serde(default)]
    pub cache_retention: Option<CacheRetention>,
    #[serde(default)]
    pub transport: Option<ProviderTransport>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub websocket_connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub stream_idle_timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_request_image_bytes: Option<u64>,
    #[serde(default)]
    pub retry: ProviderRetryPolicy,
    #[serde(default)]
    pub pricing: BTreeMap<String, String>,
    #[serde(default)]
    pub optimizer: Option<String>,
}

impl ProviderConfig {
    /// Creates a provider with safe defaults and no credential material.
    #[must_use]
    pub fn new(
        display_name: impl Into<String>,
        protocol: ProviderProtocol,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            display_name: display_name.into(),
            protocol,
            base_url: base_url.into(),
            credential: None,
            headers: BTreeMap::new(),
            models: Vec::new(),
            default_input: BTreeSet::new(),
            default_context_window: None,
            default_max_output_tokens: None,
            compatibility: ProviderCompatibility::default(),
            reasoning: None,
            thinking_budgets: BTreeMap::new(),
            cache_retention: None,
            transport: None,
            timeout_ms: None,
            websocket_connect_timeout_ms: None,
            stream_idle_timeout_ms: None,
            max_request_image_bytes: None,
            retry: ProviderRetryPolicy::default(),
            pricing: BTreeMap::new(),
            optimizer: None,
        }
    }
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
    pub providers: BTreeMap<String, ProviderConfig>,
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
        for (id, provider) in &self.providers {
            validate_provider(id, provider)?;
        }
        for routes in self.models.values() {
            for route in routes {
                if route.provider.trim().is_empty() || route.model.trim().is_empty() {
                    bail!("model routes require non-empty provider and model identifiers");
                }
                validate_effort(route.reasoning_effort.as_deref())?;
                if let Some(provider) = self.providers.get(&route.provider) {
                    let model = provider
                        .models
                        .iter()
                        .find(|model| model.id == route.model)
                        .with_context(|| {
                            format!(
                                "selected model `{}` is not configured for provider `{}`",
                                route.model, route.provider
                            )
                        })?;
                    if let Some(effort) = self.reasoning_effort_for(route)
                        && !model.reasoning_efforts.contains_key(effort)
                    {
                        bail!(
                            "selected provider model `{}/{}` does not support effort `{effort}`",
                            route.provider,
                            route.model
                        );
                    }
                }
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

fn validate_provider(id: &str, provider: &ProviderConfig) -> Result<()> {
    if !valid_identifier(id) || provider.display_name.trim().is_empty() {
        bail!("providers require a valid identifier and display name");
    }
    let endpoint = url::Url::parse(&provider.base_url)
        .with_context(|| format!("provider `{id}` has an invalid base URL"))?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        bail!(
            "provider `{id}` base URL must be an HTTP origin or path without credentials, query, or fragment"
        );
    }
    if let Some(credential) = &provider.credential {
        let local = endpoint
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
        if endpoint.scheme() != "https" && !local {
            bail!("provider `{id}` credentials require HTTPS except on loopback");
        }
        validate_credential_reference(credential)?;
    }
    for (name, value) in &provider.headers {
        validate_provider_header(name, value)?;
    }
    for limit in [
        provider.default_context_window,
        provider.default_max_output_tokens,
        provider.timeout_ms,
        provider.websocket_connect_timeout_ms,
        provider.stream_idle_timeout_ms,
        provider.max_request_image_bytes,
    ] {
        if limit == Some(0) {
            bail!("provider `{id}` limits must be positive when configured");
        }
    }
    if provider.retry.max_retries > 0
        && (provider.retry.base_delay_ms == 0
            || provider.retry.max_delay_ms < provider.retry.base_delay_ms)
    {
        bail!("provider `{id}` retry delays must be positive and ordered");
    }
    validate_effort(provider.reasoning.as_deref())?;
    for (effort, budget) in &provider.thinking_budgets {
        validate_effort(Some(effort))?;
        if *budget == 0 {
            bail!("provider `{id}` thinking budgets must be positive");
        }
    }
    validate_compatibility(id, &provider.compatibility)?;
    let mut models = BTreeSet::new();
    for model in &provider.models {
        if model.id.trim().is_empty() || !models.insert(model.id.as_str()) {
            bail!("provider `{id}` models require unique non-empty identifiers");
        }
        if model.context_window == Some(0) || model.max_output_tokens == Some(0) {
            bail!("provider `{id}` model limits must be positive when configured");
        }
        for (effort, wire_value) in &model.reasoning_efforts {
            validate_effort(Some(effort))?;
            validate_effort(wire_value.as_deref())?;
            if wire_value.is_none() && effort != "off" {
                bail!("only the `off` reasoning effort may omit its wire value");
            }
        }
        validate_compatibility(id, &model.compatibility)?;
    }
    if let Some(optimizer) = &provider.optimizer
        && !valid_identifier(optimizer)
    {
        bail!("provider `{id}` optimizer must be a valid identifier");
    }
    for (metric, value) in &provider.pricing {
        let Ok(parsed) = value.parse::<f64>() else {
            bail!("provider `{id}` pricing entries require identifier keys and numeric values");
        };
        if !valid_identifier(metric) || !parsed.is_finite() || parsed < 0.0 {
            bail!("provider `{id}` pricing entries require identifier keys and numeric values");
        }
    }
    Ok(())
}

fn validate_compatibility(id: &str, compatibility: &ProviderCompatibility) -> Result<()> {
    for value in [
        compatibility.max_tokens_field.as_deref(),
        compatibility.thinking_format.as_deref(),
        compatibility.cache_control_format.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if value.is_empty() || value.len() > 64 {
            bail!("provider `{id}` compatibility values must contain 1-64 bytes");
        }
    }
    Ok(())
}

fn validate_credential_reference(reference: &CredentialReference) -> Result<()> {
    match reference {
        CredentialReference::Environment { name } => validate_environment_name(name),
        CredentialReference::Keyring { service, account } => {
            if service.trim().is_empty() || account.trim().is_empty() {
                bail!("keyring credential references require service and account names");
            }
            Ok(())
        }
    }
}

fn validate_provider_header(name: &str, value: &ProviderHeader) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
    {
        bail!("provider header names must be valid HTTP tokens");
    }
    match value {
        ProviderHeader::Environment { name } => validate_environment_name(name),
        ProviderHeader::Public { value } => {
            if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
                bail!("public provider header values must contain 1-4096 bytes");
            }
            let sensitive = matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "proxy-authorization" | "x-api-key" | "api-key"
            );
            if sensitive {
                bail!("sensitive provider headers must use an environment reference");
            }
            Ok(())
        }
    }
}

fn validate_environment_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let first = bytes.next();
    if !first.is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("environment references must use portable variable names");
    }
    Ok(())
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

    /// Validates and atomically replaces the live configuration.
    ///
    /// The temporary file is created beside the destination so the final
    /// rename cannot cross a filesystem boundary. The original remains intact
    /// when mutation, validation, serialization, or persistence fails.
    ///
    /// # Errors
    ///
    /// Returns an error when the current configuration is invalid, the
    /// mutation fails, or the replacement cannot be persisted.
    pub fn update<F>(&self, mutate: F) -> Result<AgentConfig>
    where
        F: FnOnce(&mut AgentConfig) -> Result<()>,
    {
        let mut config = self.load_or_default()?;
        mutate(&mut config)?;
        config.validate()?;
        let mut encoded = serde_json::to_vec_pretty(&config)
            .context("failed to serialize agent configuration")?;
        encoded.push(b'\n');

        let parent = self
            .path
            .parent()
            .context("agent configuration path has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create agent config directory at {}",
                parent.display()
            )
        })?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "failed to create temporary agent config beside {}",
                self.path.display()
            )
        })?;
        temporary
            .write_all(&encoded)
            .context("failed to write temporary agent configuration")?;
        temporary
            .as_file_mut()
            .sync_all()
            .context("failed to sync temporary agent configuration")?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| {
                format!(
                    "failed to atomically replace agent config at {}",
                    self.path.display()
                )
            })?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        AgentConfig, AgentMode, HarnessConfig, LiveConfig, MistakePolicy, Modality, ModelRoute,
    };
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
    fn live_updates_are_validated_before_atomic_replacement() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".crumb").join("agent.json");
        let live = LiveConfig::new(&path);
        live.update(|config| {
            config.mode = AgentMode::Plan;
            Ok(())
        })
        .expect("initial update");
        let original = std::fs::read(&path).expect("persisted config");
        assert_eq!(live.load().expect("load").mode, AgentMode::Plan);

        let result = live.update(|config| {
            config.limits.max_output_bytes = 0;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(path).expect("preserved config"), original);
    }

    #[test]
    fn arbitrary_provider_configuration_is_validated_without_secrets() {
        let config = serde_json::from_str::<AgentConfig>(
            r#"{
                "providers": {
                    "custom_gateway": {
                        "display_name": "Custom Gateway",
                        "protocol": "open_ai_completions",
                        "base_url": "https://models.example.test/v1",
                        "credential": {"source": "environment", "name": "CUSTOM_API_KEY"},
                        "headers": {
                            "HTTP-Referer": {"source": "public", "value": "https://crumb.elixpo.com"},
                            "X-API-Key": {"source": "environment", "name": "CUSTOM_API_KEY"}
                        },
                        "models": [{
                            "id": "coding-model",
                            "input": ["text", "image"],
                            "tool_calling": true,
                            "context_window": 131072,
                            "max_output_tokens": 8192,
                            "reasoning_efforts": {"low": "low", "high": "high"}
                        }],
                        "pricing": {"input_tokens": "0.25", "output_tokens": "1.00"}
                    }
                },
                "models": {
                    "text": [{"provider": "custom_gateway", "model": "coding-model", "effort": "high"}]
                }
            }"#,
        )
        .expect("provider schema parses");
        config.validate().expect("provider configuration validates");
        let encoded = serde_json::to_string(&config).expect("config serializes");
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("CUSTOM_API_KEY\":"));
    }

    #[test]
    fn sensitive_headers_cannot_store_literal_values() {
        let config = serde_json::from_str::<AgentConfig>(
            r#"{
                "providers": {
                    "fixture": {
                        "display_name": "Fixture",
                        "protocol": "open_ai_responses",
                        "base_url": "https://example.test/v1",
                        "headers": {
                            "Authorization": {"source": "public", "value": "Bearer secret"}
                        }
                    }
                }
            }"#,
        )
        .expect("shape parses");
        assert!(config.validate().is_err());
    }

    #[test]
    fn selected_custom_model_and_effort_must_exist() {
        let config = serde_json::from_str::<AgentConfig>(
            r#"{
                "providers": {
                    "fixture": {
                        "display_name": "Fixture",
                        "protocol": "anthropic_messages",
                        "base_url": "https://example.test/v1",
                        "models": [{"id": "known", "reasoning_efforts": {"high": "high"}}]
                    }
                },
                "models": {
                    "text": [{"provider": "fixture", "model": "missing", "effort": "max"}]
                }
            }"#,
        )
        .expect("shape parses");
        assert!(config.validate().is_err());
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
