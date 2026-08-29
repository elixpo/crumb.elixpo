//! Lightweight terminal presentation for crumb.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crumb_platform::Platform;

const FULL_LOGO: &str = r"   ██████╗██████╗ ██╗   ██╗███╗   ███╗██████╗
  ██╔════╝██╔══██╗██║   ██║████╗ ████║██╔══██╗
  ██║     ██████╔╝██║   ██║██╔████╔██║██████╔╝
  ██║     ██╔══██╗██║   ██║██║╚██╔╝██║██╔══██╗
  ╚██████╗██║  ██║╚██████╔╝██║ ╚═╝ ██║██████╔╝
   ╚═════╝╚═╝  ╚═╝ ╚═════╝ ╚═╝     ╚═╝╚═════╝";

/// Startup branding density.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrandingMode {
    Full,
    Compact,
    Disabled,
}

/// Presentation settings resolved once during startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiSettings {
    pub color: bool,
    pub plain: bool,
    pub branding: BrandingMode,
}

impl UiSettings {
    /// Resolves UI settings from terminal capability and environment flags.
    #[must_use]
    pub fn from_environment(is_terminal: bool) -> Self {
        let plain = !is_terminal || env_flag("CRUMB_PLAIN");
        let color = is_terminal && !plain && env::var_os("NO_COLOR").is_none();
        let branding = match env::var("CRUMB_BRANDING").as_deref() {
            Ok("full") => BrandingMode::Full,
            Ok("off" | "none" | "disabled") => BrandingMode::Disabled,
            _ => BrandingMode::Compact,
        };
        Self {
            color,
            plain,
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
        match self.settings.branding {
            BrandingMode::Full => self.paint(FULL_LOGO, "36"),
            BrandingMode::Compact => {
                self.paint("crumb • native shell, intelligently layered", "36")
            }
            BrandingMode::Disabled => String::new(),
        }
    }

    #[must_use]
    pub fn prompt(&self, context: &PromptContext<'_>) -> String {
        let cwd = display_path(context.cwd);
        if self.settings.plain {
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

    fn paint(&self, text: &str, color: &str) -> String {
        if self.settings.color {
            format!("\x1b[{color}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
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

    use super::{BrandingMode, GitSegment, PromptContext, Renderer, UiSettings};

    #[test]
    fn plain_prompt_is_deterministic() {
        let renderer = Renderer::new(UiSettings {
            color: false,
            plain: true,
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
            plain: false,
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
            plain: false,
            branding: BrandingMode::Full,
        });

        let branding = renderer.branding();

        assert!(branding.contains("██████╗"));
        assert_eq!(branding.lines().count(), 6);
    }
}
