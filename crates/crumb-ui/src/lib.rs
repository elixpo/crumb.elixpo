//! Lightweight terminal presentation for crumb.

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crumb_platform::Platform;

const WORDMARK: &str = r"   _____ ____  _   _ __  __ ____
  / ____|  _ \| | | |  \/  |  _ \
 | |    | |_) | | | | |\/| | |_) |
 | |____|  _ <| |_| | |  | |  _ <
  \_____|_| \_\ \___/|_|  |_|_| \_\";
const PANDA_AWAKE: &str = include_str!("../assets/panda-awake.txt");
const PANDA_COOL: &str = include_str!("../assets/panda-cool.txt");
const COOKIE_SPINNER: [&str; 4] = ["(.:)", "(:.)", "(o.)", "(.o)"];

const PUNCHLINES: &[&str] = &[
    "Broken biscuit. Working terminal.",
    "Crumbs included. Cleanup optional.",
    "Breaking biscuits, not workflows.",
    "Small crumb. Big terminal energy.",
    "Bash first. Snack later.",
    "Shell yeah. Crumb happens.",
    "Fresh from the command-line bakery.",
    "One shell. Several questionable snacks.",
    "Bite-sized commands. Full-stack appetite.",
    "Crunching tasks, not your patience.",
    "Natural language in. Terminal magic out.",
    "The shell is real. The jokes are optional-ish.",
    "Your command broke. The biscuit came that way.",
    "Less yak shaving. More biscuit breaking.",
    "Works offline. The jokes do too.",
    "AI optional. Personality unavoidable.",
    "No prompt prefix. No crumbs under the rug.",
    "Built with Rust. Seasoned with crumbs.",
    "404: boring terminal not found.",
    "Your shell called. It wants an agent.",
    "Command line, now with conversational crunch.",
    "The panda has entered the shell.",
    "Oreo approved this terminal. Probably.",
    "Oreo is debugging. Please hold the bamboo.",
    "Oreo says this probably needs a test.",
    "Oreo watches the permissions. Closely.",
    "Panda-powered. Developer-controlled.",
    "No cookies were accepted. One was broken.",
    "A little crumb goes a long way.",
    "Type boldly. Ctrl+C responsibly.",
];

/// Startup branding density.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrandingMode {
    Full,
    Compact,
    Disabled,
}

/// Output density and assistive-technology behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    Rich,
    Plain,
    ScreenReader,
}

/// Whether transient UI may animate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionMode {
    Full,
    Reduced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PandaMood {
    Awake,
    Cool,
}

/// Presentation settings resolved once during startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiSettings {
    pub color: bool,
    pub output: OutputMode,
    pub motion: MotionMode,
    pub branding: BrandingMode,
}

impl UiSettings {
    /// Resolves UI settings from terminal capability and environment flags.
    #[must_use]
    pub fn from_environment(is_terminal: bool) -> Self {
        let screen_reader = env_flag("CRUMB_SCREEN_READER");
        let plain = !is_terminal || screen_reader || env_flag("CRUMB_PLAIN");
        let color = is_terminal && !plain && env::var_os("NO_COLOR").is_none();
        let branding = match (
            is_terminal && !screen_reader,
            env::var("CRUMB_BRANDING").as_deref(),
        ) {
            (false, _) | (_, Ok("off" | "none" | "disabled")) => BrandingMode::Disabled,
            (_, Ok("compact")) => BrandingMode::Compact,
            _ => BrandingMode::Full,
        };
        Self {
            color,
            output: if screen_reader {
                OutputMode::ScreenReader
            } else if plain {
                OutputMode::Plain
            } else {
                OutputMode::Rich
            },
            motion: if screen_reader || env_flag("CRUMB_REDUCED_MOTION") {
                MotionMode::Reduced
            } else {
                MotionMode::Full
            },
            branding,
        }
    }
}

/// Git information shown in the prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSegment {
    pub branch: String,
    pub dirty: bool,
}

impl GitSegment {
    /// Reads local Git state without invoking a shell or accessing the network.
    #[must_use]
    pub fn discover(cwd: &Path) -> Option<Self> {
        let branch = git_output(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .or_else(|| git_output(cwd, &["rev-parse", "--short", "HEAD"]))?;
        let dirty = git_output(cwd, &["status", "--porcelain", "--untracked-files=no"])
            .is_some_and(|output| !output.is_empty());
        Some(Self { branch, dirty })
    }
}

/// State required to render one prompt.
#[derive(Clone, Debug)]
pub struct PromptContext<'a> {
    pub cwd: &'a Path,
    pub platform: Platform,
    pub git: Option<&'a GitSegment>,
    pub last_exit_code: Option<i32>,
    pub terminal_width: u16,
}

/// Non-secret state rendered once when the standalone terminal starts.
#[derive(Clone, Debug)]
pub struct StartupContext<'a> {
    pub version: &'a str,
    pub platform: Platform,
    pub model: Option<&'a str>,
    pub mode: &'a str,
    pub effort: Option<&'a str>,
    pub session_budget_tokens: u64,
    pub context_tokens: u64,
    pub auto_compaction: bool,
    pub agent_configured: bool,
}

/// Stateless terminal renderer.
#[derive(Clone, Copy, Debug)]
pub struct Renderer {
    settings: UiSettings,
}

impl Renderer {
    #[must_use]
    pub const fn new(settings: UiSettings) -> Self {
        Self { settings }
    }

    #[must_use]
    pub fn branding(&self) -> String {
        self.branding_for_width(120)
    }

    /// Renders the startup brand with a panda sized for the terminal width.
    #[must_use]
    pub fn branding_for_width(&self, terminal_width: u16) -> String {
        let punchline = startup_punchline();
        match self.settings.branding {
            BrandingMode::Full => format!(
                "{}\n  {}\n  {}",
                self.brand_art(terminal_width),
                self.paint(punchline, "1"),
                self.paint("Type naturally · : forces AI · /help opens commands", "2")
            ),
            BrandingMode::Compact => {
                format!(
                    "{}  {}",
                    self.paint("crumb", "36;1"),
                    self.paint(punchline, "2")
                )
            }
            BrandingMode::Disabled => String::new(),
        }
    }

    /// Renders the application-style ready screen used in full-screen mode.
    #[must_use]
    pub fn welcome(&self, context: &StartupContext<'_>, terminal_width: u16) -> String {
        let panda = panda_art(panda_mood(Some(context.agent_configured)));
        let model = context.model.unwrap_or("model not configured");
        let state = if context.agent_configured {
            format!("Ready · {model} · {}", context.mode)
        } else {
            "Native shell ready · run /auth login for AI".to_owned()
        };
        let copy = format!(
            "Crumb CLI v{} uses AI.\nNative commands stay native. Review AI actions.\n\n{}\nEffort · {}\nSession budget · {} tokens\nContext · {} / {}{}\n\nTip: type naturally · : forces AI · / opens actions",
            context.version,
            state,
            context.effort.unwrap_or("default"),
            compact_tokens(context.session_budget_tokens),
            compact_tokens(context.context_tokens),
            compact_tokens(context.session_budget_tokens),
            if context.auto_compaction {
                " · auto compact at 80%"
            } else {
                ""
            }
        );
        if terminal_width < 64 {
            return format!(
                "{}\n\n{}",
                self.paint_panda(trim_art(panda)),
                self.paint(&copy, "2")
            );
        }
        compose_welcome(*self, trim_art(panda), &copy)
    }

    /// Renders the interactive workspace trust boundary.
    #[must_use]
    pub fn folder_trust(
        &self,
        workspace: &Path,
        allow_selected: bool,
        terminal_width: u16,
    ) -> String {
        let yes = if allow_selected { "›" } else { " " };
        let no = if allow_selected { " " } else { "›" };
        let yes_line = format!("{yes} 1. Yes, trust this folder for this session");
        let no_line = format!("{no} 2. No (Esc)");
        let width = usize::from(terminal_width.clamp(56, 100));
        let inner = width.saturating_sub(4);
        let row = |text: &str| format!("│ {:<inner$} │", fit_terminal_text(text, inner));
        let yes_line = if allow_selected {
            self.paint(&row(&yes_line), "30;44;1")
        } else {
            row(&yes_line)
        };
        let no_line = if allow_selected {
            row(&no_line)
        } else {
            self.paint(&row(&no_line), "30;44;1")
        };
        [
            self.paint(&format!("╭{}╮", "─".repeat(width - 2)), "2"),
            self.paint(&row("Confirm folder trust"), "1"),
            self.paint(&row(""), "2"),
            self.paint(&row(&workspace.display().to_string()), "2"),
            self.paint(
                &row("Crumb may read, edit, and run commands here with your permission."),
                "2",
            ),
            self.paint(&row(""), "2"),
            yes_line,
            no_line,
            self.paint(&row(""), "2"),
            self.paint(&row("↑/↓ navigate · Enter select · Esc cancel"), "2"),
            self.paint(&format!("╰{}╯", "─".repeat(width - 2)), "2"),
        ]
        .join("\n")
    }

    /// Renders the persistent keyboard guide below the full-screen composer.
    #[must_use]
    pub fn composer_hotkeys(&self, terminal_width: u16) -> String {
        let guide = if terminal_width < 80 {
            "Tab complete · @ context · / commands · Ctrl+C cancel"
        } else {
            "←/→ move · ↑ history · Tab complete · @ add context · / commands · Ctrl+C cancel"
        };
        self.paint(guide, "2")
    }

    /// Renders a compact, non-blocking startup readiness summary.
    #[must_use]
    pub fn startup_status(&self, context: &StartupContext<'_>) -> String {
        let model = context.model.unwrap_or("not configured");
        let state = if context.agent_configured {
            "agent ready"
        } else {
            "native shell ready · agent setup needed"
        };
        if self.settings.output == OutputMode::ScreenReader {
            return format!(
                "crumb {} on {}\n{state}; model {model}; mode {}",
                context.version, context.platform, context.mode
            );
        }
        format!(
            "{}  {}\n{}  {}",
            self.paint(" SESSION ", "30;46;1"),
            self.paint(
                &format!("crumb {} · {}", context.version, context.platform),
                "2"
            ),
            self.paint(
                if context.agent_configured {
                    "●"
                } else {
                    "○"
                },
                "36;1"
            ),
            self.paint(&format!("{state} · {model} · {}", context.mode), "2")
        )
    }

    /// Renders the stable context shown before a Harness turn begins.
    #[must_use]
    pub fn agent_header(
        &self,
        model: &str,
        effort: Option<&str>,
        mode: &str,
        skill: Option<&str>,
    ) -> String {
        let mut details = vec![format!("model {model}"), format!("mode {mode}")];
        if let Some(effort) = effort {
            details.push(format!("effort {effort}"));
        }
        if let Some(skill) = skill {
            details.push(format!("skill {skill}"));
        }
        if self.settings.output == OutputMode::ScreenReader {
            format!("Crumb agent\n{}", details.join(", "))
        } else {
            format!(
                "{} {}  {}",
                self.paint("◆", "35;1"),
                self.paint("Crumb agent", "1"),
                self.paint(&details.join(" · "), "2")
            )
        }
    }

    /// Renders one committed Harness response without protocol metadata.
    #[must_use]
    pub fn agent_response(response: &str) -> String {
        visible_agent_text(response)
    }

    /// Renders a Harness failure without conflating it with native shell output.
    #[must_use]
    pub fn agent_error(&self, message: &str, cancelled: bool) -> String {
        let (marker, title) = if cancelled {
            ("■", "Agent cancelled")
        } else {
            ("!", "Agent unavailable")
        };
        if self.settings.output == OutputMode::ScreenReader {
            return format!("{title}: {message}");
        }
        format!(
            "{} {}\n  {}",
            self.paint(marker, if cancelled { "33;1" } else { "31;1" }),
            self.paint(title, "1"),
            self.paint(message, "2")
        )
    }

    /// Renders one transient Harness activity update.
    #[must_use]
    pub fn agent_activity(&self, label: &str) -> String {
        if self.settings.output == OutputMode::ScreenReader {
            format!("Activity: {label}")
        } else {
            format!("  ↳ {label}")
        }
    }

    /// Starts a single-line terminal activity animation.
    #[must_use]
    pub fn activity(&self, label: &str) -> ActivityIndicator {
        ActivityIndicator::start(
            label,
            self.settings.output == OutputMode::Rich && self.settings.motion == MotionMode::Full,
            self.settings.color,
        )
    }

    #[must_use]
    pub fn prompt(&self, context: &PromptContext<'_>) -> String {
        let cwd = display_path(context.cwd);
        if self.settings.output != OutputMode::Rich {
            let mut segments = vec![format!("crumb {cwd}"), context.platform.to_string()];
            append_optional_segments(&mut segments, context);
            return format!("{}\n> ", segments.join(" | "));
        }

        let mut context_line = self.paint(&cwd, "34;1");
        if let Some(git) = context.git {
            let dirty = if git.dirty { " *" } else { "" };
            context_line.push_str(&self.paint(&format!("  git:{}{dirty}", git.branch), "2"));
        }
        if let Some(exit_code) = context.last_exit_code
            && exit_code != 0
        {
            context_line.push_str(&self.paint(&format!("  exit:{exit_code}"), "31"));
        }
        let rule_width = usize::from(context.terminal_width.saturating_sub(2).clamp(20, 160));
        format!(
            "{context_line}\n{}\n{} ",
            self.paint(&format!("┌{}", "─".repeat(rule_width)), "2"),
            self.paint("│", "36;1")
        )
    }

    fn paint(self, text: &str, color: &str) -> String {
        if self.settings.color {
            format!("\x1b[{color}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn brand_art(self, terminal_width: u16) -> String {
        let panda = panda_art(panda_mood(None));
        if terminal_width < 72 {
            return format!(
                "{}\n{}",
                self.paint("CRUMB", "36;1"),
                self.paint_panda(trim_art(panda))
            );
        }
        compose_styled_art(self, WORDMARK, trim_art(panda), 4)
    }

    fn paint_panda(self, panda: &str) -> String {
        panda
            .lines()
            .map(|line| paint_panda_line(self, line))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn panda_mood(agent_configured: Option<bool>) -> PandaMood {
    match env::var("CRUMB_MOOD").as_deref() {
        Ok("awake") => PandaMood::Awake,
        Ok("cool") => PandaMood::Cool,
        _ if agent_configured == Some(true) => PandaMood::Cool,
        _ => PandaMood::Awake,
    }
}

const fn panda_art(mood: PandaMood) -> &'static str {
    match mood {
        PandaMood::Awake => PANDA_AWAKE,
        PandaMood::Cool => PANDA_COOL,
    }
}

fn paint_panda_line(renderer: Renderer, line: &str) -> String {
    let mut painted = String::new();
    for character in line.chars() {
        let color = match character {
            '╭' | '╮' | '╯' | '╰' | '─' | '│' => "38;2;255;253;241",
            '█' | '═' => "38;2;96;80;122",
            '▛' | '▀' | '▜' => "38;2;151;119;188",
            '▌' | '▐' => "38;2;119;93;154",
            '▙' | '▄' | '▟' => "38;2;76;61;99",
            '●' => "38;2;157;128;193;1",
            '░' => "38;2;255;190;195",
            '▒' => "38;2;255;125;139",
            '▓' => "38;2;255;88;110",
            '▆' | '▃' | '▂' => "38;2;255;154;171",
            _ => {
                painted.push(character);
                continue;
            }
        };
        painted.push_str(&renderer.paint(&character.to_string(), color));
    }
    painted
}

fn trim_art(art: &str) -> &str {
    art.trim()
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}.{}m", tokens / 1_000_000, tokens % 1_000_000 / 100_000)
    } else if tokens >= 1_000 {
        format!("{}.{}k", tokens / 1_000, tokens % 1_000 / 100)
    } else {
        tokens.to_string()
    }
}

fn fit_terminal_text(text: &str, width: usize) -> String {
    let mut fitted = text.chars().take(width).collect::<String>();
    if text.chars().count() > width && width > 1 {
        fitted.pop();
        fitted.push('…');
    }
    fitted
}

fn compose_styled_art(renderer: Renderer, left: &str, right: &str, gap: usize) -> String {
    let left = left.lines().collect::<Vec<_>>();
    let right = right.lines().collect::<Vec<_>>();
    let height = left.len().max(right.len());
    let left_width = left
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let left_offset = height.saturating_sub(left.len()) / 2;
    let right_offset = height.saturating_sub(right.len()) / 2;
    (0..height)
        .map(|row| {
            let left_line = row
                .checked_sub(left_offset)
                .and_then(|index| left.get(index))
                .copied()
                .unwrap_or("");
            let right_line = row
                .checked_sub(right_offset)
                .and_then(|index| right.get(index))
                .copied()
                .unwrap_or("");
            format!(
                "{}{}{}",
                renderer.paint(&format!("{left_line:<left_width$}"), "36"),
                " ".repeat(gap),
                renderer.paint_panda(right_line)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compose_welcome(renderer: Renderer, panda: &str, copy: &str) -> String {
    let panda = panda.lines().collect::<Vec<_>>();
    let copy = copy.lines().collect::<Vec<_>>();
    let height = panda.len().max(copy.len());
    let panda_width = panda
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    (0..height)
        .map(|row| {
            let panda_line = panda.get(row).copied().unwrap_or("");
            let copy_line = copy.get(row).copied().unwrap_or("");
            format!(
                "{}   {}",
                renderer.paint_panda(&format!("{panda_line:<panda_width$}")),
                renderer.paint(copy_line, if row == 0 { "1" } else { "2" })
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn startup_punchline() -> &'static str {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let process = u128::from(std::process::id());
    let width = u128::try_from(PUNCHLINES.len()).unwrap_or(1);
    let index = usize::try_from((elapsed.as_nanos() ^ process) % width).unwrap_or(0);
    PUNCHLINES[index]
}

/// A best-effort activity line that always clears itself before final output.
pub struct ActivityIndicator {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    started: Instant,
    visible: bool,
    color: bool,
}

impl ActivityIndicator {
    fn start(label: &str, animated: bool, color: bool) -> Self {
        let running = Arc::new(AtomicBool::new(animated));
        let thread = animated.then(|| {
            let running = Arc::clone(&running);
            let label = label.to_owned();
            thread::spawn(move || animate_activity(&label, color, &running))
        });
        Self {
            running,
            thread,
            started: Instant::now(),
            visible: animated,
            color,
        }
    }

    /// Stops and clears the activity line before another message is rendered.
    pub fn finish(mut self) {
        self.stop();
    }

    /// Stops the animation and leaves one compact completion crumb.
    pub fn complete(mut self) {
        self.stop();
        if self.visible {
            let elapsed = self.started.elapsed().as_secs_f32();
            let summary = if self.color {
                format!("\x1b[90m. Worked for {elapsed:.1}s\x1b[0m")
            } else {
                format!(". Worked for {elapsed:.1}s")
            };
            let _ = writeln!(io::stderr(), "{summary}");
            let _ = io::stderr().flush();
        }
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
            clear_activity_line();
        }
    }
}

impl Drop for ActivityIndicator {
    fn drop(&mut self) {
        self.stop();
    }
}

fn animate_activity(label: &str, color: bool, running: &AtomicBool) {
    let mut frame = 0_usize;
    while running.load(Ordering::Acquire) {
        let cookie = COOKIE_SPINNER[frame];
        if color {
            let _ = write!(
                io::stderr(),
                "\r\x1b[2K\x1b[35;1m{cookie}\x1b[0m \x1b[90m{label} · type to steer · Ctrl+C to cancel\x1b[0m"
            );
        } else {
            let _ = write!(
                io::stderr(),
                "\r\x1b[2K{cookie} {label} · type to steer · Ctrl+C to cancel"
            );
        }
        let _ = io::stderr().flush();
        frame = (frame + 1) % COOKIE_SPINNER.len();
        thread::sleep(Duration::from_millis(140));
    }
}

fn clear_activity_line() {
    let _ = write!(io::stderr(), "\r\x1b[2K");
    let _ = io::stderr().flush();
}

/// Removes provider reasoning wrappers from text intended for the terminal.
#[must_use]
pub fn visible_agent_text(response: &str) -> String {
    let mut visible = response.to_owned();
    for (opening, closing) in [("<thinking>", "</thinking>"), ("<think>", "</think>")] {
        while let Some(start) = visible.find(opening) {
            let tail = &visible[start + opening.len()..];
            let end = tail.find(closing).map_or(visible.len(), |offset| {
                start + opening.len() + offset + closing.len()
            });
            visible.replace_range(start..end, "");
        }
    }
    let visible = visible.trim();
    visible
        .strip_prefix("<response>")
        .and_then(|body| body.strip_suffix("</response>"))
        .map_or_else(|| visible.to_owned(), |body| body.trim().to_owned())
}

fn append_optional_segments(segments: &mut Vec<String>, context: &PromptContext<'_>) {
    if let Some(git) = context.git {
        let dirty = if git.dirty { " *" } else { "" };
        segments.push(format!("git:{}{dirty}", git.branch));
    }
    if let Some(exit_code) = context.last_exit_code
        && exit_code != 0
    {
        segments.push(format!("exit:{exit_code}"));
    }
}

fn display_path(path: &Path) -> String {
    let home = env::var_os(home_variable());
    if let Some(home) = home.map(PathBuf::from)
        && let Ok(relative) = path.strip_prefix(&home)
    {
        return if relative.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", relative.display())
        };
    }
    path.display().to_string()
}

fn home_variable() -> &'static str {
    if cfg!(windows) { "USERPROFILE" } else { "HOME" }
}

fn env_flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| !matches!(value.as_str(), "" | "0" | "false" | "no"))
}

fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crumb_platform::Platform;

    use super::{
        BrandingMode, COOKIE_SPINNER, GitSegment, MotionMode, OutputMode, PUNCHLINES,
        PromptContext, Renderer, StartupContext, UiSettings,
    };

    #[test]
    fn plain_prompt_is_deterministic() {
        let renderer = Renderer::new(UiSettings {
            color: false,
            output: OutputMode::Plain,
            motion: MotionMode::Reduced,
            branding: BrandingMode::Disabled,
        });
        let git = GitSegment {
            branch: "main".to_owned(),
            dirty: true,
        };

        let prompt = renderer.prompt(&PromptContext {
            cwd: Path::new("/workspace/crumb"),
            platform: Platform::Linux,
            git: Some(&git),
            last_exit_code: Some(2),
            terminal_width: 80,
        });

        assert_eq!(
            prompt,
            "crumb /workspace/crumb | linux | git:main * | exit:2\n> "
        );
    }

    #[test]
    fn rich_prompt_uses_two_lines_without_color_when_disabled() {
        let renderer = Renderer::new(UiSettings {
            color: false,
            output: OutputMode::Rich,
            motion: MotionMode::Full,
            branding: BrandingMode::Compact,
        });

        let prompt = renderer.prompt(&PromptContext {
            cwd: Path::new("/workspace"),
            platform: Platform::MacOs,
            git: None,
            last_exit_code: Some(0),
            terminal_width: 80,
        });

        assert!(prompt.starts_with("/workspace\n┌"));
        assert!(prompt.ends_with("\n│ "));
        assert!(!prompt.contains("exit:"));
    }

    #[test]
    fn full_branding_contains_the_product_name_shape() {
        let renderer = Renderer::new(UiSettings {
            color: false,
            output: OutputMode::Rich,
            motion: MotionMode::Full,
            branding: BrandingMode::Full,
        });

        let branding = renderer.branding_for_width(120);
        let narrow = renderer.branding_for_width(60);
        let wide = renderer.branding_for_width(180);

        assert!(branding.contains("_____ ____"));
        assert!(
            PUNCHLINES
                .iter()
                .any(|punchline| branding.contains(punchline))
        );
        assert!(narrow.contains("CRUMB"));
        assert!(narrow.contains("╭▄█▄╮"));
        assert!(wide.contains("╭▄█▄╮"));
    }

    #[test]
    fn agent_output_keeps_response_and_metadata_distinct() {
        let renderer = Renderer::new(UiSettings {
            color: false,
            output: OutputMode::Plain,
            motion: MotionMode::Reduced,
            branding: BrandingMode::Disabled,
        });

        assert_eq!(
            renderer.agent_header("qwen-coder", Some("high"), "auto", None),
            "◆ Crumb agent  model qwen-coder · mode auto · effort high"
        );
        assert_eq!(Renderer::agent_response("done\n"), "done");
    }

    #[test]
    fn startup_status_contains_only_non_secret_readiness_metadata() {
        let renderer = Renderer::new(UiSettings {
            color: false,
            output: OutputMode::Plain,
            motion: MotionMode::Reduced,
            branding: BrandingMode::Compact,
        });
        let status = renderer.startup_status(&StartupContext {
            version: "0.1.0",
            platform: Platform::Linux,
            model: Some("pollinations/nova-fast"),
            mode: "auto",
            effort: Some("medium"),
            session_budget_tokens: 64_000,
            context_tokens: 0,
            auto_compaction: true,
            agent_configured: true,
        });

        assert_eq!(
            status,
            " SESSION   crumb 0.1.0 · linux\n●  agent ready · pollinations/nova-fast · auto"
        );
        let welcome = renderer.welcome(
            &StartupContext {
                version: "0.1.0",
                platform: Platform::Linux,
                model: Some("pollinations/nova-fast"),
                mode: "auto",
                effort: Some("medium"),
                session_budget_tokens: 64_000,
                context_tokens: 0,
                auto_compaction: true,
                agent_configured: true,
            },
            120,
        );
        assert!(welcome.contains("Crumb CLI v0.1.0 uses AI."));
        assert!(welcome.contains("Ready · pollinations/nova-fast · auto"));
        assert!(welcome.contains("╭▄█▄╮"));
    }

    #[test]
    fn folder_trust_keeps_the_workspace_and_selection_visible() {
        let renderer = Renderer::new(UiSettings {
            color: false,
            output: OutputMode::Rich,
            motion: MotionMode::Reduced,
            branding: BrandingMode::Full,
        });
        let dialog = renderer.folder_trust(Path::new("/workspace/crumb"), true, 80);

        assert!(dialog.contains("Confirm folder trust"));
        assert!(dialog.contains("/workspace/crumb"));
        assert!(dialog.contains("› 1. Yes"));
        assert!(dialog.contains("2. No (Esc)"));
    }

    #[test]
    fn hidden_reasoning_is_not_rendered() {
        assert_eq!(
            super::visible_agent_text("<thinking>private chain</thinking>\nanswer"),
            "answer"
        );
        assert_eq!(
            super::visible_agent_text("answer<think>unfinished"),
            "answer"
        );
        assert_eq!(
            super::visible_agent_text("<response>\nexact answer\n</response>"),
            "exact answer"
        );
    }

    #[test]
    fn cookie_spinner_is_compact_ascii_art() {
        assert!(COOKIE_SPINNER.iter().all(|frame| frame.is_ascii()));
        assert!(COOKIE_SPINNER.iter().all(|frame| frame.len() == 4));
        assert!(COOKIE_SPINNER.iter().all(|frame| frame.starts_with('(')));
        assert!(COOKIE_SPINNER.iter().all(|frame| frame.ends_with(')')));
    }

    #[test]
    fn screen_reader_output_avoids_decorative_symbols() {
        let renderer = Renderer::new(UiSettings {
            color: false,
            output: OutputMode::ScreenReader,
            motion: MotionMode::Reduced,
            branding: BrandingMode::Disabled,
        });

        assert_eq!(
            renderer.agent_header("qwen-coder", None, "plan", None),
            "Crumb agent\nmodel qwen-coder, mode plan"
        );
        assert_eq!(
            renderer.agent_error("stopped", true),
            "Agent cancelled: stopped"
        );
        assert!(renderer.branding().is_empty());
    }
}
