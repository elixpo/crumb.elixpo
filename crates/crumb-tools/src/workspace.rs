use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use crumb_agent::{
    CancellationToken, RiskClass, ToolDescriptor, ToolHandler, ToolHost, ToolOutput, ToolTransport,
};
use serde_json::{Value, json};

use crate::{CheckpointStore, bounded_text};

const READ_FILE: &str = "read_file";
const LIST_DIRECTORY: &str = "list_directory";
const WRITE_FILE: &str = "write_file";
static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

/// Runtime ceilings for workspace read tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceToolLimits {
    pub max_output_bytes: usize,
    pub max_directory_entries: usize,
}

/// Runtime ceiling for a checkpointed workspace write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceWriteLimits {
    pub max_file_bytes: usize,
}

/// Registers bounded read-only tools rooted at one canonical workspace.
///
/// # Errors
///
/// Returns an error when the workspace is unavailable, either limit is zero,
/// or a tool name is already registered.
pub fn register_workspace_read_tools(
    host: &mut ToolHost,
    workspace: &Path,
    limits: WorkspaceToolLimits,
) -> Result<()> {
    if limits.max_output_bytes == 0 {
        bail!("workspace tool output limit must be positive");
    }
    if limits.max_directory_entries == 0 {
        bail!("workspace directory entry limit must be positive");
    }
    let root = fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
    if !root.is_dir() {
        bail!("workspace root must be a directory");
    }
    let boundary = WorkspaceBoundary { root, limits };
    host.register(read_descriptor(), Arc::new(ReadFile(boundary.clone())))?;
    host.register(list_descriptor(), Arc::new(ListDirectory(boundary)))?;
    Ok(())
}

/// Registers a permission-gated UTF-8 write tool with mandatory checkpoints.
///
/// # Errors
///
/// Returns an error when the workspace or limit is invalid, or the tool name
/// is already registered.
pub fn register_workspace_write_tool(
    host: &mut ToolHost,
    workspace: &Path,
    limits: WorkspaceWriteLimits,
) -> Result<()> {
    if limits.max_file_bytes == 0 {
        bail!("workspace write limit must be positive");
    }
    let root = fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
    let checkpoints = CheckpointStore::new(&root, limits.max_file_bytes)?;
    host.register(
        write_descriptor(),
        Arc::new(WriteFile {
            root,
            limits,
            checkpoints,
        }),
    )
}

#[derive(Clone)]
struct WorkspaceBoundary {
    root: PathBuf,
    limits: WorkspaceToolLimits,
}

impl WorkspaceBoundary {
    fn resolve(&self, requested: &str) -> Result<PathBuf> {
        if requested.trim().is_empty() {
            bail!("path cannot be empty");
        }
        let requested_path = Path::new(requested);
        let candidate = if requested_path.is_absolute() {
            requested_path.to_path_buf()
        } else {
            self.root.join(requested_path)
        };
        let resolved = fs::canonicalize(&candidate)
            .with_context(|| format!("path `{requested}` does not exist"))?;
        if !resolved.starts_with(&self.root) {
            bail!("path escapes the workspace");
        }
        Ok(resolved)
    }
}

struct ReadFile(WorkspaceBoundary);

impl ToolHandler for ReadFile {
    fn call(&self, arguments: &Value, cancellation: &CancellationToken) -> Result<ToolOutput> {
        Ok(match read_file(&self.0, arguments, cancellation) {
            Ok(output) => output,
            Err(error) => ToolOutput::error(error.to_string()),
        })
    }
}

fn read_file(
    boundary: &WorkspaceBoundary,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<ToolOutput> {
    ensure_active(cancellation)?;
    let path = string_argument(arguments, "path")?;
    let offset = optional_u64(arguments, "offset_bytes")?.unwrap_or(0);
    let requested_limit = optional_u64(arguments, "limit_bytes")?
        .map(usize::try_from)
        .transpose()
        .context("limit_bytes is too large")?
        .unwrap_or(boundary.limits.max_output_bytes);
    let limit = requested_limit.min(boundary.limits.max_output_bytes);
    if limit == 0 {
        bail!("limit_bytes must be positive");
    }
    let resolved = boundary.resolve(path)?;
    if !resolved.is_file() {
        bail!("path is not a regular file");
    }
    let mut file = File::open(&resolved).with_context(|| format!("failed to open `{path}`"))?;
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("failed to seek `{path}`"))?;
    let read_limit = u64::try_from(limit)
        .context("file read limit is too large")?
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(limit.saturating_add(1));
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read `{path}`"))?;
    ensure_active(cancellation)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(error) if error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            String::from_utf8(error.into_bytes()[..valid].to_vec())
                .context("valid UTF-8 prefix could not be decoded")?
        }
        Err(_) => bail!("file is not valid UTF-8"),
    };
    let suffix = if truncated {
        "\n[output truncated]"
    } else {
        ""
    };
    Ok(ToolOutput::text(bounded_text(
        format!("path: {path}\noffset_bytes: {offset}\n{content}{suffix}"),
        boundary.limits.max_output_bytes,
    )))
}

struct ListDirectory(WorkspaceBoundary);

impl ToolHandler for ListDirectory {
    fn call(&self, arguments: &Value, cancellation: &CancellationToken) -> Result<ToolOutput> {
        Ok(match list_directory(&self.0, arguments, cancellation) {
            Ok(output) => output,
            Err(error) => ToolOutput::error(error.to_string()),
        })
    }
}

struct WriteFile {
    root: PathBuf,
    limits: WorkspaceWriteLimits,
    checkpoints: CheckpointStore,
}

impl ToolHandler for WriteFile {
    fn call(&self, arguments: &Value, cancellation: &CancellationToken) -> Result<ToolOutput> {
        match write_file(self, arguments, cancellation) {
            Ok(output) => Ok(output),
            Err(error) if cancellation.is_cancelled() => Err(error),
            Err(error) => Ok(ToolOutput::error(error.to_string())),
        }
    }
}

fn write_file(
    tool: &WriteFile,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<ToolOutput> {
    ensure_active(cancellation)?;
    let requested = string_argument(arguments, "path")?;
    let content = string_argument(arguments, "content")?;
    if content.len() > tool.limits.max_file_bytes {
        bail!("file content exceeds the configured write limit");
    }
    let (relative, target) = resolve_write_target(&tool.root, requested)?;
    let before = read_optional_bounded(&target, tool.limits.max_file_bytes)?;
    tool.checkpoints
        .validate_edit(&relative, before.as_deref(), content.as_bytes())?;
    ensure_active(cancellation)?;
    let checkpoint = install_checkpointed_write(
        &target,
        &relative,
        before.as_deref(),
        content.as_bytes(),
        &tool.checkpoints,
        cancellation,
    )?;
    Ok(ToolOutput {
        text: format!(
            "wrote {} bytes to {}\ncheckpoint: {}",
            content.len(),
            relative.display(),
            checkpoint.id
        ),
        structured: Some(json!({
            "path": relative,
            "bytes": content.len(),
            "checkpoint": checkpoint.id
        })),
        is_error: false,
    })
}

fn resolve_write_target(root: &Path, requested: &str) -> Result<(PathBuf, PathBuf)> {
    let relative = Path::new(requested);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            !matches!(component, std::path::Component::Normal(_))
                || matches!(
                    component.as_os_str().to_str(),
                    Some(".git" | ".crumb" | ".ssh")
                )
        })
    {
        bail!("write path must be a safe workspace-relative file");
    }
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let resolved_parent = fs::canonicalize(root.join(parent))
        .with_context(|| format!("parent directory for `{requested}` does not exist"))?;
    if !resolved_parent.starts_with(root) || !resolved_parent.is_dir() {
        bail!("write path escapes the workspace");
    }
    let filename = relative
        .file_name()
        .context("write path requires a filename")?;
    let target = resolved_parent.join(filename);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!("write target is a symlink"),
        Ok(metadata) if !metadata.is_file() => bail!("write target is not a regular file"),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect write target"),
    }
    Ok((relative.to_path_buf(), target))
}

fn read_optional_bounded(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to read write preimage"),
    };
    let mut content = Vec::with_capacity(limit.min(64 * 1024));
    file.take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut content)
        .context("failed to read write preimage")?;
    if content.len() > limit {
        bail!("existing file exceeds the configured write limit");
    }
    Ok(Some(content))
}

fn install_checkpointed_write(
    target: &Path,
    relative: &Path,
    before: Option<&[u8]>,
    after: &[u8],
    checkpoints: &CheckpointStore,
    cancellation: &CancellationToken,
) -> Result<crate::WorkspaceCheckpoint> {
    let parent = target.parent().context("write target has no parent")?;
    let sequence = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".crumb-write-{}-{sequence}.tmp",
        std::process::id()
    ));
    let backup = parent.join(format!(
        ".crumb-write-{}-{sequence}.bak",
        std::process::id()
    ));
    write_temporary(&temporary, after, target.exists().then_some(target))?;
    if cancellation.is_cancelled() {
        let _ = fs::remove_file(&temporary);
        bail!("tool call cancelled");
    }
    if before.is_some() {
        fs::rename(target, &backup).context("failed to stage write preimage")?;
    }
    if let Err(error) = fs::rename(&temporary, target) {
        if before.is_some() {
            let _ = fs::rename(&backup, target);
        }
        return Err(error).context("failed to install workspace write");
    }
    match checkpoints.record_edit(relative, before, after) {
        Ok(checkpoint) => {
            if before.is_some() {
                fs::remove_file(&backup).context("failed to remove write backup")?;
            }
            Ok(checkpoint)
        }
        Err(error) => {
            let _ = fs::remove_file(target);
            if before.is_some() {
                let _ = fs::rename(&backup, target);
            }
            Err(error).context("workspace write was rolled back because checkpointing failed")
        }
    }
}

fn write_temporary(path: &Path, content: &[u8], permissions_from: Option<&Path>) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("failed to create temporary workspace write")?;
    if let Some(source) = permissions_from {
        fs::set_permissions(path, fs::metadata(source)?.permissions())
            .context("failed to preserve file permissions")?;
    }
    file.write_all(content)
        .context("failed to write temporary workspace file")?;
    file.flush()
        .context("failed to flush temporary workspace file")
}

fn list_directory(
    boundary: &WorkspaceBoundary,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<ToolOutput> {
    ensure_active(cancellation)?;
    let path = string_argument(arguments, "path")?;
    let max_entries = optional_u64(arguments, "max_entries")?
        .map(usize::try_from)
        .transpose()
        .context("max_entries is too large")?
        .unwrap_or(boundary.limits.max_directory_entries)
        .min(boundary.limits.max_directory_entries);
    if max_entries == 0 {
        bail!("max_entries must be positive");
    }
    let resolved = boundary.resolve(path)?;
    if !resolved.is_dir() {
        bail!("path is not a directory");
    }
    let mut entries = fs::read_dir(&resolved)
        .with_context(|| format!("failed to list `{path}`"))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read an entry in `{path}`"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let truncated_entries = entries.len() > max_entries;
    let mut output = format!("path: {path}\n");
    for entry in entries.into_iter().take(max_entries) {
        ensure_active(cancellation)?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect `{}`", entry.path().display()))?;
        let kind = if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else if file_type.is_symlink() {
            "symlink"
        } else {
            "other"
        };
        let line = format!("{kind}\t{}\n", entry.file_name().to_string_lossy());
        if output.len().saturating_add(line.len()) > boundary.limits.max_output_bytes {
            output.push_str("[output truncated]\n");
            return Ok(ToolOutput::text(bounded_text(
                output,
                boundary.limits.max_output_bytes,
            )));
        }
        output.push_str(&line);
    }
    if truncated_entries {
        output.push_str("[entry limit reached]\n");
    }
    Ok(ToolOutput::text(bounded_text(
        output,
        boundary.limits.max_output_bytes,
    )))
}

fn ensure_active(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        bail!("tool call cancelled");
    }
    Ok(())
}

fn string_argument<'a>(arguments: &'a Value, name: &str) -> Result<&'a str> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("{name} must be a string"))
}

fn optional_u64(arguments: &Value, name: &str) -> Result<Option<u64>> {
    match arguments.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .with_context(|| format!("{name} must be a non-negative integer")),
    }
}

fn read_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: READ_FILE.to_owned(),
        description: "Read a bounded UTF-8 byte range from a file inside the workspace.".to_owned(),
        input_schema: json!({
            "type":"object",
            "properties":{
                "path":{"type":"string"},
                "offset_bytes":{"type":"integer","minimum":0},
                "limit_bytes":{"type":"integer","minimum":1}
            },
            "required":["path"],
            "additionalProperties":false
        }),
        risk: RiskClass::ReadOnly,
        transport: ToolTransport::Native,
    }
}

fn list_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: LIST_DIRECTORY.to_owned(),
        description: "List a bounded, sorted directory inside the workspace without following entry symlinks."
            .to_owned(),
        input_schema: json!({
            "type":"object",
            "properties":{
                "path":{"type":"string"},
                "max_entries":{"type":"integer","minimum":1}
            },
            "required":["path"],
            "additionalProperties":false
        }),
        risk: RiskClass::ReadOnly,
        transport: ToolTransport::Native,
    }
}

fn write_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: WRITE_FILE.to_owned(),
        description:
            "Replace or create one UTF-8 workspace file with a mandatory review checkpoint."
                .to_owned(),
        input_schema: json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","minLength":1},
                "content":{"type":"string"}
            },
            "required":["path","content"],
            "additionalProperties":false
        }),
        risk: RiskClass::WriteWorkspace,
        transport: ToolTransport::Native,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crumb_agent::{
        AgentMode, ApprovalBroker, ApprovalDecision, ApprovalRequest, CancellationToken,
        DenyAllApprovals, ToolCallErrorKind, ToolHost,
    };
    use serde_json::json;

    use crate::{CheckpointDecision, CheckpointStore};

    use super::{
        WorkspaceToolLimits, WorkspaceWriteLimits, register_workspace_read_tools,
        register_workspace_write_tool,
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempWorkspace(PathBuf);

    impl TempWorkspace {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("crumb-tools-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).expect("temporary workspace is created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("temporary workspace is removed");
        }
    }

    fn host(workspace: &Path, max_output_bytes: usize, max_directory_entries: usize) -> ToolHost {
        let mut host = ToolHost::default();
        register_workspace_read_tools(
            &mut host,
            workspace,
            WorkspaceToolLimits {
                max_output_bytes,
                max_directory_entries,
            },
        )
        .expect("workspace tools are registered");
        host
    }

    struct AllowOnce;

    impl ApprovalBroker for AllowOnce {
        fn decide(
            &self,
            _request: &ApprovalRequest,
            _arguments: &serde_json::Value,
            _cancellation: &CancellationToken,
        ) -> ApprovalDecision {
            ApprovalDecision::AllowOnce
        }
    }

    fn write_host(workspace: &Path) -> ToolHost {
        let mut host = ToolHost::default();
        register_workspace_write_tool(
            &mut host,
            workspace,
            WorkspaceWriteLimits {
                max_file_bytes: 128,
            },
        )
        .expect("write tool is registered");
        host
    }

    #[test]
    fn reads_a_bounded_file_range_without_approval() {
        let workspace = TempWorkspace::new();
        fs::write(workspace.path().join("notes.txt"), "alpha beta gamma")
            .expect("fixture is written");
        let output = host(workspace.path(), 128, 8)
            .call(
                "read_file",
                &json!({"path":"notes.txt", "offset_bytes":6, "limit_bytes":4}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &CancellationToken::default(),
            )
            .expect("read-only call is authorized");
        assert!(!output.is_error);
        assert!(output.text.contains("beta"));
        assert!(output.text.contains("[output truncated]"));
        assert!(output.text.len() <= 128);
    }

    #[test]
    fn directory_entries_are_sorted_and_limited() {
        let workspace = TempWorkspace::new();
        fs::write(workspace.path().join("z.txt"), "z").expect("fixture is written");
        fs::write(workspace.path().join("a.txt"), "a").expect("fixture is written");
        let output = host(workspace.path(), 128, 1)
            .call(
                "list_directory",
                &json!({"path":"."}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &CancellationToken::default(),
            )
            .expect("directory listing succeeds");
        assert!(output.text.contains("a.txt"));
        assert!(!output.text.contains("z.txt"));
        assert!(output.text.contains("[entry limit reached]"));
    }

    #[test]
    fn parent_traversal_cannot_escape_the_workspace() {
        let workspace = TempWorkspace::new();
        let outside_name = format!(
            "{}-outside.txt",
            workspace
                .path()
                .file_name()
                .expect("workspace has a name")
                .to_string_lossy()
        );
        let outside = workspace
            .path()
            .parent()
            .expect("workspace has a parent")
            .join(&outside_name);
        fs::write(&outside, "secret").expect("outside fixture is written");
        let output = host(workspace.path(), 128, 8)
            .call(
                "read_file",
                &json!({"path":format!("../{outside_name}")}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &CancellationToken::default(),
            )
            .expect("expected path errors are tool output");
        fs::remove_file(outside).expect("outside fixture is removed");
        assert!(output.is_error);
        assert_eq!(output.text, "path escapes the workspace");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_the_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = TempWorkspace::new();
        symlink("/etc/passwd", workspace.path().join("escape"))
            .expect("fixture symlink is created");
        let output = host(workspace.path(), 128, 8)
            .call(
                "read_file",
                &json!({"path":"escape"}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &CancellationToken::default(),
            )
            .expect("expected path errors are tool output");
        assert!(output.is_error);
        assert_eq!(output.text, "path escapes the workspace");
    }

    #[test]
    fn cancellation_stops_before_file_access() {
        let workspace = TempWorkspace::new();
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let error = host(workspace.path(), 128, 8)
            .call(
                "read_file",
                &json!({"path":"missing"}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &cancellation,
            )
            .expect_err("cancelled calls do not reach the handler");
        assert_eq!(error.kind, ToolCallErrorKind::Cancelled);
    }

    #[test]
    fn approved_write_creates_a_rewindable_checkpoint() {
        let workspace = TempWorkspace::new();
        let output = write_host(workspace.path())
            .call(
                "write_file",
                &json!({"path":"notes.txt","content":"after"}),
                AgentMode::Auto,
                &AllowOnce,
                &CancellationToken::default(),
            )
            .expect("approved write succeeds");
        assert!(!output.is_error);
        let checkpoint = output
            .structured
            .as_ref()
            .and_then(|value| value.get("checkpoint"))
            .and_then(serde_json::Value::as_str)
            .expect("checkpoint id is returned");
        let store = CheckpointStore::new(workspace.path(), 128).expect("store opens");
        store
            .decide(checkpoint, CheckpointDecision::Reject)
            .expect("checkpoint rewinds");
        assert!(!workspace.path().join("notes.txt").exists());
    }

    #[test]
    fn sensitive_write_is_rejected_before_mutation() {
        let workspace = TempWorkspace::new();
        let output = write_host(workspace.path())
            .call(
                "write_file",
                &json!({"path":".env.local","content":"TOKEN=secret"}),
                AgentMode::Auto,
                &AllowOnce,
                &CancellationToken::default(),
            )
            .expect("expected write refusal is tool output");
        assert!(output.is_error);
        assert!(!workspace.path().join(".env.local").exists());
    }
}
