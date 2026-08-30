use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crumb_agent::LiveConfig;
use nu_ansi_term::{Color, Style};
use reedline::{Completer, CompletionResult, Span, Suggestion};

const STATIC_REFERENCES: &[(&str, &str)] = &[
    ("@file:", "reference a workspace file"),
    ("@folder:", "reference a workspace folder"),
    ("@selection", "reference the active selection"),
    ("@clipboard", "reference confirmed clipboard content"),
    ("@last-error", "reference the previous native error"),
    ("@diff", "reference the current repository diff"),
    ("@session:", "reference a Crumb session"),
    ("@skill:", "reference a configured skill"),
    ("@plugin:", "reference a configured plugin or MCP server"),
    (
        "@connector:pollinations",
        "reference the Pollinations connector",
    ),
];

#[derive(Clone, Debug)]
pub struct CompletionWorkspace(Arc<RwLock<PathBuf>>);

impl CompletionWorkspace {
    pub fn new(workspace: PathBuf) -> Self {
        Self(Arc::new(RwLock::new(workspace)))
    }

    pub fn set(&self, workspace: &Path) {
        if let Ok(mut active) = self.0.write() {
            workspace.clone_into(&mut active);
        }
    }

    fn get(&self) -> PathBuf {
        self.0
            .read()
            .map_or_else(|_| PathBuf::from("."), |active| active.clone())
    }
}

pub struct CrumbCompleter {
    workspace: CompletionWorkspace,
}

impl CrumbCompleter {
    pub const fn new(workspace: CompletionWorkspace) -> Self {
        Self { workspace }
    }
}

impl Completer for CrumbCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let Some(prefix) = line.get(..pos) else {
            return CompletionResult::fresh(Vec::new());
        };
        let suggestions = if prefix.starts_with('/') && !prefix.contains('\n') {
            slash_suggestions(prefix, pos)
        } else if current_token(prefix).1.starts_with('@') {
            reference_suggestions(prefix, pos, &self.workspace.get())
        } else {
            native_path_suggestions(prefix, pos, &self.workspace.get())
        };
        CompletionResult::fresh(if suggestions.is_empty() {
            vec![no_records_suggestion(prefix, pos)]
        } else {
            suggestions
        })
    }
}

fn current_token(line: &str) -> (usize, &str) {
    let start = line
        .char_indices()
        .rev()
        .find_map(|(index, character)| character.is_whitespace().then_some(index + 1))
        .unwrap_or(0);
    (start, &line[start..])
}

fn slash_suggestions(prefix: &str, pos: usize) -> Vec<Suggestion> {
    crumb_repl::SLASH_COMMANDS
        .iter()
        .filter(|command| command.usage.starts_with(prefix))
        .map(|command| suggestion(command.usage, command.description, Span::new(0, pos)))
        .collect()
}

fn reference_suggestions(line: &str, pos: usize, workspace: &Path) -> Vec<Suggestion> {
    let (start, token) = current_token(line);
    if !token.starts_with('@') {
        return Vec::new();
    }
    let span = Span::new(start, pos);
    if let Some(path) = token.strip_prefix("@file:") {
        return path_suggestions(workspace, path, false, span);
    }
    if let Some(path) = token.strip_prefix("@folder:") {
        return path_suggestions(workspace, path, true, span);
    }

    let mut candidates = STATIC_REFERENCES
        .iter()
        .map(|(value, description)| ((*value).to_owned(), (*description).to_owned()))
        .collect::<Vec<_>>();
    if token.starts_with("@skill:") {
        candidates.extend(configured_references(workspace, true));
    } else if token.starts_with("@plugin:") {
        candidates.extend(configured_references(workspace, false));
    }
    candidates
        .into_iter()
        .filter(|(value, _)| value.starts_with(token))
        .map(|(value, description)| suggestion(&value, &description, span))
        .collect()
}

fn native_path_suggestions(line: &str, pos: usize, workspace: &Path) -> Vec<Suggestion> {
    let (start, token) = current_token(line);
    if start == 0 || token.starts_with('-') || token.contains('=') || token.contains('\n') {
        return Vec::new();
    }
    let typed_path = Path::new(token);
    let parent = typed_path.parent().unwrap_or_else(|| Path::new(""));
    let fragment = typed_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let directory = if token.starts_with("~/") {
        let Some(home) = std::env::var_os("HOME") else {
            return Vec::new();
        };
        PathBuf::from(home).join(parent.strip_prefix("~").unwrap_or(parent))
    } else {
        workspace.join(parent)
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let show_hidden = fragment.starts_with('.');
    let mut suggestions = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(fragment) || (!show_hidden && name.starts_with('.')) {
                return None;
            }
            let is_directory = entry.file_type().ok()?.is_dir();
            let mut value = parent.join(name).display().to_string();
            if is_directory {
                value.push('/');
            }
            Some(Suggestion {
                display_override: Some(value.clone()),
                description: Some(if is_directory { "folder" } else { "file" }.to_owned()),
                span: Span::new(start, pos),
                append_whitespace: !is_directory,
                value,
                ..Suggestion::default()
            })
        })
        .take(100)
        .collect::<Vec<_>>();
    suggestions.sort_by(|left, right| left.value.cmp(&right.value));
    suggestions
}

fn no_records_suggestion(line: &str, pos: usize) -> Suggestion {
    let (start, token) = current_token(line);
    let label = " NO RECORDS FOUND ";
    let width = crossterm::terminal::size().map_or(80, |(columns, _)| usize::from(columns));
    let padding = width.saturating_sub(label.len()).saturating_div(2);
    let chip = if std::env::var_os("NO_COLOR").is_some() {
        label.to_owned()
    } else {
        Style::new()
            .fg(Color::Black)
            .on(Color::Rgb(255, 255, 204))
            .bold()
            .paint(label)
            .to_string()
    };
    Suggestion {
        value: token.to_owned(),
        display_override: Some(format!("{}{chip}", " ".repeat(padding))),
        span: Span::new(start, pos),
        append_whitespace: false,
        ..Suggestion::default()
    }
}

fn configured_references(workspace: &Path, skills: bool) -> Vec<(String, String)> {
    let Some(path) = config_path(workspace) else {
        return Vec::new();
    };
    let Ok(config) = LiveConfig::new(path).load() else {
        return Vec::new();
    };
    if skills {
        config
            .skills
            .into_iter()
            .filter(|skill| skill.enabled)
            .map(|skill| {
                (
                    format!("@skill:{}", skill.id),
                    "configured skill".to_owned(),
                )
            })
            .collect()
    } else {
        config
            .mcp_servers
            .into_iter()
            .map(|server| {
                (
                    format!("@plugin:{}", server.id),
                    "configured plugin or MCP server".to_owned(),
                )
            })
            .collect()
    }
}

fn config_path(workspace: &Path) -> Option<PathBuf> {
    workspace.ancestors().find_map(|directory| {
        let candidate = directory.join(".crumb/agent.json");
        candidate.is_file().then_some(candidate)
    })
}

fn path_suggestions(
    workspace: &Path,
    typed: &str,
    directories_only: bool,
    span: Span,
) -> Vec<Suggestion> {
    let typed_path = Path::new(typed);
    let parent = typed_path.parent().unwrap_or_else(|| Path::new(""));
    let fragment = typed_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let directory = workspace.join(parent);
    let Ok(canonical_workspace) = std::fs::canonicalize(workspace) else {
        return Vec::new();
    };
    let Ok(canonical_directory) = std::fs::canonicalize(&directory) else {
        return Vec::new();
    };
    if !canonical_directory.starts_with(&canonical_workspace) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(canonical_directory) else {
        return Vec::new();
    };
    let marker = if directories_only {
        "@folder:"
    } else {
        "@file:"
    };
    let mut suggestions = entries
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if (directories_only && !file_type.is_dir())
                || (!directories_only && !file_type.is_file())
            {
                return None;
            }
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(fragment) {
                return None;
            }
            let path = parent.join(name);
            Some(suggestion(
                &format!("{marker}{}", path.display()),
                if directories_only { "folder" } else { "file" },
                span,
            ))
        })
        .take(100)
        .collect::<Vec<_>>();
    suggestions.sort_by(|left, right| left.value.cmp(&right.value));
    suggestions
}

fn suggestion(value: &str, description: &str, span: Span) -> Suggestion {
    Suggestion {
        value: value.to_owned(),
        display_override: Some(format!("{value}  {description}")),
        description: Some(description.to_owned()),
        span,
        append_whitespace: !value.ends_with([':', ' ']),
        ..Suggestion::default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use reedline::Completer;

    use super::{CompletionWorkspace, CrumbCompleter};

    #[test]
    fn slash_completion_includes_help() {
        let mut completer = CrumbCompleter::new(CompletionWorkspace::new(PathBuf::from(".")));
        let result = completer.complete("/he", 3);
        assert!(
            result
                .suggestions()
                .iter()
                .any(|candidate| candidate.value == "/help")
        );
    }

    #[test]
    fn references_complete_inside_plain_language() {
        let mut completer = CrumbCompleter::new(CompletionWorkspace::new(PathBuf::from(".")));
        let result = completer.complete("explain @last", 13);
        assert_eq!(result.suggestions()[0].value, "@last-error");
        assert_eq!(result.suggestions()[0].span.start, 8);
    }

    #[test]
    fn native_arguments_complete_relative_directories() {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut completer = CrumbCompleter::new(CompletionWorkspace::new(workspace));
        let result = completer.complete("cd do", 5);

        assert!(
            result
                .suggestions()
                .iter()
                .any(|candidate| candidate.value == "docs/")
        );
    }

    #[test]
    fn empty_completion_uses_a_non_inserting_footer_chip() {
        let mut completer = CrumbCompleter::new(CompletionWorkspace::new(PathBuf::from(".")));
        let result = completer.complete("cd this-path-does-not-exist", 27);
        let suggestion = &result.suggestions()[0];

        assert_eq!(suggestion.value, "this-path-does-not-exist");
        assert!(
            suggestion
                .display_override
                .as_deref()
                .is_some_and(|display| display.contains("NO RECORDS FOUND"))
        );
        assert!(!suggestion.append_whitespace);
    }
}
