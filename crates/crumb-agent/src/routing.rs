//! Deterministic, zero-AI routing between the native shell and agent runtime.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;

/// Destination selected for one input line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputRoute {
    Native,
    Agent,
    Negotiate,
}

/// Stable reason for a routing decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteReason {
    Empty,
    ExplicitAgentPrefix,
    ShellSyntax,
    ResolvedCommand,
    PossibleCommandTypo,
    SingleUnknownToken,
    NaturalLanguageCandidate,
    PolicyFallback,
}

/// Policy for multi-word input whose first word is not a known command.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UnknownInputPolicy {
    #[default]
    Agent,
    Negotiate,
    Native,
}

/// Configurable deterministic router policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutePolicy {
    pub agent_prefixes: Vec<String>,
    pub unknown_input: UnknownInputPolicy,
    pub typo_distance: usize,
}

impl Default for RoutePolicy {
    fn default() -> Self {
        Self {
            agent_prefixes: vec!["?".to_owned(), "@".to_owned()],
            unknown_input: UnknownInputPolicy::Agent,
            typo_distance: 2,
        }
    }
}

/// Auditable result produced without a model or network call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteDecision {
    pub route: InputRoute,
    pub reason: RouteReason,
    pub payload: String,
    pub suggestion: Option<String>,
}

/// Commands visible through `PATH` plus caller-supplied shell built-ins.
#[derive(Clone, Debug, Default)]
pub struct CommandCatalog {
    commands: BTreeSet<String>,
}

impl CommandCatalog {
    /// Discovers executable names from the current process environment.
    #[must_use]
    pub fn discover() -> Self {
        let mut catalog = Self::default();
        if let Some(path) = env::var_os("PATH") {
            for directory in env::split_paths(&path) {
                catalog.add_directory(&directory);
            }
        }
        catalog
    }

    #[must_use]
    pub fn with_commands(commands: impl IntoIterator<Item = String>) -> Self {
        Self {
            commands: commands
                .into_iter()
                .map(|command| normalize_command(&command))
                .collect(),
        }
    }

    pub fn extend(&mut self, commands: impl IntoIterator<Item = String>) {
        self.commands
            .extend(commands.into_iter().map(|command| normalize_command(&command)));
    }

    fn add_directory(&mut self, directory: &Path) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_executable(&path)
                && let Some(name) = path.file_name().and_then(|name| name.to_str())
            {
                self.commands.insert(normalize_command(name));
            }
        }
    }

    /// Routes input using only syntax, executable discovery, and configured policy.
    #[must_use]
    pub fn route(&self, input: &str, policy: &RoutePolicy) -> RouteDecision {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return decision(InputRoute::Native, RouteReason::Empty, input, None);
        }
        if let Some(payload) = explicit_agent_payload(trimmed, &policy.agent_prefixes) {
            return decision(
                InputRoute::Agent,
                RouteReason::ExplicitAgentPrefix,
                payload,
                None,
            );
        }
        if has_shell_syntax(trimmed) {
            return decision(InputRoute::Native, RouteReason::ShellSyntax, input, None);
        }
        let Some(command) = command_word(trimmed) else {
            return decision(InputRoute::Native, RouteReason::ShellSyntax, input, None);
        };
        if looks_like_path(command) || self.contains(command) {
            return decision(InputRoute::Native, RouteReason::ResolvedCommand, input, None);
        }
        if let Some(suggestion) = self.suggest(command, policy.typo_distance) {
            return decision(
                InputRoute::Native,
                RouteReason::PossibleCommandTypo,
                input,
                Some(suggestion),
            );
        }
        if trimmed.split_whitespace().count() == 1 {
            return decision(
                InputRoute::Native,
                RouteReason::SingleUnknownToken,
                input,
                None,
            );
        }
        let route = match policy.unknown_input {
            UnknownInputPolicy::Agent => InputRoute::Agent,
            UnknownInputPolicy::Negotiate => InputRoute::Negotiate,
            UnknownInputPolicy::Native => InputRoute::Native,
        };
        decision(
            route,
            if matches!(route, InputRoute::Agent) {
                RouteReason::NaturalLanguageCandidate
            } else {
                RouteReason::PolicyFallback
            },
            input,
            None,
        )
    }

    fn contains(&self, command: &str) -> bool {
        self.commands.contains(&normalize_command(command))
    }

    fn suggest(&self, command: &str, maximum_distance: usize) -> Option<String> {
        let normalized = normalize_command(command);
        self.commands
            .iter()
            .filter_map(|candidate| {
                let distance = edit_distance(&normalized, candidate);
                (distance <= maximum_distance).then_some((distance, candidate))
            })
            .min_by(|left, right| left.cmp(right))
            .map(|(_, candidate)| candidate.clone())
    }
}

fn decision(
    route: InputRoute,
    reason: RouteReason,
    payload: impl Into<String>,
    suggestion: Option<String>,
) -> RouteDecision {
    RouteDecision {
        route,
        reason,
        payload: payload.into(),
        suggestion,
    }
}

fn explicit_agent_payload<'a>(input: &'a str, prefixes: &[String]) -> Option<&'a str> {
    prefixes.iter().find_map(|prefix| {
        input
            .strip_prefix(prefix)
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            .map(str::trim_start)
    })
}

fn command_word(input: &str) -> Option<&str> {
    input
        .split_whitespace()
        .find(|word| !is_environment_assignment(word))
}

fn is_environment_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .enumerate()
            .all(|(index, character)| character == '_' || character.is_ascii_alphanumeric() && (index > 0 || !character.is_ascii_digit()))
}

fn has_shell_syntax(input: &str) -> bool {
    input
        .chars()
        .any(|character| matches!(character, '|' | '&' | ';' | '<' | '>' | '$' | '`' | '(' | ')' | '\n'))
        || input.starts_with(['.', '/', '~'])
}

fn looks_like_path(command: &str) -> bool {
    command.contains('/') || command.contains('\\') || Path::new(command).is_absolute()
}

fn normalize_command(command: &str) -> String {
    let command = command.trim_matches(['\'', '"']);
    #[cfg(windows)]
    {
        let path = Path::new(command);
        return path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or(command)
            .to_ascii_lowercase();
    }
    #[cfg(not(windows))]
    command.to_owned()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "exe" | "cmd" | "bat" | "com"))
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    let mut current = vec![0; previous.len()];
    for (left_index, left_character) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_character) in right.chars().enumerate() {
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_character != right_character));
        }
        previous.clone_from_slice(&current);
    }
    previous[right.chars().count()]
}

#[cfg(test)]
mod tests {
    use super::{
        CommandCatalog, InputRoute, RoutePolicy, RouteReason, UnknownInputPolicy, edit_distance,
    };

    fn catalog() -> CommandCatalog {
        CommandCatalog::with_commands(["git".to_owned(), "cargo".to_owned(), "echo".to_owned()])
    }

    #[test]
    fn resolved_commands_always_remain_native() {
        let decision = catalog().route("git status", &RoutePolicy::default());
        assert_eq!(decision.route, InputRoute::Native);
        assert_eq!(decision.reason, RouteReason::ResolvedCommand);
    }

    #[test]
    fn explicit_agent_prefix_is_removed_from_payload() {
        let decision = catalog().route("? explain this error", &RoutePolicy::default());
        assert_eq!(decision.route, InputRoute::Agent);
        assert_eq!(decision.payload, "explain this error");
    }

    #[test]
    fn likely_typo_stays_native_and_carries_suggestion() {
        let decision = catalog().route("gti status", &RoutePolicy::default());
        assert_eq!(decision.route, InputRoute::Native);
        assert_eq!(decision.reason, RouteReason::PossibleCommandTypo);
        assert_eq!(decision.suggestion.as_deref(), Some("git"));
    }

    #[test]
    fn unresolved_phrase_uses_configured_policy() {
        let policy = RoutePolicy {
            unknown_input: UnknownInputPolicy::Negotiate,
            ..RoutePolicy::default()
        };
        assert_eq!(catalog().route("explain this project", &policy).route, InputRoute::Negotiate);
    }

    #[test]
    fn shell_operators_never_route_to_agent() {
        assert_eq!(catalog().route("unknown | less", &RoutePolicy::default()).route, InputRoute::Native);
    }

    #[test]
    fn edit_distance_is_deterministic() {
        assert_eq!(edit_distance("gti", "git"), 2);
    }
}
