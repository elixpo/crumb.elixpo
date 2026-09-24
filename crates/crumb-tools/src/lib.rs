//! Native tools whose permissions remain owned by Crumb.

mod checkpoint;
mod shell;
mod stack_detection;
mod workspace;

pub use checkpoint::{
    CheckpointDecision, CheckpointFile, CheckpointStatus, CheckpointStore, WorkspaceCheckpoint,
};
pub use shell::{AgentShellConfig, register_shell_tool, register_shell_tool_with_optimizer};
pub use stack_detection::{
    DetectedStack, LanguageDetection, StackConfidence, StackDetectionLimits,
    register_stack_detection_tool,
};
pub use workspace::{
    WorkspaceToolLimits, WorkspaceWriteLimits, register_workspace_read_tools,
    register_workspace_write_tool,
};

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
