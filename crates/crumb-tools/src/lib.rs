//! Native tools whose permissions remain owned by Crumb.

mod workspace;

pub use workspace::{WorkspaceToolLimits, register_workspace_read_tools};
