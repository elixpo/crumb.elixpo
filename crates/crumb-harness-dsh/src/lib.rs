//! Optional `DeepSeek` Harness subprocess integration.
//!
//! This crate is outside Crumb's startup-critical and native-shell paths.

pub mod protocol;

pub use protocol::{
    IncomingFrame, InitializeParams, JsonRpcError, Notification, Response, SessionPromptParams,
    encode_initialize, encode_session_prompt, encode_shutdown,
};
