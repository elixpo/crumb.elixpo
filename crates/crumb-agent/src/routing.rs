//! Deterministic, zero-AI routing between the native shell and agent runtime.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

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

/// Policy for input whose first word is not a known command.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownInputPolicy {
    #[default]
    Agent,
    Negotiate,
    Native,
}

/// Configurable deterministic router policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutePolicy {
    pub unknown_input: UnknownInputPolicy,
    pub typo_distance: usize,
}

impl Default for RoutePolicy {
    fn default() -> Self {
        Self {
            unknown_input: UnknownInputPolicy::Agent,
            typo_distance: 1,
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
    powershell_commands: bool,
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
            powershell_commands: false,
        }
    }

    /// Recognizes PowerShell's `Verb-Noun` command form as native shell input.
    pub fn enable_powershell_commands(&mut self) {
        self.powershell_commands = true;
    }

    pub fn extend(&mut self, commands: impl IntoIterator<Item = String>) {
        self.commands.extend(
            commands
                .into_iter()
                .map(|command| normalize_command(&command)),
        );
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
        if let Some(payload) = trimmed.strip_prefix(':')
            && !payload.trim().is_empty()
        {
            return decision(
                InputRoute::Agent,
                RouteReason::ExplicitAgentPrefix,
                payload.trim_start(),
                None,
            );
        }
        if has_shell_syntax(trimmed) {
            return decision(InputRoute::Native, RouteReason::ShellSyntax, input, None);
        }
        let Some(command) = command_word(trimmed) else {
            return decision(InputRoute::Native, RouteReason::ShellSyntax, input, None);
        };
        if looks_like_sentence(trimmed) {
            let route = route_for_unknown(policy.unknown_input);
            return decision(
                route,
                if matches!(route, InputRoute::Agent) {
                    RouteReason::NaturalLanguageCandidate
                } else {
                    RouteReason::PolicyFallback
                },
                input,
                None,
            );
        }
        if looks_like_path(command)
            || (self.powershell_commands && looks_like_powershell_command(command))
            || self.contains(command)
        {
            return decision(
                InputRoute::Native,
                RouteReason::ResolvedCommand,
                input,
                None,
            );
        }
        let single_token = trimmed.split_whitespace().count() == 1;
        let suggestion = self.suggest(command, policy.typo_distance);
        if let Some(suggestion) = &suggestion
            && !single_token
        {
            return decision(
                InputRoute::Native,
                RouteReason::PossibleCommandTypo,
                input,
                Some(suggestion.clone()),
            );
        }
        let route = route_for_unknown(policy.unknown_input);
        decision(
            route,
            if single_token {
                RouteReason::SingleUnknownToken
            } else if matches!(route, InputRoute::Agent) {
                RouteReason::NaturalLanguageCandidate
            } else {
                RouteReason::PolicyFallback
            },
            input,
            suggestion,
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
            .min_by(std::cmp::Ord::cmp)
            .map(|(_, candidate)| candidate.clone())
    }
}

const fn route_for_unknown(policy: UnknownInputPolicy) -> InputRoute {
    match policy {
        UnknownInputPolicy::Agent => InputRoute::Agent,
        UnknownInputPolicy::Negotiate => InputRoute::Negotiate,
        UnknownInputPolicy::Native => InputRoute::Native,
    }
}

fn looks_like_sentence(input: &str) -> bool {
    let words = input.split_whitespace().collect::<Vec<_>>();
    if words.len() < 5
        || words.iter().any(|word| {
            word.starts_with('-')
                || word.contains('=')
                || !word
                    .trim_matches(['.', ',', '!', '?', ':'])
                    .chars()
                    .all(char::is_alphabetic)
        })
    {
        return false;
    }
    let lengths = words
        .iter()
        .map(|word| word.trim_matches(['.', ',', '!', '?', ':']).chars().count());
    lengths.clone().filter(|length| *length <= 3).count() >= 2
        && lengths.filter(|length| *length >= 5).count() >= 2
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
        && name.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphanumeric() && (index > 0 || !character.is_ascii_digit())
        })
}

fn has_shell_syntax(input: &str) -> bool {
    input.chars().any(|character| {
        matches!(
            character,
            '|' | '&' | ';' | '<' | '>' | '$' | '`' | '(' | ')' | '\n'
        )
    }) || input.starts_with(['.', '/', '~'])
}

fn looks_like_path(command: &str) -> bool {
    command.contains('/') || command.contains('\\') || Path::new(command).is_absolute()
}

fn looks_like_powershell_command(command: &str) -> bool {
    let Some((verb, noun)) = command.split_once('-') else {
        return false;
    };
    !verb.is_empty()
        && !noun.is_empty()
        && verb
            .chars()
            .all(|character| character.is_ascii_alphabetic())
        && noun
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
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
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "exe" | "cmd" | "bat" | "com"
                )
            })
}

fn edit_distance(left: &str, right: &str) -> usize {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    let mut distance = vec![vec![0; right.len() + 1]; left.len() + 1];
    for (index, row) in distance.iter_mut().enumerate() {
        row[0] = index;
    }
    for (index, cell) in distance[0].iter_mut().enumerate() {
        *cell = index;
    }
    for (left_index, left_character) in left.iter().enumerate() {
        for (right_index, right_character) in right.iter().enumerate() {
            let row = left_index + 1;
            let column = right_index + 1;
            distance[row][column] = (distance[row - 1][column] + 1)
                .min(distance[row][column - 1] + 1)
                .min(
                    distance[row - 1][column - 1] + usize::from(left_character != right_character),
                );
            if row > 1
                && column > 1
                && left[row - 1] == right[column - 2]
                && left[row - 2] == right[column - 1]
            {
                distance[row][column] =
                    distance[row][column].min(distance[row - 2][column - 2] + 1);
            }
        }
    }
    distance[left.len()][right.len()]
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
    fn sentence_shape_wins_over_an_accidental_executable_name_collision() {
        let catalog = CommandCatalog::with_commands(["what".to_owned()]);
        let decision = catalog.route("what is the folder about", &RoutePolicy::default());

        assert_eq!(decision.route, InputRoute::Agent);
        assert_eq!(decision.reason, RouteReason::NaturalLanguageCandidate);
    }

    #[test]
    fn natural_language_routes_without_a_prefix() {
        let decision = catalog().route("explain this error", &RoutePolicy::default());
        assert_eq!(decision.route, InputRoute::Agent);
        assert_eq!(decision.payload, "explain this error");
        assert_eq!(decision.reason, RouteReason::NaturalLanguageCandidate);
    }

    #[test]
    fn colon_forces_agent_routing_and_is_removed_from_payload() {
        let decision = catalog().route(": run git status", &RoutePolicy::default());

        assert_eq!(decision.route, InputRoute::Agent);
        assert_eq!(decision.reason, RouteReason::ExplicitAgentPrefix);
        assert_eq!(decision.payload, "run git status");
    }

    #[test]
    fn conversational_single_word_uses_the_unknown_input_policy() {
        let catalog = CommandCatalog::with_commands(["help".to_owned()]);
        let decision = catalog.route("hello", &RoutePolicy::default());

        assert_eq!(decision.route, InputRoute::Agent);
        assert_eq!(decision.reason, RouteReason::SingleUnknownToken);
        assert!(decision.suggestion.is_none());
    }

    #[test]
    fn ambiguous_single_word_typo_uses_the_unknown_input_policy() {
        let catalog = CommandCatalog::with_commands(["help".to_owned()]);
        let decision = catalog.route("helo", &RoutePolicy::default());

        assert_eq!(decision.route, InputRoute::Agent);
        assert_eq!(decision.reason, RouteReason::SingleUnknownToken);
        assert_eq!(decision.suggestion.as_deref(), Some("help"));
    }

    #[test]
    fn slash_namespace_never_routes_to_agent() {
        let decision = catalog().route("/connectors pollinations", &RoutePolicy::default());
        assert_eq!(decision.route, InputRoute::Native);
        assert_eq!(decision.reason, RouteReason::ShellSyntax);
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
        assert_eq!(
            catalog().route("explain this project", &policy).route,
            InputRoute::Negotiate
        );
    }

    #[test]
    fn shell_operators_never_route_to_agent() {
        assert_eq!(
            catalog()
                .route("unknown | less", &RoutePolicy::default())
                .route,
            InputRoute::Native
        );
    }

    #[test]
    fn powershell_command_shape_is_platform_opt_in() {
        let mut catalog = catalog();
        assert_eq!(
            catalog
                .route("Get-ChildItem files", &RoutePolicy::default())
                .route,
            InputRoute::Agent
        );
        catalog.enable_powershell_commands();
        assert_eq!(
            catalog
                .route("Get-ChildItem files", &RoutePolicy::default())
                .route,
            InputRoute::Native
        );
    }

    #[test]
    fn edit_distance_is_deterministic() {
        assert_eq!(edit_distance("gti", "git"), 1);
        assert_eq!(edit_distance("hello", "help"), 2);
    }
}
