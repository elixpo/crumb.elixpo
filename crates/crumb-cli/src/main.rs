use std::borrow::Cow;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use crumb_agent::{
    AgentConfig, AgentMode, CancellationToken, CommandCatalog, DenyAllApprovals, HarnessConfig,
    InputRoute, LiveConfig, MistakePolicy, Modality, RouteDecision, ToolHost, TurnStatus,
    UnknownInputPolicy, list_sessions, session_summary,
};
use crumb_auth::{CredentialSource, CredentialStore, OsCredentialStore, credential_status, login};
use crumb_core::{AuthAction, BuiltInCommand, HistoryAction, InputEvent};
use crumb_harness_dsh::Notification;
use crumb_history::{HistoryEntry, HistoryMode, HistoryStore, RecordContext};
use crumb_mcp::{McpDispatcher, serve_stdio};
use crumb_native::session::{CommandOutcome, ShellSession};
use crumb_native::shell_for;
use crumb_platform::Platform;
use crumb_pty::{PtyInput, PtyResizer, SystemPty, TerminalSize};
use crumb_repl::{ReplOutcome, read_classified_line};
use crumb_tools::{WorkspaceToolLimits, register_workspace_read_tools};
use crumb_ui::{GitSegment, PromptContext, Renderer, UiSettings};
use reedline::{
    ColumnarMenu, Emacs, FileBackedHistory, History, HistoryItem, KeyCode, KeyModifiers,
    MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, default_emacs_keybindings,
};

mod agent_runtime;
mod completion;
mod device_auth;

use agent_runtime::AgentRuntime;
use completion::{CompletionWorkspace, CrumbCompleter};

const INTERACTIVE_HISTORY_CAPACITY: usize = 1_000;

fn main() -> Result<()> {
    if run_command_line_action()? {
        return Ok(());
    }
    if run_managed_repl()? == ReplOutcome::LaunchNativeShell {
        run_native_shell()?;
    }

    Ok(())
}

fn run_command_line_action() -> Result<bool> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if matches!(arguments.as_slice(), [group, action] if group == "mcp" && action == "serve") {
        serve_mcp()?;
        return Ok(true);
    }
    let action = match arguments.as_slice() {
        [] => return Ok(false),
        [group, action] if group == "auth" && action == "login" => AuthAction::Login,
        [group, action] if group == "auth" && action == "status" => AuthAction::Status,
        [group, action] if group == "auth" && action == "logout" => AuthAction::Logout,
        _ => {
            return Err(anyhow!(
                "usage: crumb [auth <login|status|logout> | mcp serve]"
            ));
        }
    };
    handle_auth(action, &mut io::stdout().lock())?;
    Ok(true)
}

fn serve_mcp() -> Result<()> {
    let workspace = current_process_dir()?;
    let config = read_agent_config(&workspace)?;
    let host = workspace_read_host(&workspace, &config)?;
    let dispatcher = McpDispatcher::new(
        host,
        Arc::new(DenyAllApprovals),
        config.mode,
        env!("CARGO_PKG_VERSION"),
    );
    let cancellation = CancellationToken::default();
    serve_stdio(
        &dispatcher,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &cancellation,
    )
}

fn workspace_read_host(workspace: &Path, config: &AgentConfig) -> Result<ToolHost> {
    let max_output_bytes = usize::try_from(config.limits.max_output_bytes)
        .map_err(|_| anyhow!("max_output_bytes exceeds this platform's address space"))?;
    let max_directory_entries = usize::try_from(config.limits.max_directory_entries)
        .map_err(|_| anyhow!("max_directory_entries exceeds this platform's address space"))?;
    let mut host = ToolHost::default();
    register_workspace_read_tools(
        &mut host,
        workspace,
        WorkspaceToolLimits {
            max_output_bytes,
            max_directory_entries,
        },
    )?;
    Ok(host)
}

fn run_managed_repl() -> Result<ReplOutcome> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let interactive = stdin.is_terminal() && stdout.is_terminal();
    let renderer = Renderer::new(UiSettings::from_environment(interactive));
    let platform = Platform::current();
    let mut command_catalog = CommandCatalog::discover();
    command_catalog.extend(
        shell_for(platform)
            .builtin_commands()
            .iter()
            .map(|command| (*command).to_owned()),
    );
    if matches!(platform, Platform::Windows) {
        command_catalog.enable_powershell_commands();
    }
    let mut session: Option<ShellSession> = None;
    let mut agent_runtime: Option<AgentRuntime> = None;
    let mut last_exit_code = None;
    let history = open_history(&mut stdout.lock())?;
    let completion_workspace = CompletionWorkspace::new(current_process_dir()?);
    let mut line_editor = interactive
        .then(|| create_line_editor(history.as_ref(), completion_workspace.clone()))
        .transpose()?;

    let branding = renderer.branding();
    if !branding.is_empty() {
        writeln!(stdout.lock(), "{branding}")?;
    }

    loop {
        let cwd = session
            .as_ref()
            .map_or_else(current_process_dir, |shell| Ok(shell.cwd().to_path_buf()))?;
        let prompt = render_prompt(renderer, &cwd, platform, last_exit_code);
        let event = if let Some(editor) = line_editor.as_mut() {
            editor.workspace.set(&cwd);
            match editor.editor.read_line(&CrumbPrompt::new(prompt))? {
                Signal::Success(command) => Some(crumb_repl::classify_input(&command)),
                Signal::CtrlD => None,
                _ => continue,
            }
        } else {
            let mut writer = stdout.lock();
            writer.write_all(prompt.as_bytes())?;
            writer.flush()?;
            read_classified_line(&mut stdin.lock())?
        };
        let Some(event) = event else {
            shutdown_session(session)?;
            return Ok(ReplOutcome::Exit);
        };
        let mut writer = stdout.lock();

        match event {
            InputEvent::BuiltIn(command) => {
                if let Some(outcome) = handle_builtin(
                    command,
                    &mut session,
                    &mut agent_runtime,
                    history.as_ref(),
                    &cwd,
                    platform,
                    &mut writer,
                )? {
                    return Ok(outcome);
                }
            }
            InputEvent::NativeInput(command) if command.trim().is_empty() => {}
            InputEvent::NativeInput(command) => {
                let mut context = InputContext {
                    command_catalog: &command_catalog,
                    agent_runtime: &mut agent_runtime,
                    session: &mut session,
                    history: history.as_ref(),
                    cwd: &cwd,
                    platform,
                    interactive,
                    renderer,
                    writer: &mut writer,
                    last_exit_code: &mut last_exit_code,
                };
                if let Some(outcome) = handle_input(&command, &mut context)? {
                    return Ok(outcome);
                }
            }
        }
    }
}

struct InputContext<'a> {
    command_catalog: &'a CommandCatalog,
    agent_runtime: &'a mut Option<AgentRuntime>,
    session: &'a mut Option<ShellSession>,
    history: Option<&'a HistoryStore>,
    cwd: &'a Path,
    platform: Platform,
    interactive: bool,
    renderer: Renderer,
    writer: &'a mut dyn Write,
    last_exit_code: &'a mut Option<i32>,
}

fn handle_input(command: &str, context: &mut InputContext<'_>) -> Result<Option<ReplOutcome>> {
    let agent_config = load_agent_config(context.cwd, context.writer);
    let decision = context
        .command_catalog
        .route(command, &agent_config.routing);
    if !matches!(decision.route, InputRoute::Native) {
        handle_agent_boundary(
            &decision,
            &agent_config,
            context.cwd,
            context.agent_runtime,
            context.renderer,
            context.writer,
        )?;
        record_history(
            context.history,
            command,
            context.cwd,
            context.platform,
            HistoryMode::Agent,
            None,
            context.writer,
        )?;
        return Ok(None);
    }
    if context.session.is_none() {
        let (cols, rows) = size()?;
        let shell = shell_for(context.platform);
        *context.session = Some(ShellSession::start(
            shell.as_ref(),
            &SystemPty,
            TerminalSize::new(rows, cols),
        )?);
    }
    let shell = context
        .session
        .as_mut()
        .expect("shell session is initialized above");
    let outcome = if context.interactive {
        execute_foreground(shell, command, context.writer)?
    } else {
        shell.execute(command, context.writer)?
    };
    match outcome {
        CommandOutcome::Completed(completion) => {
            *context.last_exit_code = Some(completion.exit_code);
            if completion.exit_code != 0 {
                render_error_assistance(command, &decision, agent_config.mistakes, context.writer)?;
            }
            record_history(
                context.history,
                command,
                context.cwd,
                context.platform,
                HistoryMode::Native,
                Some(completion.exit_code),
                context.writer,
            )?;
            Ok(None)
        }
        CommandOutcome::ShellExited => {
            record_history(
                context.history,
                command,
                context.cwd,
                context.platform,
                HistoryMode::Native,
                None,
                context.writer,
            )?;
            Ok(Some(ReplOutcome::Exit))
        }
    }
}

fn load_agent_config(cwd: &Path, writer: &mut dyn Write) -> AgentConfig {
    match read_agent_config(cwd) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(
                writer,
                "warning: agent config is invalid; unresolved input will stay native: {error}"
            );
            let mut config = AgentConfig::default();
            config.routing.unknown_input = UnknownInputPolicy::Native;
            config
        }
    }
}

fn read_agent_config(cwd: &Path) -> Result<AgentConfig> {
    let (path, config_root) = agent_config_location(cwd);
    let mut config = LiveConfig::new(path).load_or_default()?;
    if let Some(HarnessConfig::Process {
        cordis: Some(cordis),
        ..
    }) = &mut config.harness
        && cordis.is_relative()
    {
        *cordis = config_root.join(cordis.as_path());
    }
    Ok(config)
}

fn agent_config_location(cwd: &Path) -> (PathBuf, PathBuf) {
    for directory in cwd.ancestors() {
        let candidate = directory.join(".crumb").join("agent.json");
        if candidate.is_file() {
            return (candidate, directory.to_path_buf());
        }
    }
    (cwd.join(".crumb").join("agent.json"), cwd.to_path_buf())
}

fn handle_agent_boundary(
    decision: &RouteDecision,
    config: &AgentConfig,
    workspace: &Path,
    runtime: &mut Option<AgentRuntime>,
    renderer: Renderer,
    writer: &mut dyn Write,
) -> Result<()> {
    if matches!(decision.route, InputRoute::Native) {
        unreachable!("native input is handled by the shell path");
    }
    if runtime.is_none() {
        match AgentRuntime::new() {
            Ok(created) => *runtime = Some(created),
            Err(error) => {
                writeln!(
                    writer,
                    "{}",
                    renderer.agent_error(&error.to_string(), false)
                )?;
                return Ok(());
            }
        }
    }

    let route = config
        .models
        .get(&Modality::Text)
        .and_then(|routes| routes.first());
    let model = route.map_or("not configured".to_owned(), |route| {
        format!("{}/{}", route.provider, route.model)
    });
    let effort = route.and_then(|route| config.reasoning_effort_for(route));
    writeln!(
        writer,
        "{}",
        renderer.agent_header(&model, effort, agent_mode_name(config.mode), None)
    )?;
    writer.flush()?;
    let mut activity = Some(renderer.activity("Working through Harness"));
    let result = runtime
        .as_mut()
        .expect("agent runtime is initialized above")
        .run_with_events(&decision.payload, config, workspace, |notification| {
            if let Some(label) = harness_event_label(notification) {
                if let Some(indicator) = activity.take() {
                    indicator.finish();
                }
                writeln!(writer, "  ↳ {label}")?;
                writer.flush()?;
            }
            Ok(())
        });
    if let Some(indicator) = activity.take() {
        indicator.finish();
    }

    match result {
        Ok(result) if result.final_response.trim().is_empty() => {
            writeln!(
                writer,
                "{}",
                renderer.agent_response(
                    "Turn completed without a text response.",
                    &result.session_id,
                    result.events.len(),
                )
            )?;
        }
        Ok(result) => writeln!(
            writer,
            "{}",
            renderer.agent_response(
                &result.final_response,
                &result.session_id,
                result.events.len(),
            )
        )?,
        Err(error) => {
            let message = error.to_string();
            let cancelled = message.to_ascii_lowercase().contains("cancel");
            writeln!(writer, "{}", renderer.agent_error(&message, cancelled))?;
        }
    }
    Ok(())
}

fn harness_event_label(notification: &Notification) -> Option<String> {
    if notification.method == "session.status" {
        return notification
            .params
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(safe_event_label)
            .map(|status| format!("session {status}"));
    }
    if notification.method != "session.event" {
        return None;
    }
    let event_type = notification.params.get("event")?.get("type")?.as_str()?;
    match event_type {
        "agent/inbox/spliced" => Some("request accepted".to_owned()),
        "assistant/message" | "turn/end" => None,
        other => safe_event_label(other),
    }
}

fn safe_event_label(value: &str) -> Option<String> {
    let label = value
        .chars()
        .filter_map(|character| match character {
            '/' | '-' | '_' => Some(' '),
            character if character.is_ascii_alphanumeric() || character == ' ' => Some(character),
            _ => None,
        })
        .take(64)
        .collect::<String>();
    (!label.trim().is_empty()).then(|| label.trim().to_owned())
}

const fn agent_mode_name(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Auto => "auto",
        AgentMode::Negotiate => "negotiate",
        AgentMode::Plan => "plan",
    }
}

fn render_error_assistance(
    command: &str,
    decision: &RouteDecision,
    policy: MistakePolicy,
    writer: &mut dyn Write,
) -> Result<()> {
    if matches!(policy, MistakePolicy::Disabled) {
        return Ok(());
    }
    if let Some(suggestion) = &decision.suggestion {
        writeln!(
            writer,
            "help: `{}` may be a typo for `{suggestion}`",
            command.split_whitespace().next().unwrap_or(command)
        )?;
    }
    match policy {
        MistakePolicy::Prompt => writeln!(
            writer,
            "help: describe the error in plain English for AI assistance"
        )?,
        MistakePolicy::Automatic => writeln!(
            writer,
            "help: automatic AI diagnosis will start when the agent runtime is enabled"
        )?,
        MistakePolicy::Disabled => {}
    }
    Ok(())
}

fn open_history(writer: &mut dyn Write) -> Result<Option<HistoryStore>> {
    match HistoryStore::open_default() {
        Ok(store) => Ok(Some(store)),
        Err(error) => {
            writeln!(writer, "warning: command history is unavailable: {error}")?;
            Ok(None)
        }
    }
}

fn render_prompt(
    renderer: Renderer,
    cwd: &Path,
    platform: Platform,
    last_exit_code: Option<i32>,
) -> String {
    let git = GitSegment::discover(cwd);
    renderer.prompt(&PromptContext {
        cwd,
        platform,
        git: git.as_ref(),
        last_exit_code,
    })
}

fn handle_builtin(
    command: BuiltInCommand,
    session: &mut Option<ShellSession>,
    agent_runtime: &mut Option<AgentRuntime>,
    history: Option<&HistoryStore>,
    cwd: &std::path::Path,
    platform: Platform,
    writer: &mut dyn Write,
) -> Result<Option<ReplOutcome>> {
    match command {
        BuiltInCommand::Auth(action) => handle_auth(action, writer)?,
        BuiltInCommand::Connectors => handle_auth(AuthAction::Status, writer)?,
        BuiltInCommand::Context => show_reference_help(writer)?,
        BuiltInCommand::Exit => {
            shutdown_session(session.take())?;
            return Ok(Some(ReplOutcome::Exit));
        }
        BuiltInCommand::Help => show_slash_help(writer)?,
        BuiltInCommand::History(action) => show_history(history, &action, writer)?,
        BuiltInCommand::Platform => {
            writeln!(writer, "{platform}")?;
            record_history(
                history,
                "/platform",
                cwd,
                platform,
                HistoryMode::BuiltIn,
                Some(0),
                writer,
            )?;
        }
        BuiltInCommand::Reserved(command) => {
            show_reserved(&command, cwd, agent_runtime, writer)?;
        }
        BuiltInCommand::Version => {
            writeln!(writer, "crumb {}", env!("CARGO_PKG_VERSION"))?;
            record_history(
                history,
                "/version",
                cwd,
                platform,
                HistoryMode::BuiltIn,
                Some(0),
                writer,
            )?;
        }
        BuiltInCommand::Shell if session.is_none() => {
            return Ok(Some(ReplOutcome::LaunchNativeShell));
        }
        BuiltInCommand::Shell => writeln!(
            writer,
            "`/shell` is available before the managed shell starts; restart crumb to enter raw mode"
        )?,
        BuiltInCommand::Skills => show_skills(cwd, writer)?,
    }
    Ok(None)
}

fn show_slash_help(writer: &mut dyn Write) -> Result<()> {
    writeln!(writer, "Crumb command palette")?;
    writeln!(
        writer,
        "  Type `/` then Tab to search · `@` then Tab for context"
    )?;
    for (title, roots) in [
        (
            "SHELL",
            &[
                "/help",
                "/history",
                "/platform",
                "/version",
                "/shell",
                "/exit",
            ][..],
        ),
        (
            "AGENT",
            &["/mode", "/model", "/effort", "/session", "/cancel", "/cost"][..],
        ),
        (
            "CONTEXT & CAPABILITIES",
            &[
                "/context",
                "/attach",
                "/detach",
                "/skills",
                "/plugins",
                "/tools",
                "/permissions",
                "/memory",
            ][..],
        ),
        (
            "ACCOUNT & SYSTEM",
            &["/auth", "/connectors", "/config", "/doctor"][..],
        ),
    ] {
        writeln!(writer, "\n  {title}")?;
        for command in crumb_repl::SLASH_COMMANDS.iter().filter(|command| {
            roots.iter().any(|root| {
                command
                    .usage
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(' '))
            })
        }) {
            writeln!(writer, "    {:<22} {}", command.usage, command.description)?;
        }
    }
    Ok(())
}

fn show_reference_help(writer: &mut dyn Write) -> Result<()> {
    writeln!(
        writer,
        "Inline references (type `@` then Tab inside a request):"
    )?;
    for reference in [
        "@file:<path>",
        "@folder:<path>",
        "@selection",
        "@clipboard",
        "@last-error",
        "@diff",
        "@session:<id>",
        "@skill:<id>",
        "@plugin:<id>",
        "@connector:pollinations",
    ] {
        writeln!(writer, "  {reference}")?;
    }
    Ok(())
}

fn show_skills(cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let config = read_agent_config(cwd)?;
    if config.skills.is_empty() {
        writeln!(writer, "◇ No skills configured")?;
        writeln!(
            writer,
            "  Add skills to .crumb/agent.json, then type @skill: and press Tab."
        )?;
        return Ok(());
    }
    for skill in config.skills {
        writeln!(
            writer,
            "{}  {}",
            if skill.enabled {
                "enabled "
            } else {
                "disabled"
            },
            skill.id
        )?;
    }
    Ok(())
}

fn show_reserved(
    command: &str,
    cwd: &Path,
    runtime: &mut Option<AgentRuntime>,
    writer: &mut dyn Write,
) -> Result<()> {
    match command {
        "/mode" => {
            let config = read_agent_config(cwd)?;
            writeln!(writer, "◆ Agent mode")?;
            writeln!(writer, "  {}", agent_mode_name(config.mode))?;
            writeln!(
                writer,
                "  auto executes approved steps · negotiate pauses · plan is read-only"
            )?;
        }
        "/model" => show_models(cwd, writer)?,
        "/effort" => {
            let config = read_agent_config(cwd)?;
            let text_route = config
                .models
                .get(&Modality::Text)
                .and_then(|routes| routes.first());
            let effort = text_route
                .and_then(|route| config.reasoning_effort_for(route))
                .unwrap_or("provider default");
            writeln!(writer, "◆ Reasoning effort")?;
            writeln!(writer, "  {effort}")?;
        }
        "/config" => show_config_summary(cwd, writer)?,
        "/plugins" => show_plugins(cwd, writer)?,
        command if command == "/session" || command.starts_with("/session ") => {
            show_sessions(command, cwd, runtime, writer)?;
        }
        _ => {
            writeln!(writer, "◇ {command}")?;
            writeln!(writer, "  Reserved by Crumb; not available in this build.")?;
        }
    }
    Ok(())
}

fn show_sessions(
    command: &str,
    cwd: &Path,
    runtime: &mut Option<AgentRuntime>,
    writer: &mut dyn Write,
) -> Result<()> {
    let root = cwd.join(".crumb").join("sessions").join("crumb");
    let arguments = command.split_whitespace().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["list"] => {
            let sessions = list_sessions(&root)?;
            writeln!(writer, "◆ Agent sessions")?;
            if sessions.is_empty() {
                writeln!(writer, "  No sessions in this workspace")?;
            }
            for summary in sessions.into_iter().take(20) {
                writeln!(
                    writer,
                    "  {:<34} {:<10} {} turns",
                    summary.id.as_str(),
                    turn_status_name(summary.last_status),
                    summary.turns
                )?;
            }
        }
        ["inspect", id] => {
            let summary = session_summary(&root, id)?;
            writeln!(writer, "◆ Session {}", summary.id.as_str())?;
            writeln!(writer, "  workspace  {}", summary.workspace.display())?;
            writeln!(writer, "  mode       {}", agent_mode_name(summary.mode))?;
            writeln!(writer, "  turns      {}", summary.turns)?;
            writeln!(
                writer,
                "  status     {}",
                turn_status_name(summary.last_status)
            )?;
            writeln!(writer, "  started    {} ms", summary.started_at_ms)?;
            writeln!(writer, "  last event {} ms", summary.last_event_at_ms)?;
        }
        ["resume", id] => {
            if runtime.is_none() {
                *runtime = Some(AgentRuntime::new()?);
            }
            runtime
                .as_mut()
                .context("agent runtime is unavailable")?
                .resume(cwd, id)?;
            writeln!(writer, "◆ Resumed session {id}")?;
        }
        _ => writeln!(
            writer,
            "usage: /session [list | inspect <id> | resume <id>]"
        )?,
    }
    Ok(())
}

const fn turn_status_name(status: Option<TurnStatus>) -> &'static str {
    match status {
        Some(TurnStatus::Complete) => "complete",
        Some(TurnStatus::Cancelled) => "cancelled",
        Some(TurnStatus::Failed) => "failed",
        Some(TurnStatus::LimitReached) => "limit",
        None => "new",
    }
}

fn show_models(cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let config = read_agent_config(cwd)?;
    writeln!(writer, "◆ Model routes")?;
    if config.models.values().all(Vec::is_empty) {
        writeln!(writer, "  No model routes configured in .crumb/agent.json")?;
        return Ok(());
    }
    for (modality, routes) in &config.models {
        for (index, route) in routes.iter().enumerate() {
            let marker = if index == 0 { "●" } else { "○" };
            let effort = route
                .reasoning_effort
                .as_deref()
                .or(config.reasoning_effort.as_deref())
                .map_or(String::new(), |effort| format!(" · effort {effort}"));
            writeln!(
                writer,
                "  {marker} {:<13} {}/{}{}",
                modality_name(*modality),
                route.provider,
                route.model,
                effort
            )?;
        }
    }
    writeln!(writer, "  ● active route · ○ fallback")?;
    Ok(())
}

fn show_config_summary(cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let (path, _) = agent_config_location(cwd);
    let config = read_agent_config(cwd)?;
    let routes = config.models.values().map(Vec::len).sum::<usize>();
    let enabled_skills = config.skills.iter().filter(|skill| skill.enabled).count();
    let enabled_plugins = config
        .mcp_servers
        .iter()
        .filter(|server| server.enabled)
        .count();
    writeln!(writer, "◆ Live configuration")?;
    writeln!(writer, "  {}", path.display())?;
    writeln!(
        writer,
        "  mode {} · {routes} model routes · {enabled_skills} skills · {enabled_plugins} plugins",
        agent_mode_name(config.mode)
    )?;
    writeln!(
        writer,
        "  Reloaded before every agent turn · secrets excluded"
    )?;
    Ok(())
}

fn show_plugins(cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let config = read_agent_config(cwd)?;
    writeln!(writer, "◆ Plugins and MCP servers")?;
    if config.mcp_servers.is_empty() {
        writeln!(writer, "  No plugins configured in .crumb/agent.json")?;
        return Ok(());
    }
    for server in config.mcp_servers {
        writeln!(
            writer,
            "  {} {}",
            if server.enabled { "●" } else { "○" },
            server.id
        )?;
    }
    writeln!(writer, "  ● enabled · ○ disabled")?;
    Ok(())
}

const fn modality_name(modality: Modality) -> &'static str {
    match modality {
        Modality::Text => "text",
        Modality::WebSearch => "web search",
        Modality::Image => "image",
        Modality::Video => "video",
        Modality::Audio => "audio",
        Modality::ThreeD => "3d",
        Modality::Transcription => "transcription",
        Modality::Embeddings => "embeddings",
    }
}

fn handle_auth(action: AuthAction, writer: &mut dyn Write) -> Result<()> {
    match action {
        AuthAction::Login => {
            let store = OsCredentialStore::new()?;
            let secret = device_auth::connect(writer)?;
            login(&store, &secret)?;
            writeln!(
                writer,
                "Pollinations account connected and saved in the OS credential store"
            )?;
        }
        AuthAction::Status => {
            let environment = std::env::var("POLLINATIONS_API_KEY").ok();
            if environment
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                writeln!(writer, "Pollinations BYOK configured (environment)")?;
                return Ok(());
            }
            let store = OsCredentialStore::new()?;
            let status = credential_status(&store, None)?;
            match status.source {
                Some(CredentialSource::Keyring) => {
                    writeln!(writer, "Pollinations BYOK configured (OS credential store)")?;
                }
                Some(CredentialSource::Environment) => {
                    unreachable!("handled before keyring access")
                }
                None => writeln!(writer, "Pollinations BYOK is not configured")?,
            }
        }
        AuthAction::Logout => {
            let store = OsCredentialStore::new()?;
            if store.delete()? {
                writeln!(
                    writer,
                    "Pollinations BYOK removed from the OS credential store"
                )?;
            } else {
                writeln!(writer, "Pollinations BYOK was not stored")?;
            }
            if std::env::var_os("POLLINATIONS_API_KEY").is_some() {
                writeln!(
                    writer,
                    "POLLINATIONS_API_KEY remains active for this process"
                )?;
            }
        }
    }
    Ok(())
}

fn execute_foreground(
    shell: &mut ShellSession,
    command: &str,
    output: &mut dyn Write,
) -> Result<CommandOutcome> {
    let input = shell.try_clone_input()?;
    let resizer = shell.resizer();
    let running = Arc::new(AtomicBool::new(true));
    let relay_running = Arc::clone(&running);
    let _raw_mode = RawModeGuard::enable()?;
    let submitted = shell.submit(command)?;

    thread::scope(|scope| {
        let relay = scope.spawn(move || relay_foreground_input(input, &resizer, &relay_running));
        let outcome = submitted.wait(output);
        running.store(false, Ordering::Relaxed);
        let relay_result = relay
            .join()
            .map_err(|_| anyhow!("foreground input relay panicked"))?;
        relay_result?;
        outcome
    })
}

fn relay_foreground_input(
    mut destination: PtyInput,
    resizer: &PtyResizer,
    running: &AtomicBool,
) -> io::Result<()> {
    let mut stdin = io::stdin();
    let mut previous_size = size()?;
    let mut buffer = [0_u8; 8192];

    while running.load(Ordering::Relaxed) {
        if stdin_ready(&stdin)? {
            let bytes_read = stdin.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            destination.write_all(&buffer[..bytes_read])?;
            destination.flush()?;
        }

        if let Ok(current @ (cols, rows)) = size()
            && current != previous_size
        {
            resizer
                .resize(TerminalSize::new(rows, cols))
                .map_err(io::Error::other)?;
            previous_size = current;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn stdin_ready(stdin: &io::Stdin) -> io::Result<bool> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: 25_000_000,
    };
    let mut descriptors = [PollFd::new(stdin, PollFlags::IN)];
    poll(&mut descriptors, Some(&timeout)).map_err(io::Error::from)?;
    Ok(descriptors[0].revents().contains(PollFlags::IN))
}

#[cfg(not(target_os = "linux"))]
fn stdin_ready(_stdin: &io::Stdin) -> io::Result<bool> {
    crossterm::event::poll(Duration::from_millis(25))
}

struct InteractiveLineEditor {
    editor: Reedline,
    workspace: CompletionWorkspace,
}

fn create_line_editor(
    history: Option<&HistoryStore>,
    workspace: CompletionWorkspace,
) -> Result<InteractiveLineEditor> {
    let mut interactive_history = FileBackedHistory::new(INTERACTIVE_HISTORY_CAPACITY)?;
    if let Some(history) = history {
        let capacity = u32::try_from(INTERACTIVE_HISTORY_CAPACITY)
            .map_err(|_| anyhow!("interactive history capacity exceeds u32"))?;
        let mut entries = history.recent(capacity)?;
        entries.reverse();
        for entry in entries {
            interactive_history.save(HistoryItem::from_command_line(entry.command))?;
        }
    }
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_owned()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let menu = ColumnarMenu::default()
        .with_name("completion_menu")
        .with_columns(1);
    let editor = Reedline::create()
        .with_history(Box::new(interactive_history))
        .with_completer(Box::new(CrumbCompleter::new(workspace.clone())))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)));
    Ok(InteractiveLineEditor { editor, workspace })
}

struct CrumbPrompt {
    rendered: String,
}

impl CrumbPrompt {
    const fn new(rendered: String) -> Self {
        Self { rendered }
    }
}

impl Prompt for CrumbPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.rendered)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("::: ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let status = match history_search.status {
            PromptHistorySearchStatus::Passing => "reverse-search",
            PromptHistorySearchStatus::Failing => "failing reverse-search",
        };
        Cow::Owned(format!("({status}: {}) ", history_search.term))
    }
}

fn show_history(
    history: Option<&HistoryStore>,
    action: &HistoryAction,
    writer: &mut dyn Write,
) -> Result<()> {
    let Some(history) = history else {
        writeln!(writer, "history is unavailable")?;
        return Ok(());
    };
    let result = match action {
        HistoryAction::Recent => history.recent(20),
        HistoryAction::Search(query) if query.trim().is_empty() => {
            writeln!(writer, "usage: /history search <text>")?;
            return Ok(());
        }
        HistoryAction::Search(query) => history.search(query, 20),
    };
    match result {
        Ok(entries) if entries.is_empty() => writeln!(writer, "no history entries")?,
        Ok(entries) => {
            for entry in entries {
                writeln!(writer, "{}", format_history_entry(&entry))?;
            }
        }
        Err(error) => writeln!(writer, "warning: history query failed: {error}")?,
    }
    Ok(())
}

fn format_history_entry(entry: &HistoryEntry) -> String {
    let exit = entry
        .exit_code
        .map_or_else(|| "-".to_owned(), |code| code.to_string());
    format!(
        "{}\t{}\t{}\t{}",
        entry.id,
        exit,
        entry.cwd.display(),
        entry.command
    )
}

#[allow(clippy::too_many_arguments)]
fn record_history(
    history: Option<&HistoryStore>,
    command: &str,
    cwd: &std::path::Path,
    platform: Platform,
    mode: HistoryMode,
    exit_code: Option<i32>,
    writer: &mut dyn Write,
) -> Result<()> {
    if let Some(history) = history
        && let Err(error) = history.record(
            command,
            RecordContext {
                cwd,
                platform,
                mode,
                exit_code,
            },
        )
    {
        writeln!(writer, "warning: failed to record history: {error}")?;
    }
    Ok(())
}

fn current_process_dir() -> Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

fn shutdown_session(session: Option<ShellSession>) -> Result<()> {
    if let Some(session) = session {
        session.shutdown()?;
    }
    Ok(())
}

fn run_native_shell() -> Result<()> {
    let (cols, rows) = size()?;
    let shell = shell_for(Platform::current());
    let mut process = shell.spawn(&SystemPty, TerminalSize::new(rows, cols))?;
    let mut reader = process.try_clone_reader()?;
    let mut writer = process.take_writer()?;
    let resizer = process.resizer();
    let running = Arc::new(AtomicBool::new(true));
    let _raw_mode = RawModeGuard::enable()?;

    let input_thread = thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        io::copy(&mut stdin, &mut writer)
    });

    let resize_running = Arc::clone(&running);
    let resize_thread = thread::spawn(move || {
        let mut previous = (cols, rows);
        while resize_running.load(Ordering::Relaxed) {
            if let Ok(current @ (new_cols, new_rows)) = size()
                && current != previous
            {
                let _ = resizer.resize(TerminalSize::new(new_rows, new_cols));
                previous = current;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });

    let mut stdout = io::stdout().lock();
    let output_result = relay_output(&mut reader, &mut stdout);

    running.store(false, Ordering::Relaxed);
    let _ = resize_thread.join();
    if output_result.is_err() {
        let _ = process.kill();
    }
    let wait_result = process.wait();
    drop(input_thread);

    output_result?;
    wait_result?;
    Ok(())
}

fn relay_output(reader: &mut dyn Read, writer: &mut dyn Write) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        writer.write_all(&buffer[..bytes_read])?;
        writer.flush()?;
    }
    Ok(())
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use crumb_agent::{AgentConfig, RiskClass};

    use super::{agent_config_location, safe_event_label, workspace_read_host};

    #[test]
    fn stdio_mcp_host_exposes_only_read_tools() {
        let workspace = std::env::current_dir().expect("current directory is available");
        let host = workspace_read_host(&workspace, &AgentConfig::default())
            .expect("workspace tools are registered");
        let tools = host.tools().collect::<Vec<_>>();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|tool| tool.risk == RiskClass::ReadOnly));
        assert_eq!(tools[0].name, "list_directory");
        assert_eq!(tools[1].name, "read_file");
    }

    #[test]
    fn cordis_composition_has_no_direct_mutating_tools() {
        let composition = include_str!("../../../config/harness/crumb.cordis.yml");
        assert!(composition.contains("@deepseek-ai/dsh-mcp-client"));
        for forbidden in [
            "@deepseek-ai/dsh-bash-local",
            "@deepseek-ai/dsh-fs-local",
            "@deepseek-ai/dsh-tool-fs",
        ] {
            assert!(!composition.contains(forbidden));
        }
    }

    #[test]
    fn harness_event_labels_cannot_inject_terminal_controls() {
        assert_eq!(
            safe_event_label("tool/run\u{1b}[31m_secret"),
            Some("tool run31m secret".to_owned())
        );
    }

    #[test]
    fn nested_workspaces_find_the_nearest_agent_config() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is valid")
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("crumb-config-{suffix}"));
        let nested = workspace.join("site");
        std::fs::create_dir_all(workspace.join(".crumb")).expect("config directory can be created");
        std::fs::create_dir_all(&nested).expect("nested workspace can be created");
        std::fs::write(workspace.join(".crumb/agent.json"), b"{}").expect("config can be created");

        let (path, root) = agent_config_location(&nested);

        assert_eq!(root, workspace);
        assert_eq!(path, root.join(".crumb/agent.json"));
        std::fs::remove_dir_all(root).expect("temporary workspace can be removed");
    }
}
