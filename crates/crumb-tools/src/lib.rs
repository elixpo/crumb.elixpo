//! Native tools whose permissions remain owned by Crumb.

mod shell;
mod workspace;

pub use shell::{AgentShellConfig, register_shell_tool};
pub use workspace::{WorkspaceToolLimits, register_workspace_read_tools};

fn bounded_text(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut boundary = limit;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text
}
