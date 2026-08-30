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

const FULL_LOGO: &str = r"   ██████╗██████╗ ██╗   ██╗███╗   ███╗██████╗
  ██╔════╝██╔══██╗██║   ██║████╗ ████║██╔══██╗
  ██║     ██████╔╝██║   ██║██╔████╔██║██████╔╝
  ██║     ██╔══██╗██║   ██║██║╚██╔╝██║██╔══██╗
  ╚██████╗██║  ██║╚██████╔╝██║ ╚═╝ ██║██████╔╝
   ╚═════╝╚═╝  ╚═╝ ╚═════╝ ╚═╝     ╚═╝╚═════╝";
const COOKIE_ART: &str = include_str!("../assets/cookie.txt");

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
        let punchline = startup_punchline();
        match self.settings.branding {
            BrandingMode::Full => format!(
                "{}\n  {}  {}",
                self.paint(FULL_LOGO, "36"),
                self.paint(punchline, "1"),
                self.paint("Type /help or press Tab after / and @", "2")
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
                "{} {}\n  {}",
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

        let mut segments = vec![
            self.paint("crumb", "36"),
            self.paint(&cwd, "34"),
            context.platform.to_string(),
        ];
        append_optional_segments(&mut segments, context);
        format!("╭─[ {} ]\n╰─❯ ", segments.join(" ]─[ "))
    }

    fn paint(self, text: &str, color: &str) -> String {
        if self.settings.color {
            format!("\x1b[{color}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
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
    line_count: usize,
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
            line_count: COOKIE_ART.lines().count() + 1,
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
            let crumb = if self.color {
                "\x1b[35;1m.\x1b[0m"
            } else {
                "."
            };
            let _ = writeln!(io::stderr(), "{crumb} Worked for {elapsed:.1}s");
            let _ = io::stderr().flush();
        }
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
            clear_activity_lines(self.line_count);
        }
    }
}

impl Drop for ActivityIndicator {
    fn drop(&mut self) {
        self.stop();
    }
}

fn animate_activity(label: &str, color: bool, running: &AtomicBool) {
    let mirrored = mirror_cookie(COOKIE_ART);
    let frames = [
        (COOKIE_ART, 0_usize),
        (COOKIE_ART, 1),
        (mirrored.as_str(), 2),
        (mirrored.as_str(), 1),
    ];
    let mut frame = 0_usize;
    let mut drawn = false;
    while running.load(Ordering::Acquire) {
        if drawn {
            clear_activity_lines(COOKIE_ART.lines().count() + 1);
        }
        draw_activity_frame(frames[frame].0, frames[frame].1, label, color);
        let _ = io::stderr().flush();
        drawn = true;
        frame = (frame + 1) % frames.len();
        thread::sleep(Duration::from_millis(220));
    }
}

fn draw_activity_frame(art: &str, offset: usize, label: &str, color: bool) {
    let indent = " ".repeat(offset);
    for line in art.lines() {
        if color {
            let _ = write!(io::stderr(), "\r\x1b[2K{indent}\x1b[35m{line}\x1b[0m\r\n");
        } else {
            let _ = write!(io::stderr(), "\r\x1b[2K{indent}{line}\r\n");
        }
    }
    let _ = write!(
        io::stderr(),
        "\r\x1b[2K{indent}{label}  type to steer · Ctrl+C to cancel"
    );
}

fn mirror_cookie(art: &str) -> String {
    art.lines()
        .map(|line| line.chars().rev().map(mirror_braille).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn mirror_braille(character: char) -> char {
    let code = u32::from(character);
    if !(0x2800..=0x28ff).contains(&code) {
        return character;
    }
    let dots = code - 0x2800;
    let mirrored = ((dots & 0b0000_0001) << 3)
        | ((dots & 0b0000_0010) << 3)
        | ((dots & 0b0000_0100) << 3)
        | ((dots & 0b0000_1000) >> 3)
        | ((dots & 0b0001_0000) >> 3)
        | ((dots & 0b0010_0000) >> 3)
        | ((dots & 0b0100_0000) << 1)
        | ((dots & 0b1000_0000) >> 1);
    char::from_u32(0x2800 + mirrored).unwrap_or(character)
}

fn clear_activity_lines(line_count: usize) {
    for line in 0..line_count {
        let _ = write!(io::stderr(), "\r\x1b[2K");
        if line + 1 < line_count {
            let _ = write!(io::stderr(), "\x1b[1A");
        }
    }
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
        BrandingMode, COOKIE_ART, GitSegment, MotionMode, OutputMode, PUNCHLINES, PromptContext,
        Renderer, UiSettings, mirror_cookie,
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
        });

        assert_eq!(prompt, "╭─[ crumb ]─[ /workspace ]─[ macos ]\n╰─❯ ");
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

        let branding = renderer.branding();

        assert!(branding.contains("██████╗"));
        assert!(
            PUNCHLINES
                .iter()
                .any(|punchline| branding.contains(punchline))
        );
        assert_eq!(branding.lines().count(), 7);
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
            "◆ Crumb agent\n  model qwen-coder · mode auto · effort high"
        );
        assert_eq!(Renderer::agent_response("done\n"), "done");
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
    fn cookie_activity_uses_the_asset_and_mirrors_cleanly() {
        let mirrored = mirror_cookie(COOKIE_ART);

        assert_eq!(COOKIE_ART.lines().count(), 15);
        assert_ne!(mirrored, COOKIE_ART.trim_end());
        assert_eq!(mirror_cookie(&mirrored), COOKIE_ART.trim_end());
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
