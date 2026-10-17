use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const GRAPHIFY_PINNED_VERSION: &str = "0.9.92";
const VERSION_OUTPUT_LIMIT: usize = 4096;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphSettings {
    pub enabled: bool,
    pub adapter: String,
    pub command: PathBuf,
    pub expected_version: String,
    pub graph_path: PathBuf,
}

impl Default for GraphSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            adapter: "graphify".to_owned(),
            command: PathBuf::from("graphify"),
            expected_version: GRAPHIFY_PINNED_VERSION.to_owned(),
            graph_path: PathBuf::from("graphify-out/graph.json"),
        }
    }
}

impl GraphSettings {
    /// Validates settings without launching the adapter.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported adapters, invalid versions, or paths
    /// that could escape the project workspace.
    pub fn validate(&self) -> Result<()> {
        if self.adapter != "graphify" {
            bail!("project graph adapter is unsupported");
        }
        if self.command.as_os_str().is_empty()
            || !valid_version(&self.expected_version)
            || !safe_relative_path(&self.graph_path)
        {
            bail!("project graph settings are invalid");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct GraphSettingsStore {
    path: PathBuf,
}

impl GraphSettingsStore {
    #[must_use]
    pub fn for_workspace(workspace: &Path) -> Self {
        Self {
            path: workspace.join(".crumb").join("graph.json"),
        }
    }

    /// Loads settings or safe disabled defaults when no settings exist.
    ///
    /// # Errors
    ///
    /// Returns an error when existing settings are unreadable or invalid.
    pub fn load_or_default(&self) -> Result<GraphSettings> {
        if !self.path.exists() {
            return Ok(GraphSettings::default());
        }
        let encoded = fs::read(&self.path).context("project graph settings could not be read")?;
        if encoded.len() > 16 * 1024 {
            bail!("project graph settings exceed the byte limit");
        }
        let settings: GraphSettings =
            serde_json::from_slice(&encoded).context("project graph settings are malformed")?;
        settings.validate()?;
        Ok(settings)
    }

    /// Atomically persists non-secret graph settings.
    ///
    /// # Errors
    ///
    /// Returns an error when validation or persistence fails.
    pub fn save(&self, settings: &GraphSettings) -> Result<()> {
        settings.validate()?;
        let parent = self
            .path
            .parent()
            .context("project graph settings path has no parent")?;
        fs::create_dir_all(parent).context("project graph settings directory is unavailable")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .context("project graph settings temporary file is unavailable")?;
        serde_json::to_writer_pretty(&mut temporary, settings)
            .context("project graph settings could not be encoded")?;
        temporary.write_all(b"\n")?;
        temporary.as_file_mut().sync_all()?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .context("project graph settings could not be replaced")?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphifyStatus {
    pub available: bool,
    pub compatible: bool,
    pub detected_version: Option<String>,
}

pub struct GraphifyAdapter {
    settings: GraphSettings,
}

impl GraphifyAdapter {
    /// Creates an adapter from validated non-secret settings.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings are invalid.
    pub fn new(settings: GraphSettings) -> Result<Self> {
        settings.validate()?;
        Ok(Self { settings })
    }

    #[must_use]
    pub fn graph_path(&self, workspace: &Path) -> PathBuf {
        workspace.join(&self.settings.graph_path)
    }

    #[must_use]
    pub fn settings(&self) -> &GraphSettings {
        &self.settings
    }

    #[must_use]
    pub fn mcp_command(&self) -> PathBuf {
        let parent = self
            .settings
            .command
            .parent()
            .filter(|path| !path.as_os_str().is_empty());
        parent.map_or_else(
            || PathBuf::from("graphify-mcp"),
            |path| path.join("graphify-mcp"),
        )
    }

    #[must_use]
    pub fn mcp_available(&self) -> bool {
        run_probe(&self.mcp_command(), "--help").is_ok()
    }

    /// Checks the installed executable without installing or updating it.
    #[must_use]
    pub fn status(&self) -> GraphifyStatus {
        let output = run_version(&self.settings.command);
        match output {
            Ok(output) => {
                let detected = detect_version(&output);
                GraphifyStatus {
                    available: true,
                    compatible: detected.as_deref() == Some(&self.settings.expected_version),
                    detected_version: detected,
                }
            }
            Err(_) => GraphifyStatus {
                available: false,
                compatible: false,
                detected_version: None,
            },
        }
    }

    /// Runs an explicit, local Graphify extraction after an exact version
    /// check. The child receives no inherited secrets and is killed on timeout
    /// or cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error when Graphify is absent, incompatible, cancelled,
    /// times out, fails, or does not produce the configured graph.
    pub fn build<F>(&self, workspace: &Path, timeout: Duration, cancelled: F) -> Result<PathBuf>
    where
        F: Fn() -> bool,
    {
        let status = self.status();
        if !status.available {
            bail!("Graphify is not installed; install graphifyy[mcp]=={GRAPHIFY_PINNED_VERSION}");
        }
        if !status.compatible {
            bail!("Graphify version is incompatible; expected {GRAPHIFY_PINNED_VERSION}");
        }
        let mut command = Command::new(&self.settings.command);
        command
            .args(["extract", ".", "--out", "."])
            .current_dir(workspace)
            .env_clear()
            .env("PYTHONHASHSEED", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        preserve_runtime_path(&mut command);
        let mut child = command.spawn().context("Graphify could not be started")?;
        wait_for_child(&mut child, timeout, cancelled)?;
        let graph_path = self.graph_path(workspace);
        if !graph_path.is_file() {
            bail!("Graphify completed without producing graph.json");
        }
        Ok(graph_path)
    }
}

fn run_version(command: &Path) -> Result<String> {
    run_probe(command, "--version")
}

fn run_probe(command: &Path, argument: &str) -> Result<String> {
    let mut process = Command::new(command);
    process
        .arg(argument)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    preserve_runtime_path(&mut process);
    let mut child = process.spawn().context("Graphify is unavailable")?;
    let mut stdout = child
        .stdout
        .take()
        .context("Graphify version output is unavailable")?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .by_ref()
            .take((VERSION_OUTPUT_LIMIT + 1) as u64)
            .read_to_end(&mut output)
            .map(|_| output)
    });
    wait_for_child(&mut child, Duration::from_secs(5), || false)?;
    let output = reader
        .join()
        .map_err(|_| anyhow::anyhow!("Graphify version reader failed"))??;
    if output.len() > VERSION_OUTPUT_LIMIT {
        bail!("Graphify version output exceeds the byte limit");
    }
    String::from_utf8(output).context("Graphify version output is invalid")
}

fn wait_for_child<F>(child: &mut Child, timeout: Duration, cancelled: F) -> Result<()>
where
    F: Fn() -> bool,
{
    let started = Instant::now();
    loop {
        if cancelled() {
            terminate(child);
            bail!("Graphify was cancelled");
        }
        if started.elapsed() >= timeout {
            terminate(child);
            bail!("Graphify timed out");
        }
        match child
            .try_wait()
            .context("Graphify process state is unavailable")?
        {
            Some(status) if status.success() => return Ok(()),
            Some(_) => bail!("Graphify failed"),
            None => thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn preserve_runtime_path(command: &mut Command) {
    for name in ["PATH", "SYSTEMROOT", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

fn detect_version(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches('v');
        (valid_version(word) && word.bytes().filter(|byte| *byte == b'.').count() == 2)
            .then(|| word.to_owned())
    })
}

fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn settings_are_disabled_and_pinned_by_default() {
        let settings = GraphSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.expected_version, GRAPHIFY_PINNED_VERSION);
        settings.validate().expect("defaults validate");
    }

    #[test]
    fn settings_round_trip_without_executable_activity() {
        let workspace = tempfile::tempdir().expect("workspace");
        let store = GraphSettingsStore::for_workspace(workspace.path());
        let mut settings = store.load_or_default().expect("defaults");
        settings.enabled = true;
        store.save(&settings).expect("save");
        assert_eq!(store.load_or_default().expect("reload"), settings);
    }

    #[cfg(unix)]
    #[test]
    fn build_requires_exact_version_and_supports_cancellation() {
        use std::os::unix::fs::PermissionsExt;

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("crumb-graphify-{suffix}"));
        fs::create_dir_all(&workspace).expect("workspace");
        let script = workspace.join("graphify-fixture");
        fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'graphify 0.9.92'; exit 0; fi\nsleep 5\n",
        )
        .expect("script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("permissions");
        let adapter = GraphifyAdapter::new(GraphSettings {
            command: script,
            ..GraphSettings::default()
        })
        .expect("adapter");
        assert!(adapter.status().compatible);
        assert!(!adapter.mcp_available());
        let started = Instant::now();
        let error = adapter
            .build(&workspace, Duration::from_secs(10), || true)
            .expect_err("cancelled");
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(1));
        fs::remove_dir_all(workspace).expect("cleanup");
    }
}
