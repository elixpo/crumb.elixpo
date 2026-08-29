use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use crumb_agent::{
    CancellationToken, RiskClass, ToolDescriptor, ToolHandler, ToolHost, ToolOutput, ToolTransport,
};
use serde_json::{Value, json};

use crate::bounded_text;

const READ_FILE: &str = "read_file";
const LIST_DIRECTORY: &str = "list_directory";

/// Runtime ceilings for workspace read tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceToolLimits {
    pub max_output_bytes: usize,
    pub max_directory_entries: usize,
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crumb_agent::{
        AgentMode, CancellationToken, DenyAllApprovals, ToolCallErrorKind, ToolHost,
    };
    use serde_json::json;

    use super::{WorkspaceToolLimits, register_workspace_read_tools};

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
}
