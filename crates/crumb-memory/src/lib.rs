//! Explicitly approved, bounded memory for Crumb agent sessions.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use tempfile::Builder;

const HEADER: &str =
    "# Crumb memory\n\n<!-- Entries are persisted only by an explicit user command. -->\n\n";
const MAX_ENTRY_BYTES: usize = 2 * 1024;
const MAX_MEMORY_BYTES: usize = 32 * 1024;
const MAX_CONTEXT_BYTES: usize = 16 * 1024;

/// Durable memory scope selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryScope {
    Project,
    User,
}

/// One memory file with a fixed trust scope.
#[derive(Clone, Debug)]
pub struct MemoryStore {
    path: PathBuf,
    scope: MemoryScope,
}

/// Paired user and project memory for one workspace.
#[derive(Clone, Debug)]
pub struct MemorySet {
    pub user: MemoryStore,
    pub project: MemoryStore,
}

/// Non-sensitive storage statistics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryStatus {
    pub entries: usize,
    pub bytes: usize,
}

impl MemorySet {
    /// Resolves the closest Crumb project and the current user's private memory.
    ///
    /// # Errors
    ///
    /// Returns an error when a user home directory cannot be resolved.
    pub fn discover(workspace: &Path) -> Result<Self> {
        let project_root = workspace
            .ancestors()
            .find(|directory| directory.join(".crumb/agent.json").is_file())
            .unwrap_or(workspace);
        let home = BaseDirs::new()
            .context("home directory is unavailable")?
            .home_dir()
            .to_path_buf();
        Ok(Self {
            user: MemoryStore::new(home.join(".crumb/memory/MEMORY.md"), MemoryScope::User),
            project: MemoryStore::new(project_root.join(".crumb/MEMORY.md"), MemoryScope::Project),
        })
    }

    /// Returns bounded approved memory ready for insertion ahead of a request.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing memory file is malformed or unreadable.
    pub fn prompt_context(&self) -> Result<Option<String>> {
        let mut context = String::new();
        for (label, store) in [
            ("User memory", &self.user),
            ("Project memory", &self.project),
        ] {
            let entries = store.entries()?;
            if entries.is_empty() {
                continue;
            }
            context.push_str("## ");
            context.push_str(label);
            context.push('\n');
            for entry in entries {
                let projected = context.len().saturating_add(entry.len()).saturating_add(3);
                if projected > MAX_CONTEXT_BYTES {
                    context.push_str("- [additional approved memory omitted]\n");
                    return Ok(Some(context));
                }
                context.push_str("- ");
                context.push_str(&entry);
                context.push('\n');
            }
        }
        Ok((!context.is_empty()).then_some(context))
    }
}

impl MemoryStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, scope: MemoryScope) -> Self {
        Self {
            path: path.into(),
            scope,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn scope(&self) -> MemoryScope {
        self.scope
    }

    /// Reads normalized entries. Missing storage is an empty memory.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized, malformed, sensitive, or unreadable storage.
    pub fn entries(&self) -> Result<Vec<String>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_MEMORY_BYTES as u64 {
            bail!("memory file is not a bounded regular file");
        }
        let contents = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read memory at {}", self.path.display()))?;
        if !contents.starts_with(HEADER) {
            bail!("memory file has an unsupported format");
        }
        contents[HEADER.len()..]
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let entry = line
                    .strip_prefix("- ")
                    .context("memory entries must be Markdown list items")?;
                validate_entry(entry)?;
                Ok(entry.to_owned())
            })
            .collect()
    }

    /// Persists a single explicit, user-approved memory.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry looks sensitive, exceeds limits, or cannot be saved.
    pub fn remember_approved(&self, entry: &str) -> Result<bool> {
        let normalized = normalize_entry(entry);
        validate_entry(&normalized)?;
        let mut entries = self.entries()?;
        if entries.iter().any(|existing| existing == &normalized) {
            return Ok(false);
        }
        entries.push(normalized);
        self.write_entries(&entries)?;
        Ok(true)
    }

    /// Removes one entry by its one-based display index.
    ///
    /// # Errors
    ///
    /// Returns an error when the index is absent or storage cannot be saved.
    pub fn forget_approved(&self, index: usize) -> Result<String> {
        let mut entries = self.entries()?;
        if index == 0 || index > entries.len() {
            bail!("memory index must be between 1 and {}", entries.len());
        }
        let removed = entries.remove(index - 1);
        self.write_entries(&entries)?;
        Ok(removed)
    }

    /// Removes duplicate entries while preserving first-seen order.
    ///
    /// # Errors
    ///
    /// Returns an error when storage cannot be read or saved.
    pub fn compact_approved(&self) -> Result<usize> {
        let entries = self.entries()?;
        let before = entries.len();
        let mut seen = BTreeSet::new();
        let compacted = entries
            .into_iter()
            .filter(|entry| seen.insert(entry.clone()))
            .collect::<Vec<_>>();
        self.write_entries(&compacted)?;
        Ok(before.saturating_sub(compacted.len()))
    }

    /// Returns bounded file statistics without exposing contents.
    ///
    /// # Errors
    ///
    /// Returns an error when storage is invalid or unreadable.
    pub fn status(&self) -> Result<MemoryStatus> {
        let entries = self.entries()?;
        Ok(MemoryStatus {
            entries: entries.len(),
            bytes: entries.iter().map(String::len).sum(),
        })
    }

    fn write_entries(&self, entries: &[String]) -> Result<()> {
        let mut contents = String::from(HEADER);
        for entry in entries {
            validate_entry(entry)?;
            contents.push_str("- ");
            contents.push_str(entry);
            contents.push('\n');
        }
        if contents.len() > MAX_MEMORY_BYTES {
            bail!("memory exceeds the {MAX_MEMORY_BYTES} byte limit");
        }
        let parent = self.path.parent().context("memory path has no parent")?;
        fs::create_dir_all(parent)?;
        let mut temporary = Builder::new().prefix(".memory-").tempfile_in(parent)?;
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file_mut().sync_all()?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to save memory at {}", self.path.display()))?;
        Ok(())
    }
}

fn normalize_entry(entry: &str) -> String {
    entry.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_entry(entry: &str) -> Result<()> {
    if entry.is_empty() || entry.len() > MAX_ENTRY_BYTES || entry.chars().any(char::is_control) {
        bail!("memory entries must contain 1-{MAX_ENTRY_BYTES} safe bytes on one line");
    }
    if looks_sensitive(entry) {
        bail!("memory rejected because it may contain a credential or secret");
    }
    Ok(())
}

fn looks_sensitive(entry: &str) -> bool {
    let lowercase = entry.to_ascii_lowercase();
    [
        "-----begin private key",
        "authorization: bearer",
        "api_key=",
        "apikey=",
        "access_token=",
        "refresh_token=",
        "client_secret=",
        "password=",
        "passwd=",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::{MemoryScope, MemorySet, MemoryStore};
    use tempfile::tempdir;

    #[test]
    fn approved_memory_round_trips_and_compacts() {
        let directory = tempdir().expect("memory tempdir");
        let store = MemoryStore::new(directory.path().join("MEMORY.md"), MemoryScope::Project);
        assert!(
            store
                .remember_approved("Use  cargo   fmt")
                .expect("remember")
        );
        assert!(
            !store
                .remember_approved("Use cargo fmt")
                .expect("deduplicate")
        );
        assert_eq!(store.entries().expect("entries"), ["Use cargo fmt"]);
        assert_eq!(store.compact_approved().expect("compact"), 0);
        assert_eq!(store.forget_approved(1).expect("forget"), "Use cargo fmt");
        assert!(store.entries().expect("empty entries").is_empty());
    }

    #[test]
    fn secrets_and_unstructured_files_are_rejected() {
        let directory = tempdir().expect("memory tempdir");
        let path = directory.path().join("MEMORY.md");
        let store = MemoryStore::new(&path, MemoryScope::User);
        assert!(store.remember_approved("API_KEY=top-secret").is_err());
        std::fs::write(path, "arbitrary instructions").expect("malformed memory fixture");
        assert!(store.entries().is_err());
    }

    #[test]
    fn prompt_context_preserves_scope_labels() {
        let directory = tempdir().expect("memory tempdir");
        let set = MemorySet {
            user: MemoryStore::new(directory.path().join("user.md"), MemoryScope::User),
            project: MemoryStore::new(directory.path().join("project.md"), MemoryScope::Project),
        };
        set.user
            .remember_approved("Prefer concise output")
            .expect("user memory");
        set.project
            .remember_approved("Run workspace tests")
            .expect("project memory");
        let context = set
            .prompt_context()
            .expect("prompt context")
            .expect("memory context");
        assert!(context.contains("## User memory\n- Prefer concise output"));
        assert!(context.contains("## Project memory\n- Run workspace tests"));
    }
}
