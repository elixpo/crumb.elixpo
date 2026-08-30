use std::borrow::Cow;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{
    Event as TerminalEvent, KeyCode as TerminalKeyCode, KeyEventKind,
    KeyModifiers as TerminalModifiers, poll as poll_terminal_event, read as read_terminal_event,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use crumb_agent::{
    AgentConfig, AgentMode, BackendDiscovery, CacheRetention, CancellationToken, CommandCatalog,
    CompatibilityFlag, ConfiguredApprovals, CredentialReference, HarnessConfig, InputRoute, JobId,
    JobSchedule, JobState, JobStore, LiveConfig, MistakePolicy, Modality, ModelRoute, NewJob,
    ProviderCompatibility, ProviderConfig, ProviderHeader, ProviderModel, ProviderProtocol,
    ProviderTransport, RouteDecision, SteeringAction, SteeringQueue, TokenOptimizer, ToolHost,
    TurnStatus, UnknownInputPolicy, export_session, list_sessions, search_sessions,
    session_summary, set_session_archived, set_session_label, trash_session,
};
use crumb_auth::{CredentialSource, CredentialStore, OsCredentialStore, credential_status, login};
use crumb_core::{AuthAction, BuiltInCommand, HistoryAction, InputEvent};
use crumb_harness_dsh::HarnessActivity;
use crumb_history::{HistoryEntry, HistoryMode, HistoryStore, RecordContext};
use crumb_mcp::{McpDispatcher, serve_stdio};
use crumb_native::session::{CommandOutcome, ShellSession};
use crumb_native::shell_for;
use crumb_optimize::RtkOptimizer;
use crumb_platform::Platform;
use crumb_pollinations::{PollinationsSearchConfig, register_web_search_tool};
use crumb_pty::{PtyInput, PtyResizer, SystemPty, TerminalSize};
use crumb_repl::{ReplOutcome, read_classified_line};
use crumb_tools::{
    CheckpointDecision, CheckpointStatus, CheckpointStore, WorkspaceToolLimits,
    WorkspaceWriteLimits, register_workspace_read_tools, register_workspace_write_tool,
};
use crumb_ui::{GitSegment, PromptContext, Renderer, UiSettings};
use reedline::{
    ColumnarMenu, EditCommand, Emacs, FileBackedHistory, History, HistoryItem, KeyCode,
    KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, ReedlineEvent, ReedlineMenu, Signal,
    default_emacs_keybindings,
};

mod agent_runtime;
mod completion;
mod device_auth;
mod provider_projection;
mod shell_completion;

use agent_runtime::AgentRuntime;
use completion::{CompletionWorkspace, CrumbCompleter};
use shell_completion::{CompletionShell, write_completion};

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
    if let [group, shell] = arguments.as_slice()
        && group == "completions"
    {
        let shell = shell.to_str().context("shell identifier must be UTF-8")?;
        write_completion(CompletionShell::parse(shell)?, &mut io::stdout().lock())?;
        return Ok(true);
    }
    if let [group, action, id] = arguments.as_slice()
        && group == "jobs"
        && action == "run"
    {
        let id = id.to_str().context("job identifier must be UTF-8")?;
        run_job_worker(&current_process_dir()?, id, &mut io::stdout().lock())?;
        return Ok(true);
    }
    if matches!(arguments.as_slice(), [group, action] if group == "jobs" && action == "tick") {
        launch_due_jobs(&current_process_dir()?, &mut io::stdout().lock())?;
        return Ok(true);
    }
    if matches!(arguments.as_slice(), [group, action] if group == "jobs" && action == "list") {
        serde_json::to_writer(
            &mut io::stdout().lock(),
            &JobStore::new(current_process_dir()?).list()?,
        )?;
        writeln!(io::stdout().lock())?;
        return Ok(true);
    }
    if let [group, action, id] = arguments.as_slice()
        && group == "review"
        && action == "export"
    {
        let id = id.to_str().context("checkpoint identifier must be UTF-8")?;
        export_reviews(&current_process_dir()?, id, &mut io::stdout().lock())?;
        return Ok(true);
    }
    let action = match arguments.as_slice() {
        [] => return Ok(false),
        [group, action] if group == "auth" && action == "login" => AuthAction::Login,
        [group, action] if group == "auth" && action == "status" => AuthAction::Status,
        [group, action] if group == "auth" && action == "logout" => AuthAction::Logout,
        _ => {
            return Err(anyhow!(
                "usage: crumb [auth <login|status|logout> | mcp serve | review export <id|all> | completions <shell> | jobs <list|run <id>|tick>]"
            ));
        }
    };
    handle_auth(action, &mut io::stdout().lock())?;
    Ok(true)
}

fn export_reviews(cwd: &Path, id: &str, writer: &mut dyn Write) -> Result<()> {
    let config = read_agent_config(cwd)?;
    let max_file_bytes = usize::try_from(config.limits.max_file_write_bytes)
        .context("max_file_write_bytes exceeds this platform's address space")?;
    let store = CheckpointStore::new(cwd, max_file_bytes)?;
    if id == "all" {
        serde_json::to_writer(&mut *writer, &store.list()?)?;
    } else {
        serde_json::to_writer(&mut *writer, &store.load(id)?)?;
    }
    writeln!(writer)?;
    Ok(())
}

fn run_job_worker(cwd: &Path, id: &str, writer: &mut dyn Write) -> Result<()> {
    let store = JobStore::new(cwd.to_path_buf());
    let definition = store.claim_due(id, std::process::id())?;
    let mut runtime = match AgentRuntime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            store.finish(id, Some(&error.to_string()))?;
            return Err(error);
        }
    };
    let session_id = match runtime.prepare_local_job(definition.config.mode, &definition.workspace)
    {
        Ok(session_id) => session_id,
        Err(error) => {
            store.finish(id, Some(&error.to_string()))?;
            return Err(error);
        }
    };
    if let Err(error) = store.attach_session(id, session_id) {
        store.finish(id, Some(&error.to_string()))?;
        return Err(error);
    }
    let cancellation = CancellationToken::default();
    let monitoring = monitor_job_cancellation(store.clone(), id.to_owned(), cancellation.clone());
    let result = runtime.run_local_job(
        definition.request(),
        &definition.config,
        &definition.workspace,
        &cancellation,
    );
    monitoring.stop();
    match result {
        Ok(result) => {
            store.finish(id, None)?;
            writeln!(
                writer,
                "{}",
                crumb_ui::visible_agent_text(&result.final_response)
            )?;
            Ok(())
        }
        Err(error) => {
            if cancellation.is_cancelled()
                && matches!(store.inspect(id)?.state, JobState::Running { .. })
            {
                store.request_cancel(id)?;
            }
            store.finish(id, Some(&error.to_string()))?;
            Err(error)
        }
    }
}

struct JobCancellationMonitor {
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl JobCancellationMonitor {
    fn stop(mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn monitor_job_cancellation(
    store: JobStore,
    id: String,
    cancellation: CancellationToken,
) -> JobCancellationMonitor {
    let stopped = Arc::new(AtomicBool::new(false));
    let monitor_stopped = Arc::clone(&stopped);
    let worker = thread::spawn(move || {
        while !monitor_stopped.load(Ordering::Acquire) {
            if let Ok(job) = store.inspect(&id)
                && matches!(job.state, JobState::CancellationRequested { .. })
            {
                cancellation.cancel();
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    JobCancellationMonitor {
        stopped,
        worker: Some(worker),
    }
}

fn launch_job_worker(cwd: &Path, id: &str) -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate crumb executable")?;
    let mut child = Command::new(executable)
        .args(["jobs", "run", id])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to launch local job worker")?;
    drop(thread::spawn(move || {
        let _ = child.wait();
    }));
    Ok(())
}

fn launch_due_jobs(cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let store = JobStore::new(cwd.to_path_buf());
    let due = store.due_now()?;
    let mut launched = 0_usize;
    for job in due {
        launch_job_worker(cwd, job.definition.id.as_str())?;
        launched += 1;
    }
    writeln!(writer, "launched {launched} due jobs")?;
    Ok(())
}

fn serve_mcp() -> Result<()> {
    let workspace = current_process_dir()?;
    let config = read_agent_config(&workspace)?;
    let search_api_key = pollinations_environment_key();
    let host = workspace_read_host(&workspace, &config, search_api_key)?;
    let dispatcher = McpDispatcher::new(
        host,
        Arc::new(
            ConfiguredApprovals::new(config.permissions.allow_network_tools.clone())
                .with_workspace_tools(config.permissions.allow_workspace_tools.clone()),
        ),
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

fn workspace_read_host(
    workspace: &Path,
    config: &AgentConfig,
    search_api_key: Option<String>,
) -> Result<ToolHost> {
    let max_output_bytes = usize::try_from(config.limits.max_output_bytes)
        .map_err(|_| anyhow!("max_output_bytes exceeds this platform's address space"))?;
    let max_directory_entries = usize::try_from(config.limits.max_directory_entries)
        .map_err(|_| anyhow!("max_directory_entries exceeds this platform's address space"))?;
    let max_file_write_bytes = usize::try_from(config.limits.max_file_write_bytes)
        .map_err(|_| anyhow!("max_file_write_bytes exceeds this platform's address space"))?;
    let mut host = ToolHost::default();
    register_workspace_read_tools(
        &mut host,
        workspace,
        WorkspaceToolLimits {
            max_output_bytes,
            max_directory_entries,
        },
    )?;
    register_workspace_write_tool(
        &mut host,
        workspace,
        WorkspaceWriteLimits {
            max_file_bytes: max_file_write_bytes,
        },
    )?;
    if let Some(route) = config
        .models
        .get(&Modality::WebSearch)
        .and_then(|routes| routes.first())
        && route.provider == "pollinations"
        && let Some(api_key) = search_api_key
        && !api_key.trim().is_empty()
    {
        register_web_search_tool(
            &mut host,
            PollinationsSearchConfig::new(api_key, max_output_bytes)?.with_model(&route.model),
        )?;
    }
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
            context.interactive,
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
        command, cordis, ..
    }) = &mut config.harness
    {
        if command.is_relative() && command.components().count() > 1 {
            *command = config_root.join(command.as_path());
        }
        if let Some(cordis) = cordis
            && cordis.is_relative()
        {
            *cordis = config_root.join(cordis.as_path());
        }
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
    interactive: bool,
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
    execute_agent_sequence(
        decision.payload.clone(),
        config,
        workspace,
        runtime,
        renderer,
        writer,
        interactive,
    )
}

fn render_agent_result(
    result: &Result<crumb_harness_dsh::RunResult>,
    renderer: Renderer,
    writer: &mut dyn Write,
) -> Result<()> {
    let visible = result
        .as_ref()
        .ok()
        .map(|result| crumb_ui::visible_agent_text(&result.final_response));
    match result {
        Ok(result)
            if result.finish_reason.as_deref().is_some_and(|reason| {
                matches!(reason, "error" | "failed" | "cancelled" | "canceled")
            }) => {}
        Ok(_) if visible.as_deref().is_none_or(str::is_empty) => {
            writeln!(
                writer,
                "{}",
                Renderer::agent_response("Turn completed without a text response.")
            )?;
        }
        Ok(_) => writeln!(
            writer,
            "{}",
            Renderer::agent_response(visible.as_deref().unwrap_or_default())
        )?,
        Err(error) => {
            let message = error.to_string();
            let cancelled = message.to_ascii_lowercase().contains("cancel");
            writeln!(writer, "{}", renderer.agent_error(&message, cancelled))?;
        }
    }
    Ok(())
}

fn execute_agent_sequence(
    initial_request: String,
    config: &AgentConfig,
    workspace: &Path,
    runtime: &mut Option<AgentRuntime>,
    renderer: Renderer,
    writer: &mut dyn Write,
    interactive: bool,
) -> Result<()> {
    let activity_capacity = usize::try_from(config.limits.max_activity_events)
        .context("max_activity_events exceeds this platform's address space")?;
    let steering_messages = usize::try_from(config.limits.max_steering_messages)
        .context("max_steering_messages exceeds this platform's address space")?;
    let steering_bytes = usize::try_from(config.limits.max_steering_bytes)
        .context("max_steering_bytes exceeds this platform's address space")?;
    let mut steering = SteeringQueue::new(steering_messages, steering_bytes)?;
    let mut request = initial_request;
    let mut active_runtime = runtime.take().context("agent runtime is unavailable")?;
    loop {
        let session_id = active_runtime.prepare_local_job(config.mode, workspace)?;
        let persisted_request = request.clone();
        let mut task = active_runtime.spawn_turn(
            request,
            config.clone(),
            workspace.to_path_buf(),
            activity_capacity,
        )?;
        let (returned_runtime, result) = loop {
            match observe_agent_turn(
                task,
                &mut steering,
                steering_bytes,
                renderer,
                writer,
                interactive,
            )? {
                TurnObservation::Finished(runtime, result) => break (*runtime, result),
                TurnObservation::Backgrounded(returned_task) => {
                    match promote_agent_turn(
                        returned_task,
                        &persisted_request,
                        config,
                        workspace,
                        session_id.clone(),
                    ) {
                        Ok(id) => {
                            writeln!(
                                writer,
                                "◆ Agent turn continues as background job {}",
                                id.as_str()
                            )?;
                            return Ok(());
                        }
                        Err((error, returned_task)) => {
                            writeln!(writer, "◇ Background promotion rejected · {error}")?;
                            task = returned_task;
                        }
                    }
                }
            }
        };
        active_runtime = returned_runtime;
        let completed = result.is_ok();
        render_agent_result(&result, renderer, writer)?;
        if !completed {
            steering.clear();
            break;
        }
        let Some(queued) = steering.pop() else {
            break;
        };
        writeln!(
            writer,
            "◇ Running queued follow-up · {} remaining",
            steering.len()
        )?;
        request = queued;
    }
    *runtime = Some(active_runtime);
    Ok(())
}

enum TurnObservation {
    Finished(Box<AgentRuntime>, Result<crumb_harness_dsh::RunResult>),
    Backgrounded(agent_runtime::AgentTurnTask),
}

fn promote_agent_turn(
    task: agent_runtime::AgentTurnTask,
    request: &str,
    config: &AgentConfig,
    workspace: &Path,
    session_id: crumb_agent::SessionId,
) -> std::result::Result<JobId, (anyhow::Error, agent_runtime::AgentTurnTask)> {
    let store = JobStore::new(workspace.to_path_buf());
    let promotion = (|| -> Result<crumb_agent::JobSummary> {
        let created = store.create(NewJob {
            request: request.to_owned(),
            config: config.clone(),
            schedule: JobSchedule::Immediate,
            scheduler_opt_in: false,
        })?;
        if let Err(error) = store.mark_running(created.id.as_str(), std::process::id()) {
            let _ = store.request_cancel(created.id.as_str());
            return Err(error);
        }
        if let Err(error) = store.attach_session(created.id.as_str(), session_id) {
            let _ = store.finish(created.id.as_str(), Some(&error.to_string()));
            return Err(error);
        }
        Ok(created)
    })();
    let created = match promotion {
        Ok(created) => created,
        Err(error) => return Err((error, task)),
    };
    let id = created.id.clone();
    let worker_id = id.as_str().to_owned();
    drop(thread::spawn(move || {
        while !task.is_finished() {
            if let Ok(job) = store.inspect(&worker_id)
                && matches!(job.state, JobState::CancellationRequested { .. })
            {
                task.cancel();
            }
            let _ = task.recv_timeout(Duration::from_millis(50));
        }
        while task.recv_timeout(Duration::ZERO).is_ok() {}
        let error = match task.finish() {
            Ok((_, result)) => result.err().map(|error| error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        let _ = store.finish(&worker_id, error.as_deref());
    }));
    Ok(id)
}

fn observe_agent_turn(
    task: agent_runtime::AgentTurnTask,
    steering: &mut SteeringQueue,
    steering_bytes: usize,
    renderer: Renderer,
    writer: &mut dyn Write,
    interactive: bool,
) -> Result<TurnObservation> {
    let mut activity = Some(renderer.activity("Working"));
    let mut input = SteeringInput::new(steering_bytes);
    let raw_mode = interactive.then(RawModeGuard::enable).transpose()?;
    while !task.is_finished() {
        match task.recv_timeout(Duration::from_millis(15)) {
            Ok(event) => {
                input.clear_line(writer)?;
                render_harness_activity(&event, &mut activity, renderer, writer)?;
                input.redraw(writer)?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if interactive
            && poll_terminal_event(Duration::from_millis(10))?
            && let TerminalEvent::Key(key) = read_terminal_event()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            if let Some(indicator) = activity.take() {
                indicator.finish();
            }
            if input.handle_key(key.code, key.modifiers, &task, steering, writer)?
                == SteeringInputAction::Background
            {
                input.clear_line(writer)?;
                drop(raw_mode);
                if let Some(indicator) = activity.take() {
                    indicator.finish();
                }
                task.detach_interrupt();
                return Ok(TurnObservation::Backgrounded(task));
            }
        }
    }
    while let Ok(event) = task.recv_timeout(Duration::ZERO) {
        input.clear_line(writer)?;
        render_harness_activity(&event, &mut activity, renderer, writer)?;
        input.redraw(writer)?;
    }
    input.clear_line(writer)?;
    drop(raw_mode);
    if let Some(indicator) = activity.take() {
        indicator.complete();
    }
    let (runtime, result) = task.finish()?;
    Ok(TurnObservation::Finished(Box::new(runtime), result))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SteeringInputAction {
    Continue,
    Background,
}

struct SteeringInput {
    buffer: String,
    max_bytes: usize,
    visible: bool,
}

impl SteeringInput {
    const fn new(max_bytes: usize) -> Self {
        Self {
            buffer: String::new(),
            max_bytes,
            visible: false,
        }
    }

    fn handle_key(
        &mut self,
        code: TerminalKeyCode,
        modifiers: TerminalModifiers,
        task: &agent_runtime::AgentTurnTask,
        steering: &mut SteeringQueue,
        writer: &mut dyn Write,
    ) -> Result<SteeringInputAction> {
        if code == TerminalKeyCode::Char('c') && modifiers.contains(TerminalModifiers::CONTROL) {
            task.cancel();
            steering.clear();
            self.buffer.clear();
            self.clear_line(writer)?;
            writeln!(writer, "◇ Cancelling active agent turn")?;
            return Ok(SteeringInputAction::Continue);
        }
        match code {
            TerminalKeyCode::Enter => return self.submit(steering, task, writer),
            TerminalKeyCode::Backspace => {
                self.buffer.pop();
                self.redraw(writer)?;
            }
            TerminalKeyCode::Esc => {
                self.buffer.clear();
                self.clear_line(writer)?;
            }
            TerminalKeyCode::Char('u') if modifiers.contains(TerminalModifiers::CONTROL) => {
                self.buffer.clear();
                self.clear_line(writer)?;
            }
            TerminalKeyCode::Char(character)
                if !modifiers.intersects(TerminalModifiers::CONTROL | TerminalModifiers::ALT)
                    && self.buffer.len().saturating_add(character.len_utf8()) <= self.max_bytes =>
            {
                self.buffer.push(character);
                self.visible = true;
                self.redraw(writer)?;
            }
            _ => {}
        }
        Ok(SteeringInputAction::Continue)
    }

    fn submit(
        &mut self,
        steering: &mut SteeringQueue,
        task: &agent_runtime::AgentTurnTask,
        writer: &mut dyn Write,
    ) -> Result<SteeringInputAction> {
        let input = self.buffer.trim().to_owned();
        if input.is_empty() {
            self.buffer.clear();
            self.clear_line(writer)?;
            return Ok(SteeringInputAction::Continue);
        }
        if input == "/cancel" {
            task.cancel();
            steering.clear();
            self.buffer.clear();
            self.clear_line(writer)?;
            writeln!(writer, "◇ Cancelling active agent turn")?;
            return Ok(SteeringInputAction::Continue);
        }
        if input == "/background" {
            self.buffer.clear();
            self.clear_line(writer)?;
            return Ok(SteeringInputAction::Background);
        }
        let (action, message) = input
            .strip_prefix("/replace ")
            .map_or((SteeringAction::Queue, input.as_str()), |message| {
                (SteeringAction::Replace, message)
            });
        self.clear_line(writer)?;
        match steering.submit(action, message) {
            Ok(()) => writeln!(
                writer,
                "◇ {} follow-up · {} queued",
                if action == SteeringAction::Replace {
                    "Replaced"
                } else {
                    "Queued"
                },
                steering.len()
            )?,
            Err(error) => writeln!(writer, "◇ Follow-up rejected · {error}")?,
        }
        self.buffer.clear();
        self.visible = false;
        writer.flush()?;
        Ok(SteeringInputAction::Continue)
    }

    fn clear_line(&mut self, writer: &mut dyn Write) -> Result<()> {
        if self.visible {
            write!(writer, "\r\x1b[2K")?;
            writer.flush()?;
            self.visible = false;
        }
        Ok(())
    }

    fn redraw(&mut self, writer: &mut dyn Write) -> Result<()> {
        if self.buffer.is_empty() {
            return self.clear_line(writer);
        }
        write!(writer, "\r\x1b[2K↪ steer> {}", self.buffer)?;
        writer.flush()?;
        self.visible = true;
        Ok(())
    }
}

fn harness_activity_label(activity: &HarnessActivity) -> String {
    match activity {
        HarnessActivity::RequestAccepted => "request accepted".to_owned(),
        HarnessActivity::Status { state } => format!("session {state}"),
        HarnessActivity::ToolStarted { name } => format!("tool started · {name}"),
        HarnessActivity::ApprovalRequired { name } => format!("approval required · {name}"),
        HarnessActivity::ToolOutput { name, bytes } => bytes.map_or_else(
            || format!("tool output · {name}"),
            |bytes| format!("tool output · {name} · {bytes} bytes"),
        ),
        HarnessActivity::ToolFinished { name, success } => format!(
            "tool {} · {name}",
            if *success { "completed" } else { "failed" }
        ),
        HarnessActivity::Completed { reason } => format!("completed · {reason}"),
        HarnessActivity::Failed { reason } => format!("failed · {reason}"),
        HarnessActivity::Cancelled => "cancelled".to_owned(),
        HarnessActivity::Progress { label } => label.clone(),
    }
}

fn render_harness_activity(
    activity: &HarnessActivity,
    indicator: &mut Option<crumb_ui::ActivityIndicator>,
    renderer: Renderer,
    writer: &mut dyn Write,
) -> Result<()> {
    if matches!(
        activity,
        HarnessActivity::RequestAccepted
            | HarnessActivity::Status { .. }
            | HarnessActivity::Completed { .. }
            | HarnessActivity::Progress { .. }
    ) {
        return Ok(());
    }
    if let Some(indicator) = indicator.take() {
        indicator.finish();
    }
    write!(writer, "\r\x1b[2K")?;
    writeln!(
        writer,
        "{}",
        renderer.agent_activity(&harness_activity_label(activity))
    )?;
    writer.flush()?;
    if matches!(
        activity,
        HarnessActivity::ToolStarted { .. }
            | HarnessActivity::ToolOutput { .. }
            | HarnessActivity::ToolFinished { .. }
            | HarnessActivity::ApprovalRequired { .. }
    ) {
        *indicator = Some(renderer.activity("Working"));
    }
    Ok(())
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
    writeln!(
        writer,
        "  Enter submit · Alt/Shift+Enter newline · Ctrl+O editor · Ctrl+R history"
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
            &[
                "/mode",
                "/model",
                "/effort",
                "/session",
                "/review",
                "/jobs",
                "/background",
                "/cancel",
                "/cost",
            ][..],
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
        command if command.starts_with("/mode use ") => {
            set_agent_mode(command, cwd, writer)?;
        }
        command if command.starts_with("/model use ") => {
            set_text_model(command, cwd, writer)?;
        }
        command if command.starts_with("/effort use ") => {
            set_reasoning_effort(command, cwd, writer)?;
        }
        command if command == "/config provider" || command.starts_with("/config provider ") => {
            configure_provider(command, cwd, writer)?;
        }
        "/doctor" => show_doctor(cwd, writer)?,
        "/plugins" => show_plugins(cwd, writer)?,
        command if command == "/review" || command.starts_with("/review ") => {
            show_reviews(command, cwd, runtime, writer)?;
        }
        command if command == "/jobs" || command.starts_with("/jobs ") => {
            show_jobs(command, cwd, runtime, writer)?;
        }
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

fn set_agent_mode(command: &str, cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let value = command
        .strip_prefix("/mode use ")
        .context("usage: /mode use <auto|negotiate|plan>")?
        .trim();
    let mode = match value {
        "auto" => AgentMode::Auto,
        "negotiate" => AgentMode::Negotiate,
        "plan" => AgentMode::Plan,
        _ => return Err(anyhow!("usage: /mode use <auto|negotiate|plan>")),
    };
    update_agent_config(cwd, |config| {
        config.mode = mode;
        Ok(())
    })?;
    writeln!(writer, "◆ Agent mode · {}", agent_mode_name(mode))?;
    Ok(())
}

fn set_text_model(command: &str, cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let selection = command
        .strip_prefix("/model use ")
        .context("usage: /model use <provider>/<model>")?
        .trim();
    let (provider_id, model_id) = selection
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .context("usage: /model use <provider>/<model>")?;
    update_agent_config(cwd, |config| {
        let provider = config
            .providers
            .get(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?;
        provider
            .models
            .iter()
            .find(|model| model.id == model_id)
            .with_context(|| {
                format!("model `{model_id}` is not configured for provider `{provider_id}`")
            })?;
        let routes = config.models.entry(Modality::Text).or_default();
        routes.retain(|route| route.provider != provider_id || route.model != model_id);
        routes.insert(
            0,
            ModelRoute {
                provider: provider_id.to_owned(),
                model: model_id.to_owned(),
                reasoning_effort: None,
            },
        );
        Ok(())
    })?;
    writeln!(writer, "◆ Text model · {provider_id}/{model_id}")?;
    Ok(())
}

fn set_reasoning_effort(command: &str, cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let value = command
        .strip_prefix("/effort use ")
        .context("usage: /effort use <level|default>")?
        .trim();
    if value.is_empty() {
        return Err(anyhow!("usage: /effort use <level|default>"));
    }
    let selected = (value != "default").then(|| value.to_owned());
    update_agent_config(cwd, |config| {
        let route = config
            .models
            .get_mut(&Modality::Text)
            .and_then(|routes| routes.first_mut())
            .context("select a text model before setting reasoning effort")?;
        route.reasoning_effort.clone_from(&selected);
        Ok(())
    })?;
    writeln!(writer, "◆ Reasoning effort · {value}")?;
    Ok(())
}

fn configure_provider(command: &str, cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let arguments = command.split_whitespace().skip(2).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["list"] => show_providers(cwd, writer),
        ["show", id] => show_provider(cwd, id, writer),
        ["remove", id] => remove_provider(cwd, id, writer),
        ["preset", preset] => add_provider_preset(cwd, preset, preset, writer),
        ["preset", preset, id] => add_provider_preset(cwd, preset, id, writer),
        ["credential", "set", provider, reference] => {
            set_provider_credential(cwd, provider, Some(reference), writer)
        }
        ["credential", "clear", provider] => set_provider_credential(cwd, provider, None, writer),
        ["header", "set", provider, name, reference] => {
            set_provider_header(cwd, provider, name, reference, writer)
        }
        ["header", "remove", provider, name] => remove_provider_header(cwd, provider, name, writer),
        ["set", provider, field, value] => set_provider_field(cwd, provider, field, value, writer),
        ["retry", provider, retries, base_delay, max_delay] => {
            set_provider_retry(cwd, provider, retries, base_delay, max_delay, writer)
        }
        ["pricing", "set", provider, metric, value] => {
            set_provider_pricing(cwd, provider, metric, Some(value), writer)
        }
        ["pricing", "remove", provider, metric] => {
            set_provider_pricing(cwd, provider, metric, None, writer)
        }
        ["compatibility", "set", provider, flag, value] => {
            set_provider_compatibility(cwd, provider, flag, value, writer)
        }
        ["compatibility-field", "set", provider, field, value] => {
            set_provider_compatibility_field(cwd, provider, field, Some(value), writer)
        }
        ["compatibility-field", "clear", provider, field] => {
            set_provider_compatibility_field(cwd, provider, field, None, writer)
        }
        ["modality", action, provider, modality] => {
            set_provider_default_modality(cwd, provider, action, modality, writer)
        }
        ["thinking-budget", "set", provider, effort, tokens] => {
            set_provider_thinking_budget(cwd, provider, effort, Some(tokens), writer)
        }
        ["thinking-budget", "remove", provider, effort] => {
            set_provider_thinking_budget(cwd, provider, effort, None, writer)
        }
        ["model", model_arguments @ ..] => configure_provider_model(model_arguments, cwd, writer),
        ["add", id, protocol, base_url] => add_provider(cwd, id, protocol, base_url, None, writer),
        ["add", id, protocol, base_url, credential] => {
            add_provider(cwd, id, protocol, base_url, Some(credential), writer)
        }
        _ => Err(anyhow!(
            "usage: /config provider <list|show|add|remove|preset|set|retry|pricing|compatibility|credential|header|model ...>"
        )),
    }
}

fn configure_provider_model(arguments: &[&str], cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    match arguments {
        ["add", provider, model] => {
            add_provider_model(cwd, provider, model, None, None, false, writer)
        }
        ["add", provider, model, context, output, tools] => add_provider_model(
            cwd,
            provider,
            model,
            Some(parse_positive_u64(context, "context window")?),
            Some(parse_positive_u64(output, "maximum output tokens")?),
            parse_tool_capability(tools)?,
            writer,
        ),
        ["remove", provider, model] => remove_provider_model(cwd, provider, model, writer),
        ["set", provider, model, field, value] => {
            set_provider_model_field(cwd, provider, model, field, value, writer)
        }
        ["modality", action, provider, model, modality] => {
            set_provider_model_modality(cwd, provider, model, action, modality, writer)
        }
        ["effort", "set", provider, model, effort, wire] => {
            set_provider_model_effort(cwd, provider, model, effort, Some(wire), writer)
        }
        ["effort", "remove", provider, model, effort] => {
            set_provider_model_effort(cwd, provider, model, effort, None, writer)
        }
        ["compatibility", "set", provider, model, flag, value] => {
            set_provider_model_compatibility(cwd, provider, model, flag, value, writer)
        }
        ["compatibility-field", "set", provider, model, field, value] => {
            set_provider_model_compatibility_field(cwd, provider, model, field, Some(value), writer)
        }
        ["compatibility-field", "clear", provider, model, field] => {
            set_provider_model_compatibility_field(cwd, provider, model, field, None, writer)
        }
        _ => Err(anyhow!(
            "usage: /config provider model <add|remove|set|modality|effort|compatibility|compatibility-field ...>"
        )),
    }
}

fn show_providers(cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let config = raw_agent_config(cwd).load_or_default()?;
    writeln!(writer, "◆ Harness providers")?;
    if config.providers.is_empty() {
        writeln!(writer, "  No configurable providers")?;
        return Ok(());
    }
    for (id, provider) in config.providers {
        writeln!(
            writer,
            "  {id:<18} {} · {} models",
            provider.display_name,
            provider.models.len()
        )?;
    }
    Ok(())
}

fn show_provider(cwd: &Path, id: &str, writer: &mut dyn Write) -> Result<()> {
    let config = raw_agent_config(cwd).load_or_default()?;
    let provider = config
        .providers
        .get(id)
        .with_context(|| format!("provider `{id}` is not configured"))?;
    serde_json::to_writer_pretty(&mut *writer, provider)?;
    writeln!(writer)?;
    Ok(())
}

fn remove_provider(cwd: &Path, id: &str, writer: &mut dyn Write) -> Result<()> {
    update_agent_config(cwd, |config| {
        if config
            .models
            .values()
            .flatten()
            .any(|route| route.provider == id)
        {
            return Err(anyhow!(
                "provider `{id}` is selected by a model route; select another model first"
            ));
        }
        config
            .providers
            .remove(id)
            .with_context(|| format!("provider `{id}` is not configured"))?;
        Ok(())
    })?;
    writeln!(writer, "◆ Removed provider · {id}")?;
    Ok(())
}

fn add_provider(
    cwd: &Path,
    id: &str,
    protocol: &str,
    base_url: &str,
    credential: Option<&str>,
    writer: &mut dyn Write,
) -> Result<()> {
    let protocol = parse_provider_protocol(protocol)?;
    let credential = credential.map(parse_credential_reference).transpose()?;
    update_agent_config(cwd, |config| {
        if config.providers.contains_key(id) {
            return Err(anyhow!("provider `{id}` is already configured"));
        }
        let mut provider = ProviderConfig::new(id.replace(['_', '-'], " "), protocol, base_url);
        provider.credential = credential;
        config.providers.insert(id.to_owned(), provider);
        Ok(())
    })?;
    writeln!(writer, "◆ Added provider · {id}")?;
    Ok(())
}

fn add_provider_preset(cwd: &Path, preset: &str, id: &str, writer: &mut dyn Write) -> Result<()> {
    let provider = match preset {
        "openrouter" => openrouter_preset(),
        "pollinations" => pollinations_preset(),
        _ => return Err(anyhow!("preset must be openrouter or pollinations")),
    };
    update_agent_config(cwd, |config| {
        if config.providers.contains_key(id) {
            return Err(anyhow!("provider `{id}` is already configured"));
        }
        config.providers.insert(id.to_owned(), provider);
        Ok(())
    })?;
    writeln!(writer, "◆ Added {preset} preset · {id}")?;
    writeln!(writer, "  Add and explicitly select a model before use.")?;
    Ok(())
}

fn openrouter_preset() -> ProviderConfig {
    let mut provider = ProviderConfig::new(
        "OpenRouter",
        ProviderProtocol::OpenAiCompletions,
        "https://openrouter.ai/api/v1",
    );
    provider.credential = Some(CredentialReference::Environment {
        name: "OPENROUTER_API_KEY".to_owned(),
    });
    provider.headers.insert(
        "HTTP-Referer".to_owned(),
        ProviderHeader::Public {
            value: "https://crumb.elixpo.com".to_owned(),
        },
    );
    provider.headers.insert(
        "X-OpenRouter-Title".to_owned(),
        ProviderHeader::Public {
            value: "Crumb".to_owned(),
        },
    );
    provider
}

fn pollinations_preset() -> ProviderConfig {
    let mut provider = ProviderConfig::new(
        "Pollinations",
        ProviderProtocol::OpenAiCompletions,
        "https://gen.pollinations.ai/v1",
    );
    provider.credential = Some(CredentialReference::Environment {
        name: "POLLINATIONS_API_KEY".to_owned(),
    });
    provider
}

fn set_provider_credential(
    cwd: &Path,
    provider_id: &str,
    reference: Option<&str>,
    writer: &mut dyn Write,
) -> Result<()> {
    let reference = reference.map(parse_credential_reference).transpose()?;
    let configured = reference.is_some();
    update_agent_config(cwd, |config| {
        config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .credential = reference;
        Ok(())
    })?;
    writeln!(
        writer,
        "◆ Provider credential reference {} · {provider_id}",
        if configured { "updated" } else { "cleared" }
    )?;
    Ok(())
}

fn set_provider_header(
    cwd: &Path,
    provider_id: &str,
    name: &str,
    reference: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    let header = parse_provider_header(reference)?;
    update_agent_config(cwd, |config| {
        config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .headers
            .insert(name.to_owned(), header);
        Ok(())
    })?;
    writeln!(writer, "◆ Provider header updated · {provider_id}/{name}")?;
    Ok(())
}

fn remove_provider_header(
    cwd: &Path,
    provider_id: &str,
    name: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    update_agent_config(cwd, |config| {
        config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .headers
            .remove(name)
            .with_context(|| format!("header `{name}` is not configured"))?;
        Ok(())
    })?;
    writeln!(writer, "◆ Provider header removed · {provider_id}/{name}")?;
    Ok(())
}

fn set_provider_field(
    cwd: &Path,
    provider_id: &str,
    field: &str,
    value: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    update_agent_config(cwd, |config| {
        let provider = config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?;
        match field {
            "display-name" => provider.display_name = value.replace('_', " "),
            "protocol" => provider.protocol = parse_provider_protocol(value)?,
            "base-url" => value.clone_into(&mut provider.base_url),
            "transport" => provider.transport = parse_provider_transport(value)?,
            "cache-retention" => provider.cache_retention = parse_cache_retention(value)?,
            "optimizer" => provider.optimizer = parse_optional_string(value),
            "reasoning" => provider.reasoning = parse_optional_string(value),
            "timeout-ms" => provider.timeout_ms = parse_optional_u64(value, field)?,
            "websocket-connect-timeout-ms" => {
                provider.websocket_connect_timeout_ms = parse_optional_u64(value, field)?;
            }
            "stream-idle-timeout-ms" => {
                provider.stream_idle_timeout_ms = parse_optional_u64(value, field)?;
            }
            "max-request-image-bytes" => {
                provider.max_request_image_bytes = parse_optional_u64(value, field)?;
            }
            "default-context-window" => {
                provider.default_context_window = parse_optional_u64(value, field)?;
            }
            "default-max-output-tokens" => {
                provider.default_max_output_tokens = parse_optional_u64(value, field)?;
            }
            _ => {
                return Err(anyhow!(
                    "provider field must be display-name, protocol, base-url, transport, cache-retention, optimizer, reasoning, timeout-ms, websocket-connect-timeout-ms, stream-idle-timeout-ms, max-request-image-bytes, default-context-window, or default-max-output-tokens"
                ));
            }
        }
        Ok(())
    })?;
    writeln!(writer, "◆ Provider field updated · {provider_id}/{field}")?;
    Ok(())
}

fn set_provider_retry(
    cwd: &Path,
    provider_id: &str,
    retries: &str,
    base_delay: &str,
    max_delay: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    let max_retries = retries
        .parse::<u32>()
        .context("maximum retries must be a non-negative integer")?;
    let base_delay_ms = base_delay
        .parse::<u64>()
        .context("base retry delay must be a non-negative integer")?;
    let max_delay_ms = max_delay
        .parse::<u64>()
        .context("maximum retry delay must be a non-negative integer")?;
    update_agent_config(cwd, |config| {
        let retry = &mut config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .retry;
        retry.max_retries = max_retries;
        retry.base_delay_ms = base_delay_ms;
        retry.max_delay_ms = max_delay_ms;
        Ok(())
    })?;
    writeln!(writer, "◆ Provider retry policy updated · {provider_id}")?;
    Ok(())
}

fn set_provider_pricing(
    cwd: &Path,
    provider_id: &str,
    metric: &str,
    value: Option<&str>,
    writer: &mut dyn Write,
) -> Result<()> {
    update_agent_config(cwd, |config| {
        let pricing = &mut config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .pricing;
        if let Some(value) = value {
            pricing.insert(metric.to_owned(), value.to_owned());
        } else {
            pricing
                .remove(metric)
                .with_context(|| format!("pricing metric `{metric}` is not configured"))?;
        }
        Ok(())
    })?;
    writeln!(
        writer,
        "◆ Provider pricing updated · {provider_id}/{metric}"
    )?;
    Ok(())
}

fn set_provider_compatibility(
    cwd: &Path,
    provider_id: &str,
    flag: &str,
    value: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    let flag = parse_compatibility_flag(flag)?;
    let enabled = parse_bool(value)?;
    update_agent_config(cwd, |config| {
        config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .compatibility
            .flags
            .insert(flag, enabled);
        Ok(())
    })?;
    writeln!(writer, "◆ Provider compatibility updated · {provider_id}")?;
    Ok(())
}

fn set_provider_compatibility_field(
    cwd: &Path,
    provider_id: &str,
    field: &str,
    value: Option<&str>,
    writer: &mut dyn Write,
) -> Result<()> {
    update_agent_config(cwd, |config| {
        let compatibility = &mut config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .compatibility;
        set_compatibility_field(compatibility, field, value)
    })?;
    writeln!(
        writer,
        "◆ Provider compatibility field updated · {provider_id}/{field}"
    )?;
    Ok(())
}

fn set_provider_default_modality(
    cwd: &Path,
    provider_id: &str,
    action: &str,
    modality: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    let modality = parse_modality(modality)?;
    update_agent_config(cwd, |config| {
        let input = &mut config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .default_input;
        match action {
            "add" => {
                input.insert(modality);
            }
            "remove" => {
                input.remove(&modality);
            }
            _ => return Err(anyhow!("modality action must be add or remove")),
        }
        Ok(())
    })?;
    writeln!(writer, "◆ Provider modalities updated · {provider_id}")?;
    Ok(())
}

fn set_provider_thinking_budget(
    cwd: &Path,
    provider_id: &str,
    effort: &str,
    tokens: Option<&str>,
    writer: &mut dyn Write,
) -> Result<()> {
    let tokens = tokens
        .map(|value| parse_positive_u64(value, "thinking budget"))
        .transpose()?;
    update_agent_config(cwd, |config| {
        let budgets = &mut config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?
            .thinking_budgets;
        if let Some(tokens) = tokens {
            budgets.insert(effort.to_owned(), tokens);
        } else {
            budgets
                .remove(effort)
                .with_context(|| format!("thinking budget `{effort}` is not configured"))?;
        }
        Ok(())
    })?;
    writeln!(
        writer,
        "◆ Provider thinking budget updated · {provider_id}/{effort}"
    )?;
    Ok(())
}

fn add_provider_model(
    cwd: &Path,
    provider_id: &str,
    model_id: &str,
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
    tool_calling: bool,
    writer: &mut dyn Write,
) -> Result<()> {
    update_agent_config(cwd, |config| {
        let provider = config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?;
        if provider.models.iter().any(|model| model.id == model_id) {
            return Err(anyhow!(
                "model `{model_id}` is already configured for provider `{provider_id}`"
            ));
        }
        let mut model = ProviderModel::new(model_id);
        model.input.insert(Modality::Text);
        model.context_window = context_window;
        model.max_output_tokens = max_output_tokens;
        model.tool_calling = tool_calling;
        provider.models.push(model);
        Ok(())
    })?;
    writeln!(writer, "◆ Added model · {provider_id}/{model_id}")?;
    Ok(())
}

fn remove_provider_model(
    cwd: &Path,
    provider_id: &str,
    model_id: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    update_agent_config(cwd, |config| {
        if config
            .models
            .values()
            .flatten()
            .any(|route| route.provider == provider_id && route.model == model_id)
        {
            return Err(anyhow!(
                "model `{provider_id}/{model_id}` is selected; choose another model first"
            ));
        }
        let provider = config
            .providers
            .get_mut(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?;
        let count = provider.models.len();
        provider.models.retain(|model| model.id != model_id);
        if count == provider.models.len() {
            return Err(anyhow!(
                "model `{model_id}` is not configured for provider `{provider_id}`"
            ));
        }
        Ok(())
    })?;
    writeln!(writer, "◆ Removed model · {provider_id}/{model_id}")?;
    Ok(())
}

fn set_provider_model_field(
    cwd: &Path,
    provider_id: &str,
    model_id: &str,
    field: &str,
    value: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    update_agent_config(cwd, |config| {
        let model = provider_model_mut(config, provider_id, model_id)?;
        match field {
            "display-name" => {
                model.display_name =
                    parse_optional_string(value).map(|name| name.replace('_', " "));
            }
            "context-window" => model.context_window = parse_optional_u64(value, field)?,
            "max-output-tokens" => {
                model.max_output_tokens = parse_optional_u64(value, field)?;
            }
            "tool-calling" => model.tool_calling = parse_bool(value)?,
            _ => {
                return Err(anyhow!(
                    "model field must be display-name, context-window, max-output-tokens, or tool-calling"
                ));
            }
        }
        Ok(())
    })?;
    writeln!(
        writer,
        "◆ Model field updated · {provider_id}/{model_id}/{field}"
    )?;
    Ok(())
}

fn set_provider_model_modality(
    cwd: &Path,
    provider_id: &str,
    model_id: &str,
    action: &str,
    modality: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    let modality = parse_modality(modality)?;
    update_agent_config(cwd, |config| {
        let input = &mut provider_model_mut(config, provider_id, model_id)?.input;
        match action {
            "add" => {
                input.insert(modality);
            }
            "remove" => {
                input.remove(&modality);
            }
            _ => return Err(anyhow!("modality action must be add or remove")),
        }
        Ok(())
    })?;
    writeln!(
        writer,
        "◆ Model modalities updated · {provider_id}/{model_id}"
    )?;
    Ok(())
}

fn set_provider_model_effort(
    cwd: &Path,
    provider_id: &str,
    model_id: &str,
    effort: &str,
    wire: Option<&str>,
    writer: &mut dyn Write,
) -> Result<()> {
    let wire = wire.map(|wire| (wire != "none").then(|| wire.to_owned()));
    update_agent_config(cwd, |config| {
        let efforts = &mut provider_model_mut(config, provider_id, model_id)?.reasoning_efforts;
        if let Some(wire) = wire {
            efforts.insert(effort.to_owned(), wire);
        } else {
            efforts
                .remove(effort)
                .with_context(|| format!("reasoning effort `{effort}` is not configured"))?;
        }
        Ok(())
    })?;
    writeln!(
        writer,
        "◆ Model effort map updated · {provider_id}/{model_id}"
    )?;
    Ok(())
}

fn set_provider_model_compatibility(
    cwd: &Path,
    provider_id: &str,
    model_id: &str,
    flag: &str,
    value: &str,
    writer: &mut dyn Write,
) -> Result<()> {
    let flag = parse_compatibility_flag(flag)?;
    let enabled = parse_bool(value)?;
    update_agent_config(cwd, |config| {
        provider_model_mut(config, provider_id, model_id)?
            .compatibility
            .flags
            .insert(flag, enabled);
        Ok(())
    })?;
    writeln!(
        writer,
        "◆ Model compatibility updated · {provider_id}/{model_id}"
    )?;
    Ok(())
}

fn set_provider_model_compatibility_field(
    cwd: &Path,
    provider_id: &str,
    model_id: &str,
    field: &str,
    value: Option<&str>,
    writer: &mut dyn Write,
) -> Result<()> {
    update_agent_config(cwd, |config| {
        let compatibility = &mut provider_model_mut(config, provider_id, model_id)?.compatibility;
        set_compatibility_field(compatibility, field, value)
    })?;
    writeln!(
        writer,
        "◆ Model compatibility field updated · {provider_id}/{model_id}/{field}"
    )?;
    Ok(())
}

fn set_compatibility_field(
    compatibility: &mut ProviderCompatibility,
    field: &str,
    value: Option<&str>,
) -> Result<()> {
    let value = value.map(str::to_owned);
    match field {
        "max-tokens-field" => compatibility.max_tokens_field = value,
        "thinking-format" => compatibility.thinking_format = value,
        "cache-control-format" => compatibility.cache_control_format = value,
        _ => {
            return Err(anyhow!(
                "compatibility field must be max-tokens-field, thinking-format, or cache-control-format"
            ));
        }
    }
    Ok(())
}

fn provider_model_mut<'a>(
    config: &'a mut AgentConfig,
    provider_id: &str,
    model_id: &str,
) -> Result<&'a mut ProviderModel> {
    config
        .providers
        .get_mut(provider_id)
        .with_context(|| format!("provider `{provider_id}` is not configured"))?
        .models
        .iter_mut()
        .find(|model| model.id == model_id)
        .with_context(|| {
            format!("model `{model_id}` is not configured for provider `{provider_id}`")
        })
}

fn parse_positive_u64(value: &str, label: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .with_context(|| format!("{label} must be a positive integer"))?;
    if parsed == 0 {
        return Err(anyhow!("{label} must be a positive integer"));
    }
    Ok(parsed)
}

fn parse_tool_capability(value: &str) -> Result<bool> {
    match value {
        "tools" => Ok(true),
        "no-tools" => Ok(false),
        _ => Err(anyhow!("tool capability must be tools or no-tools")),
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "true" | "on" | "yes" => Ok(true),
        "false" | "off" | "no" => Ok(false),
        _ => Err(anyhow!("value must be true or false")),
    }
}

fn parse_optional_string(value: &str) -> Option<String> {
    (value != "default" && value != "none").then(|| value.to_owned())
}

fn parse_optional_u64(value: &str, label: &str) -> Result<Option<u64>> {
    if matches!(value, "default" | "none") {
        return Ok(None);
    }
    parse_positive_u64(value, label).map(Some)
}

fn parse_provider_transport(value: &str) -> Result<Option<ProviderTransport>> {
    match value {
        "default" | "none" => Ok(None),
        "sse" => Ok(Some(ProviderTransport::Sse)),
        "websocket" => Ok(Some(ProviderTransport::Websocket)),
        "websocket-cached" | "websocket_cached" => Ok(Some(ProviderTransport::WebsocketCached)),
        "auto" => Ok(Some(ProviderTransport::Auto)),
        _ => Err(anyhow!(
            "transport must be sse, websocket, websocket-cached, auto, or default"
        )),
    }
}

fn parse_cache_retention(value: &str) -> Result<Option<CacheRetention>> {
    match value {
        "default" => Ok(None),
        "none" => Ok(Some(CacheRetention::None)),
        "short" => Ok(Some(CacheRetention::Short)),
        "long" => Ok(Some(CacheRetention::Long)),
        _ => Err(anyhow!(
            "cache retention must be none, short, long, or default"
        )),
    }
}

fn parse_compatibility_flag(value: &str) -> Result<CompatibilityFlag> {
    match value {
        "store" => Ok(CompatibilityFlag::Store),
        "developer-role" | "developer_role" => Ok(CompatibilityFlag::DeveloperRole),
        "reasoning-effort" | "reasoning_effort" => Ok(CompatibilityFlag::ReasoningEffort),
        "usage-in-streaming" | "usage_in_streaming" => Ok(CompatibilityFlag::UsageInStreaming),
        "strict-tools" | "strict_tools" => Ok(CompatibilityFlag::StrictTools),
        "temperature" => Ok(CompatibilityFlag::Temperature),
        _ => Err(anyhow!(
            "compatibility flag must be store, developer-role, reasoning-effort, usage-in-streaming, strict-tools, or temperature"
        )),
    }
}

fn parse_modality(value: &str) -> Result<Modality> {
    match value {
        "text" => Ok(Modality::Text),
        "web-search" | "web_search" => Ok(Modality::WebSearch),
        "image" => Ok(Modality::Image),
        "video" => Ok(Modality::Video),
        "audio" => Ok(Modality::Audio),
        "3d" | "three-d" | "three_d" => Ok(Modality::ThreeD),
        "transcription" => Ok(Modality::Transcription),
        "embeddings" => Ok(Modality::Embeddings),
        _ => Err(anyhow!(
            "modality must be text, web-search, image, video, audio, 3d, transcription, or embeddings"
        )),
    }
}

fn parse_provider_protocol(value: &str) -> Result<ProviderProtocol> {
    match value {
        "openai" | "open_ai_completions" => Ok(ProviderProtocol::OpenAiCompletions),
        "responses" | "open_ai_responses" => Ok(ProviderProtocol::OpenAiResponses),
        "anthropic" | "anthropic_messages" => Ok(ProviderProtocol::AnthropicMessages),
        _ => Err(anyhow!("protocol must be openai, responses, or anthropic")),
    }
}

fn parse_credential_reference(value: &str) -> Result<CredentialReference> {
    if let Some(name) = value.strip_prefix("env:") {
        return Ok(CredentialReference::Environment {
            name: name.to_owned(),
        });
    }
    if let Some(reference) = value.strip_prefix("keyring:") {
        let (service, account) = reference
            .split_once('/')
            .filter(|(service, account)| !service.is_empty() && !account.is_empty())
            .context("keyring references use keyring:SERVICE/ACCOUNT")?;
        return Ok(CredentialReference::Keyring {
            service: service.to_owned(),
            account: account.to_owned(),
        });
    }
    Err(anyhow!(
        "credentials must be references: env:NAME or keyring:SERVICE/ACCOUNT"
    ))
}

fn parse_provider_header(value: &str) -> Result<ProviderHeader> {
    if let Some(name) = value.strip_prefix("env:") {
        return Ok(ProviderHeader::Environment {
            name: name.to_owned(),
        });
    }
    if let Some(value) = value.strip_prefix("public:") {
        return Ok(ProviderHeader::Public {
            value: value.to_owned(),
        });
    }
    Err(anyhow!(
        "headers must use env:NAME or public:VALUE; raw secret values are rejected"
    ))
}

fn raw_agent_config(cwd: &Path) -> LiveConfig {
    let (path, _) = agent_config_location(cwd);
    LiveConfig::new(path)
}

fn update_agent_config<F>(cwd: &Path, mutate: F) -> Result<AgentConfig>
where
    F: FnOnce(&mut AgentConfig) -> Result<()>,
{
    raw_agent_config(cwd).update(mutate)
}

fn show_jobs(
    command: &str,
    cwd: &Path,
    runtime: &mut Option<AgentRuntime>,
    writer: &mut dyn Write,
) -> Result<()> {
    let store = JobStore::new(cwd.to_path_buf());
    if let Some(request) = command.strip_prefix("/jobs create ") {
        let created = create_job(cwd, request, JobSchedule::Immediate, false)?;
        launch_job_worker(cwd, created.id.as_str())?;
        writeln!(writer, "◆ Background job {} launched", created.id.as_str())?;
        return Ok(());
    }
    if let Some(arguments) = command.strip_prefix("/jobs schedule once ") {
        let (run_at_ms, request) = split_job_argument(arguments, "run_at_ms")?;
        let run_at_ms = run_at_ms
            .parse::<u64>()
            .context("run_at_ms must be an integer")?;
        let created = create_job(cwd, request, JobSchedule::Once { run_at_ms }, true)?;
        writeln!(writer, "◆ Scheduled one-shot job {}", created.id.as_str())?;
        return Ok(());
    }
    if let Some(arguments) = command.strip_prefix("/jobs schedule recurring ") {
        let (every_seconds, remaining) = split_job_argument(arguments, "every_seconds")?;
        let (next_run_at_ms, request) = split_job_argument(remaining, "next_run_at_ms")?;
        let schedule = JobSchedule::Recurring {
            every_seconds: every_seconds
                .parse::<u64>()
                .context("every_seconds must be an integer")?,
            next_run_at_ms: next_run_at_ms
                .parse::<u64>()
                .context("next_run_at_ms must be an integer")?,
        };
        let created = create_job(cwd, request, schedule, true)?;
        writeln!(writer, "◆ Scheduled recurring job {}", created.id.as_str())?;
        return Ok(());
    }
    let arguments = command.split_whitespace().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["list"] => show_job_list(&store, writer)?,
        ["inspect", id] => {
            serde_json::to_writer_pretty(&mut *writer, &store.inspect(id)?)?;
            writeln!(writer)?;
        }
        ["cancel", id] => {
            let job = store.request_cancel(id)?;
            writeln!(
                writer,
                "◆ Job {} is {}",
                job.id.as_str(),
                job_state_name(&job.state)
            )?;
        }
        ["run", id] => {
            launch_job_worker(cwd, id)?;
            writeln!(writer, "◆ Job worker launched for {id}")?;
        }
        ["tick"] => launch_due_jobs(cwd, writer)?,
        ["reattach", id] => reattach_job(&store, id, cwd, runtime, writer)?,
        _ => writeln!(
            writer,
            "usage: /jobs [list | inspect <id> | create <request> | schedule once <run_at_ms> <request> | schedule recurring <seconds> <next_ms> <request> | run <id> | cancel <id> | reattach <id> | tick]"
        )?,
    }
    Ok(())
}

fn create_job(
    cwd: &Path,
    request: &str,
    schedule: JobSchedule,
    scheduler_opt_in: bool,
) -> Result<crumb_agent::JobSummary> {
    JobStore::new(cwd.to_path_buf()).create(NewJob {
        request: request.to_owned(),
        config: read_agent_config(cwd)?,
        schedule,
        scheduler_opt_in,
    })
}

fn split_job_argument<'a>(input: &'a str, name: &str) -> Result<(&'a str, &'a str)> {
    let input = input.trim();
    let boundary = input
        .find(char::is_whitespace)
        .with_context(|| format!("missing {name} or job request"))?;
    let (value, remaining) = input.split_at(boundary);
    let remaining = remaining.trim();
    if remaining.is_empty() {
        return Err(anyhow!("job request cannot be empty"));
    }
    Ok((value, remaining))
}

fn show_job_list(store: &JobStore, writer: &mut dyn Write) -> Result<()> {
    let jobs = store.list()?;
    writeln!(writer, "◆ Local agent jobs")?;
    if jobs.is_empty() {
        writeln!(writer, "  No jobs configured")?;
    }
    for job in jobs.into_iter().take(20) {
        writeln!(
            writer,
            "  {}  {:<22}  {} bytes",
            job.id.as_str(),
            job_state_name(&job.state),
            job.request_bytes
        )?;
    }
    Ok(())
}

fn reattach_job(
    store: &JobStore,
    id: &str,
    cwd: &Path,
    runtime: &mut Option<AgentRuntime>,
    writer: &mut dyn Write,
) -> Result<()> {
    let job = store.inspect(id)?;
    if matches!(
        &job.state,
        JobState::Running { .. } | JobState::CancellationRequested { .. }
    ) {
        writeln!(
            writer,
            "◆ Job {id} is still active; its cancellation and limits remain attached"
        )?;
        return Ok(());
    }
    let session_id = job.session_id.context("job has no resumable session")?;
    if runtime.is_none() {
        *runtime = Some(AgentRuntime::new()?);
    }
    runtime
        .as_mut()
        .context("agent runtime is unavailable")?
        .resume(cwd, session_id.as_str())?;
    writeln!(
        writer,
        "◆ Reattached session {} from job {id}",
        session_id.as_str()
    )?;
    Ok(())
}

const fn job_state_name(state: &JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Running { .. } => "running",
        JobState::CancellationRequested { .. } => "cancelling",
        JobState::Completed { .. } => "completed",
        JobState::Failed { .. } => "failed",
        JobState::Cancelled { .. } => "cancelled",
    }
}

fn show_doctor(cwd: &Path, writer: &mut dyn Write) -> Result<()> {
    let config = read_agent_config(cwd)?;
    writeln!(writer, "◆ Agent backend")?;
    match &config.harness {
        Some(HarnessConfig::CodingCli {
            backend, command, ..
        }) => {
            let discovery = BackendDiscovery::discover(*backend, command);
            let route = config
                .models
                .get(&Modality::Text)
                .and_then(|routes| routes.first())
                .context("coding backend has no selected text model")?;
            let effort = config
                .reasoning_effort_for(route)
                .unwrap_or("provider default");
            writeln!(
                writer,
                "  {backend:?}  {}  {}/{} · effort {effort}",
                if discovery.is_available() {
                    "available"
                } else {
                    "missing"
                },
                route.provider,
                route.model
            )?;
            writeln!(
                writer,
                "  authentication is provider-managed and verified when a turn starts"
            )?;
        }
        Some(HarnessConfig::Process { command, .. }) => {
            writeln!(
                writer,
                "  DeepSeek Harness  configured  {}",
                command.display()
            )?;
        }
        Some(HarnessConfig::Native) => writeln!(writer, "  native Harness  unavailable")?,
        None => writeln!(writer, "  none configured · native shell remains available")?,
    }
    writeln!(writer, "◆ Token optimizers")?;
    if config.optimizers.is_empty() {
        writeln!(
            writer,
            "  none configured · native filtering remains active"
        )?;
        return Ok(());
    }
    let timeout = Duration::from_secs(2);
    for configured in &config.optimizers {
        let state = RtkOptimizer::from_config(configured, timeout).map_or("unsupported", |rtk| {
            if rtk.available() {
                "available"
            } else {
                "missing"
            }
        });
        writeln!(
            writer,
            "  {}  {}  {}",
            configured.id,
            if configured.enabled {
                "enabled "
            } else {
                "disabled"
            },
            state
        )?;
    }
    writeln!(
        writer,
        "  TOON requires a smaller, verified round trip; otherwise JSON wins"
    )?;
    Ok(())
}

fn show_reviews(
    command: &str,
    cwd: &Path,
    runtime: &mut Option<AgentRuntime>,
    writer: &mut dyn Write,
) -> Result<()> {
    let config = read_agent_config(cwd)?;
    let max_file_bytes = usize::try_from(config.limits.max_file_write_bytes)
        .context("max_file_write_bytes exceeds this platform's address space")?;
    let max_output_bytes = usize::try_from(config.limits.max_output_bytes)
        .context("max_output_bytes exceeds this platform's address space")?;
    let store = CheckpointStore::new(cwd, max_file_bytes)?;
    if let Some(comment) = command.strip_prefix("/review comment ") {
        return queue_review_comment(comment, runtime, &store, &config, writer);
    }
    let arguments = command.split_whitespace().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["list"] => show_review_list(&store, writer)?,
        ["diff", id] => {
            let checkpoint = store.load(id)?;
            show_review_summary(&checkpoint, writer)?;
            writeln!(writer, "{}", store.render_diff(id, max_output_bytes)?)?;
        }
        ["approve", "all"] => {
            let decided = store.decide_pending(CheckpointDecision::Approve)?;
            writeln!(writer, "◆ Approved {} pending checkpoints", decided.len())?;
        }
        ["approve", id] => {
            let checkpoint = store.decide(id, CheckpointDecision::Approve)?;
            writeln!(writer, "◆ Approved checkpoint {}", checkpoint.id)?;
        }
        ["reject" | "rewind", "all"] => {
            let decided = store.decide_pending(CheckpointDecision::Reject)?;
            writeln!(writer, "◆ Rewound {} pending checkpoints", decided.len())?;
        }
        ["reject" | "rewind", id] => {
            let checkpoint = store.decide(id, CheckpointDecision::Reject)?;
            writeln!(
                writer,
                "◆ Rewound {} from checkpoint {}",
                checkpoint.file.path.display(),
                checkpoint.id
            )?;
        }
        ["export", id] => {
            if *id == "all" {
                serde_json::to_writer_pretty(&mut *writer, &store.list()?)?;
            } else {
                serde_json::to_writer_pretty(&mut *writer, &store.load(id)?)?;
            }
            writeln!(writer)?;
        }
        _ => writeln!(
            writer,
            "usage: /review [list | diff <id> | approve <id|all> | reject <id|all> | comment <id> <feedback> | export <id|all>]"
        )?,
    }
    Ok(())
}

fn queue_review_comment(
    input: &str,
    runtime: &mut Option<AgentRuntime>,
    store: &CheckpointStore,
    config: &AgentConfig,
    writer: &mut dyn Write,
) -> Result<()> {
    let (id, comment) = input
        .trim()
        .split_once(char::is_whitespace)
        .context("usage: /review comment <id> <feedback>")?;
    let checkpoint = store.load(id)?;
    let max_messages = usize::try_from(config.limits.max_steering_messages)
        .context("max_steering_messages exceeds this platform's address space")?;
    let max_bytes = usize::try_from(config.limits.max_steering_bytes)
        .context("max_steering_bytes exceeds this platform's address space")?;
    if runtime.is_none() {
        *runtime = Some(AgentRuntime::new()?);
    }
    runtime
        .as_mut()
        .context("agent runtime is unavailable")?
        .queue_review_note(&checkpoint.id, comment, max_messages, max_bytes)?;
    writeln!(
        writer,
        "◆ Queued feedback for {} on the next agent turn",
        checkpoint.id
    )?;
    Ok(())
}

fn show_review_list(store: &CheckpointStore, writer: &mut dyn Write) -> Result<()> {
    let checkpoints = store.list()?;
    writeln!(writer, "◆ Crumb edit checkpoints")?;
    if checkpoints.is_empty() {
        writeln!(writer, "  No Crumb-owned edits to review")?;
    }
    for checkpoint in checkpoints.into_iter().take(20) {
        show_review_summary(&checkpoint, writer)?;
    }
    Ok(())
}

fn show_review_summary(
    checkpoint: &crumb_tools::WorkspaceCheckpoint,
    writer: &mut dyn Write,
) -> Result<()> {
    writeln!(
        writer,
        "  {}  {:<8}  {}  {} → {} bytes",
        checkpoint.id,
        checkpoint_status_name(checkpoint.file.status),
        checkpoint.file.path.display(),
        checkpoint.file.before_bytes,
        checkpoint.file.after_bytes
    )?;
    Ok(())
}

const fn checkpoint_status_name(status: CheckpointStatus) -> &'static str {
    match status {
        CheckpointStatus::Pending => "pending",
        CheckpointStatus::Approved => "approved",
        CheckpointStatus::Rejected => "rejected",
    }
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
        [] | ["list"] => show_session_list(&root, writer)?,
        ["inspect", id] => show_session_details(&root, id, writer)?,
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
        ["search", query @ ..] if !query.is_empty() => {
            show_session_search(&root, &query.join(" "), writer)?;
        }
        ["rename", id, label @ ..] if !label.is_empty() => {
            let label = label.join(" ");
            set_session_label(&root, id, &label)?;
            writeln!(writer, "◆ Session {id} labeled {label}")?;
        }
        ["archive" | "restore", id] => {
            let archived = arguments[0] == "archive";
            set_session_archived(&root, id, archived)?;
            writeln!(
                writer,
                "◆ Session {id} {}",
                if archived { "archived" } else { "restored" }
            )?;
        }
        ["export", id] => {
            let export = export_session(&root, id)?;
            serde_json::to_writer_pretty(&mut *writer, &export)?;
            writeln!(writer)?;
        }
        ["delete", id] => {
            if runtime.as_ref().and_then(AgentRuntime::active_session_id) == Some(*id) {
                return Err(anyhow!("cannot delete the active session"));
            }
            let destination = trash_session(&root, id)?;
            writeln!(writer, "◆ Session {id} moved to {}", destination.display())?;
        }
        _ => writeln!(
            writer,
            "usage: /session [list | inspect <id> | resume <id> | search <query> | rename <id> <label> | archive <id> | restore <id> | export <id> | delete <id>]"
        )?,
    }
    Ok(())
}

fn show_session_list(root: &Path, writer: &mut dyn Write) -> Result<()> {
    let sessions = list_sessions(root)?;
    writeln!(writer, "◆ Agent sessions")?;
    if sessions.is_empty() {
        writeln!(writer, "  No sessions in this workspace")?;
    }
    for summary in sessions.into_iter().take(20) {
        let label = summary
            .label
            .as_deref()
            .map_or(String::new(), |label| format!(" · {label}"));
        let archived = if summary.archived { " [archived]" } else { "" };
        writeln!(
            writer,
            "  {:<34} {:<10} {} turns{}{}",
            summary.id.as_str(),
            turn_status_name(summary.last_status),
            summary.turns,
            archived,
            label
        )?;
    }
    Ok(())
}

fn show_session_details(root: &Path, id: &str, writer: &mut dyn Write) -> Result<()> {
    let summary = session_summary(root, id)?;
    writeln!(writer, "◆ Session {}", summary.id.as_str())?;
    writeln!(
        writer,
        "  label      {}",
        summary.label.as_deref().unwrap_or("—")
    )?;
    writeln!(writer, "  archived   {}", summary.archived)?;
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
    Ok(())
}

fn show_session_search(root: &Path, query: &str, writer: &mut dyn Write) -> Result<()> {
    let sessions = search_sessions(root, query)?;
    writeln!(writer, "◆ Session search · {query}")?;
    if sessions.is_empty() {
        writeln!(writer, "  No matching sessions")?;
    }
    for summary in sessions.into_iter().take(20) {
        writeln!(
            writer,
            "  {}{}",
            summary.id.as_str(),
            summary
                .label
                .as_deref()
                .map_or(String::new(), |label| format!(" · {label}"))
        )?;
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
            if pollinations_environment_key().is_some() {
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
            let active = ["POLLINATIONS_API_KEY", "POLLINATIONS_KEY"]
                .into_iter()
                .filter(|name| std::env::var_os(name).is_some())
                .collect::<Vec<_>>();
            if !active.is_empty() {
                writeln!(
                    writer,
                    "{} remains active for this process",
                    active.join(" and ")
                )?;
            }
        }
    }
    Ok(())
}

fn pollinations_environment_key() -> Option<String> {
    ["POLLINATIONS_API_KEY", "POLLINATIONS_KEY"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
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
    for modifiers in [KeyModifiers::ALT, KeyModifiers::SHIFT] {
        keybindings.add_binding(
            modifiers,
            KeyCode::Enter,
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        );
    }
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('o'),
        ReedlineEvent::OpenEditor,
    );
    let terminal_width = suggestion_width(size()?.0);
    let menu = ColumnarMenu::default()
        .with_name("completion_menu")
        .with_columns(1)
        .with_column_width(Some(terminal_width));
    let editor = Reedline::create()
        .with_history(Box::new(interactive_history))
        .with_completer(Box::new(CrumbCompleter::new(workspace.clone())))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)));
    Ok(InteractiveLineEditor { editor, workspace })
}

fn suggestion_width(terminal_columns: u16) -> usize {
    usize::from(terminal_columns.saturating_sub(4).max(1))
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

    use crumb_agent::{
        AgentConfig, CredentialReference, ProviderHeader, ProviderProtocol, RiskClass,
    };

    use super::{
        agent_config_location, openrouter_preset, parse_credential_reference,
        parse_provider_header, suggestion_width, workspace_read_host,
    };

    #[test]
    fn openrouter_preset_contains_only_public_metadata_and_secret_references() {
        let provider = openrouter_preset();
        assert_eq!(provider.protocol, ProviderProtocol::OpenAiCompletions);
        assert_eq!(provider.base_url, "https://openrouter.ai/api/v1");
        assert!(matches!(
            provider.credential,
            Some(CredentialReference::Environment { ref name }) if name == "OPENROUTER_API_KEY"
        ));
        assert!(matches!(
            provider.headers.get("HTTP-Referer"),
            Some(ProviderHeader::Public { .. })
        ));
        assert!(provider.models.is_empty());
    }

    #[test]
    fn terminal_provider_values_require_typed_references() {
        assert!(parse_credential_reference("raw-secret").is_err());
        assert!(parse_provider_header("Bearer raw-secret").is_err());
        assert!(parse_credential_reference("env:CUSTOM_API_KEY").is_ok());
        assert!(parse_provider_header("public:Crumb").is_ok());
    }

    #[test]
    fn stdio_mcp_host_exposes_rust_owned_tool_risk() {
        let workspace = std::env::current_dir().expect("current directory is available");
        let host = workspace_read_host(&workspace, &AgentConfig::default(), None)
            .expect("workspace tools are registered");
        let tools = host.tools().collect::<Vec<_>>();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name, "list_directory");
        assert_eq!(tools[0].risk, RiskClass::ReadOnly);
        assert_eq!(tools[1].name, "read_file");
        assert_eq!(tools[1].risk, RiskClass::ReadOnly);
        assert_eq!(tools[2].name, "write_file");
        assert_eq!(tools[2].risk, RiskClass::WriteWorkspace);
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

    #[test]
    fn suggestion_width_never_exceeds_the_terminal() {
        for columns in [0, 1, 4, 5, 40, 240] {
            let width = suggestion_width(columns);
            assert!(width >= 1);
            assert!(width <= usize::from(columns.max(1)));
        }
    }
}
